//! Native platform extraction. Hosts receive one media contract regardless of site.
use anyhow::{bail, ensure, Context, Result};
use bbdown_core::{BiliClient, ClientConfig, MediaRequestSpec, PlaybackPlan, Selection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::PathBuf, sync::Mutex, time::Duration};
use tokio::sync::Semaphore;
use ytdown::{Container, Format, MediaInfo, Ytdown};

const CACHE_TTL_MS: u64 = 15 * 60 * 1000;
const MAX_CACHED: usize = 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaStream {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub size: Option<u64>,
    #[serde(default)]
    pub decrypt_key: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assembly {
    Direct,
    Merge,
    Concat,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaPlan {
    pub source_url: String,
    pub source_id: String,
    pub platform: String,
    pub title: String,
    pub format_id: String,
    pub label: String,
    pub extension: String,
    pub assembly: Assembly,
    pub streams: Vec<MediaStream>,
    pub extracted_at: u64,
}

impl MediaPlan {
    pub fn total_size(&self) -> Option<u64> {
        self.streams
            .iter()
            .try_fold(0_u64, |sum, stream| sum.checked_add(stream.size?))
    }

    pub fn filename(&self) -> String {
        media_filename(&self.title, &self.extension)
    }

    pub fn for_display(&mut self) {
        // Signed CDN URLs and request headers stay in the private task store.
        self.streams.clear();
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.streams.is_empty() && self.streams.len() <= 128,
            "视频没有可下载的媒体轨道"
        );
        ensure!(
            matches!(
                self.extension.as_str(),
                "mp4" | "webm" | "mkv" | "m4a" | "mp3" | "ogg" | "flv"
            ),
            "不支持的视频容器"
        );
        ensure!(
            self.assembly != Assembly::Direct || self.streams.len() == 1,
            "单文件格式包含多个地址"
        );
        ensure!(
            self.assembly != Assembly::Merge || self.streams.len() == 2,
            "音视频合并需要两个轨道"
        );
        for stream in &self.streams {
            let url = crate::filename::parse_url(&stream.url)?;
            ensure!(
                !is_manifest(&url, None),
                "此格式是流媒体清单，当前版本只下载完整媒体文件"
            );
            crate::download::validate_headers(&stream.headers)?;
            ensure!(
                stream.decrypt_key.is_none()
                    || (self.platform == "WeChat Channels" && self.assembly == Assembly::Direct),
                "加密媒体只能使用视频号单文件格式"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaFormat {
    pub id: String,
    pub label: String,
    pub filename: String,
    pub extension: String,
    pub total_bytes: Option<u64>,
    pub needs_merge: bool,
    pub audio_only: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaPreview {
    pub id: String,
    pub title: String,
    pub platform: String,
    pub duration_seconds: Option<u64>,
    pub formats: Vec<MediaFormat>,
    pub ffmpeg_available: bool,
    pub expires_at: u64,
}

struct Resolved {
    title: String,
    duration: Option<u64>,
    // Height is used only for presentation order. Audio-only options come last.
    plans: Vec<(u32, bool, MediaPlan)>,
}

struct Cached {
    expires_at: u64,
    plans: Vec<MediaPlan>,
}

pub struct MediaResolver {
    youtube: crate::youtube::YoutubeResolver,
    other_platforms: Ytdown,
    probe: crate::youtube::MediaProbe,
    bilibili: BiliClient,
    chinese: crate::chinese_video::ChineseResolver,
    cache: Mutex<HashMap<String, Cached>>,
    slots: Semaphore,
    pub ffmpeg: Option<PathBuf>,
}

impl MediaResolver {
    pub fn new() -> Result<Self> {
        Self::with_ffmpeg(find_ffmpeg())
    }

    pub fn with_ffmpeg(ffmpeg: Option<PathBuf>) -> Result<Self> {
        let client = reqwest_media::Client::builder()
            .use_native_tls()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15))
            .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36")
            .build()?;
        let probe = crate::youtube::MediaProbe::new()?;
        Ok(Self {
            youtube: crate::youtube::YoutubeResolver::new(client.clone(), probe.clone()),
            other_platforms: Ytdown::builder().client(client).build()?,
            probe,
            bilibili: BiliClient::new(
                ClientConfig::default().with_request_timeout(Duration::from_secs(15)),
            ),
            chinese: crate::chinese_video::ChineseResolver::new()?,
            cache: Mutex::new(HashMap::new()),
            slots: Semaphore::new(2),
            ffmpeg,
        })
    }

    pub async fn resolve(&self, raw: &str) -> Result<MediaPreview> {
        self.resolve_with_cookie(raw, None).await
    }

    pub async fn resolve_with_cookie(
        &self,
        raw: &str,
        platform_cookie: Option<&str>,
    ) -> Result<MediaPreview> {
        let url = crate::filename::parse_url(raw)?;
        let platform = platform(url.as_str()).context(
            "暂不支持这个视频平台，请使用 YouTube、B 站、抖音、小红书、视频号等单个视频链接",
        )?;
        let _permit = self
            .slots
            .try_acquire()
            .context("已有两个视频正在解析，请稍后再试")?;
        let work = async {
            if matches!(platform, "Douyin" | "Xiaohongshu" | "WeChat Channels") {
                let video = self
                    .chinese
                    .resolve(&url, platform, platform_cookie)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "{}解析失败：{}",
                            platform,
                            safe_error(&format!("{error:#}"))
                        )
                    })?;
                normalize_native(url.as_str(), platform, video)
            } else if platform == "Bilibili" {
                let plan = self
                    .bilibili
                    .plan_playback(url.as_str(), Some(Selection::Current))
                    .await
                    .map_err(|e| anyhow::anyhow!("B 站解析失败：{}", safe_error(&e.to_string())))?;
                normalize_bilibili(url.as_str(), plan)
            } else {
                let info = if platform == "YouTube" {
                    self.youtube.resolve(&url).await
                } else {
                    self.other_platforms.resolve(url.as_str()).await
                }
                .map_err(ytdown_error)?;
                match info {
                    MediaInfo::Single(info) => {
                        ensure!(!info.is_live, "当前版本暂不支持直播录制");
                        let mut resolved = normalize_ytdown(
                            url.as_str(),
                            platform,
                            &info.id,
                            &info.title,
                            info.duration.map(|d| d.as_secs()),
                            &info.formats,
                        )?;
                        if platform == "YouTube" {
                            use futures_util::{stream, StreamExt};
                            let checked: Vec<_> =
                                stream::iter(resolved.plans.into_iter().map(|plan| async {
                                    for media in &plan.2.streams {
                                        if self.probe.check(media).await.is_err() {
                                            return None;
                                        }
                                    }
                                    Some(plan)
                                }))
                                .buffered(4)
                                .collect()
                                .await;
                            resolved.plans = checked.into_iter().flatten().collect();
                        }
                        Ok(resolved)
                    }
                    MediaInfo::Collection(_) => {
                        bail!("请粘贴单个视频链接，当前版本暂不批量下载播放列表或频道")
                    }
                }
            }
        };
        let resolved = tokio::time::timeout(Duration::from_secs(50), work)
            .await
            .context("视频解析超时，请检查网络后重试")??;
        self.cache_resolved(platform, resolved)
    }

    fn cache_resolved(&self, platform: &str, mut resolved: Resolved) -> Result<MediaPreview> {
        resolved
            .plans
            .retain(|(_, _, plan)| plan.validate().is_ok());
        ensure!(
            !resolved.plans.is_empty(),
            "没有找到可下载的完整媒体文件；该链接可能需要登录，或仅提供暂不支持的流媒体格式"
        );
        resolved.plans.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then(b.0.cmp(&a.0))
                .then((!a.2.label.contains("H.264")).cmp(&(!b.2.label.contains("H.264"))))
                .then(a.2.extension.cmp(&b.2.extension))
        });
        let mut seen = std::collections::HashSet::new();
        resolved
            .plans
            .retain(|(_, _, p)| seen.insert(p.format_id.clone()));
        resolved.plans.truncate(160);
        let now = crate::service::now_ms();
        let id = gosh_dl::DownloadId::new().to_string();
        let expires_at = now + CACHE_TTL_MS;
        let formats = resolved
            .plans
            .iter()
            .map(|(_, audio_only, p)| MediaFormat {
                id: p.format_id.clone(),
                label: p.label.clone(),
                filename: p.filename(),
                extension: p.extension.clone(),
                total_bytes: p.total_size(),
                needs_merge: p.assembly != Assembly::Direct,
                audio_only: *audio_only,
            })
            .collect();
        let mut cache = self.cache.lock().unwrap();
        cache.retain(|_, entry| entry.expires_at > now);
        if cache.len() >= MAX_CACHED {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, c)| c.expires_at)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            id.clone(),
            Cached {
                expires_at,
                plans: resolved.plans.into_iter().map(|(_, _, p)| p).collect(),
            },
        );
        Ok(MediaPreview {
            id,
            title: resolved.title,
            platform: platform.into(),
            duration_seconds: resolved.duration,
            formats,
            ffmpeg_available: self.ffmpeg.is_some(),
            expires_at,
        })
    }

    pub fn select(&self, id: &str, format_id: &str) -> Result<MediaPlan> {
        let cache = self.cache.lock().unwrap();
        let entry = cache
            .get(id)
            .filter(|e| e.expires_at > crate::service::now_ms())
            .context("解析结果已过期，请重新解析视频")?;
        let plan = entry
            .plans
            .iter()
            .find(|p| p.format_id == format_id)
            .context("所选清晰度已不存在，请重新解析")?
            .clone();
        ensure!(
            plan.assembly == Assembly::Direct || self.ffmpeg.is_some(),
            "这个格式需要合并音视频，请先安装 FFmpeg 并重启服务，或选择无需合并的格式"
        );
        plan.validate()?;
        Ok(plan)
    }

    pub async fn validate_download(&self, plan: &MediaPlan) -> Result<()> {
        if plan.platform == "YouTube" {
            for stream in &plan.streams {
                self.probe.check(stream).await.map_err(|error| {
                    anyhow::anyhow!(
                        "所选格式当前无法完整下载，请重新解析并选择可用格式。{}",
                        safe_error(&format!("{error:#}")),
                    )
                })?;
            }
        }
        Ok(())
    }
}

pub fn platform(raw: &str) -> Option<&'static str> {
    let url = crate::filename::parse_url(raw).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let matches = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if matches("bilibili.com") || matches("b23.tv") || matches("bilibili.tv") {
        Some("Bilibili")
    } else if matches("youtube.com") || matches("youtu.be") || matches("youtube-nocookie.com") {
        Some("YouTube")
    } else if matches("douyin.com") || matches("iesdouyin.com") {
        Some("Douyin")
    } else if matches("xiaohongshu.com") || matches("xhslink.com") || matches("xhslink.cn") {
        Some("Xiaohongshu")
    } else if matches("weixin.qq.com") && url.path().starts_with("/sph/")
        || matches("channels.weixin.qq.com") && url.path().starts_with("/finder-preview/pages/")
    {
        Some("WeChat Channels")
    } else if matches("tiktok.com") {
        Some("TikTok")
    } else if matches("instagram.com") {
        Some("Instagram")
    } else if matches("x.com") || matches("twitter.com") {
        Some("X")
    } else if matches("reddit.com") || matches("redd.it") {
        Some("Reddit")
    } else {
        None
    }
}

fn is_manifest(url: &reqwest::Url, mime: Option<&str>) -> bool {
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".m3u8")
        || path.ends_with(".mpd")
        || path.contains("/api/manifest/")
        || mime.is_some_and(|m| m.contains("mpegurl") || m.contains("dash+xml"))
}

fn extension(format: &Format, audio: bool) -> Option<&'static str> {
    match format.container.as_ref() {
        Some(Container::Mp4 | Container::M4a) => Some(if audio { "m4a" } else { "mp4" }),
        Some(Container::WebM | Container::Weba) => Some("webm"),
        _ => match format
            .mime_type
            .as_deref()
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
        {
            "video/mp4" | "audio/mp4" => Some(if audio { "m4a" } else { "mp4" }),
            "video/webm" | "audio/webm" => Some("webm"),
            "audio/mpeg" => Some("mp3"),
            "audio/ogg" => Some("ogg"),
            _ => None,
        },
    }
}

fn stream_key(format: &Format) -> String {
    if let Some(id) = format.itag {
        return id.to_string();
    }
    // Avoid signing-query values in selection identities.
    let spec = format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}",
        format.container, format.video, format.audio, format.bitrate, format.filesize
    );
    format!("{:x}", Sha256::digest(spec))[..16].into()
}

fn yt_stream(format: &Format) -> MediaStream {
    MediaStream {
        url: format.url.clone(),
        headers: format.http_headers.clone(),
        size: format.filesize,
        decrypt_key: None,
    }
}

fn normalize_ytdown(
    url: &str,
    site: &str,
    source_id: &str,
    title: &str,
    duration: Option<u64>,
    formats: &[Format],
) -> Result<Resolved> {
    let usable: Vec<_> = formats
        .iter()
        .filter(|f| {
            crate::filename::parse_url(&f.url)
                .is_ok_and(|u| !is_manifest(&u, f.mime_type.as_deref()))
                && extension(f, f.video.is_none()).is_some()
        })
        .collect();
    let mut plans = Vec::new();
    for f in &usable {
        let audio_only = f.video.is_none();
        if audio_only && f.audio.is_none() {
            continue;
        }
        let ext = extension(f, audio_only).unwrap();
        let height = f.video.as_ref().and_then(|v| v.height).unwrap_or(0);
        let mut streams = vec![yt_stream(f)];
        let mut format_id = stream_key(f);
        let mut output_ext = ext;
        let assembly = if f.video.is_some() && f.audio.is_none() {
            let audio = usable
                .iter()
                .filter(|a| a.video.is_none() && a.audio.is_some())
                .max_by_key(|a| {
                    (
                        matches!(
                            (ext, extension(a, true)),
                            ("mp4", Some("m4a")) | ("webm", Some("webm"))
                        ),
                        a.audio
                            .as_ref()
                            .and_then(|s| s.bitrate)
                            .or(a.bitrate)
                            .unwrap_or(0),
                    )
                });
            let Some(audio) = audio else {
                continue;
            }; // Do not label a silent track a complete video.
            if !matches!(
                (ext, extension(audio, true)),
                ("mp4", Some("m4a")) | ("webm", Some("webm"))
            ) {
                output_ext = "mkv";
            }
            streams.push(yt_stream(audio));
            format_id.push('+');
            format_id.push_str(&stream_key(audio));
            Assembly::Merge
        } else {
            Assembly::Direct
        };
        let kind = if audio_only {
            "仅音频".to_owned()
        } else if height > 0 {
            format!("{height}p")
        } else {
            "视频".to_owned()
        };
        let codec = f
            .video
            .as_ref()
            .map(|v| v.codec.as_str())
            .or_else(|| f.audio.as_ref().map(|a| a.codec.as_str()))
            .unwrap_or("");
        let fps = f
            .video
            .as_ref()
            .and_then(|v| v.fps)
            .filter(|fps| *fps > 30.0)
            .map(|fps| format!(" · {fps:.0} fps"))
            .unwrap_or_default();
        let bitrate = f
            .audio
            .as_ref()
            .and_then(|a| a.bitrate)
            .or(f.bitrate)
            .map(|rate| format!(" · {} kbps", rate / 1000))
            .unwrap_or_default();
        let label = format!(
            "{kind}{fps} · {} · {}{bitrate}",
            output_ext.to_uppercase(),
            codec_label(codec)
        );
        plans.push((
            height,
            audio_only,
            MediaPlan {
                source_url: url.into(),
                source_id: source_id.into(),
                platform: site.into(),
                title: title.into(),
                format_id,
                label,
                extension: output_ext.into(),
                assembly,
                streams,
                extracted_at: crate::service::now_ms(),
            },
        ));
    }
    Ok(Resolved {
        title: title.into(),
        duration,
        plans,
    })
}

fn normalize_native(
    url: &str,
    site: &str,
    video: crate::chinese_video::NativeVideo,
) -> Result<Resolved> {
    let mut plans = Vec::new();
    for variant in video.variants {
        let media_url = crate::filename::parse_url(&variant.url)?;
        if is_manifest(&media_url, None) {
            continue;
        }
        plans.push((
            variant.height,
            false,
            MediaPlan {
                source_url: url.into(),
                source_id: video.id.clone(),
                platform: site.into(),
                title: video.title.clone(),
                format_id: variant.id,
                label: variant.label,
                extension: "mp4".into(),
                assembly: Assembly::Direct,
                streams: vec![MediaStream {
                    url: variant.url,
                    headers: variant.headers,
                    size: variant.size,
                    decrypt_key: variant.decrypt_key,
                }],
                extracted_at: crate::service::now_ms(),
            },
        ));
    }
    Ok(Resolved {
        title: video.title,
        duration: video.duration_seconds,
        plans,
    })
}

fn bili_stream(spec: &MediaRequestSpec) -> MediaStream {
    MediaStream {
        url: spec.url.clone(),
        headers: spec
            .headers
            .iter()
            .map(|h| (h.name.clone(), h.value.clone()))
            .collect(),
        size: spec.size,
        decrypt_key: None,
    }
}

fn normalize_bilibili(url: &str, plan: PlaybackPlan) -> Result<Resolved> {
    let entry = plan
        .entries
        .first()
        .context("这个 B 站链接没有可下载的视频")?;
    ensure!(plan.entries.len() == 1, "请使用单个视频或具体分 P 的链接");
    let title = if entry.index > 1 {
        format!("{} - P{} {}", plan.title, entry.index, entry.title)
    } else {
        plan.title.clone()
    };
    let mut plans = Vec::new();
    let mut variants: Vec<_> = entry.variants.iter().collect();
    variants.sort_by_key(|v| {
        std::cmp::Reverse((
            v.audio
                .as_ref()
                .and_then(|a| a.codecs.as_deref())
                .is_some_and(|c| c.starts_with("mp4a")),
            v.audio.as_ref().and_then(|a| a.bandwidth).unwrap_or(0),
        ))
    });
    let mut seen_video = std::collections::HashSet::new();
    for variant in variants {
        if let Some(video) = &variant.video {
            if !seen_video.insert(video.url.clone()) {
                continue;
            }
        }
        let mut streams = Vec::new();
        let audio_only = variant.video.is_none() && variant.flv_segments.is_empty();
        let (assembly, ext) = if !variant.flv_segments.is_empty() {
            streams.extend(variant.flv_segments.iter().map(bili_stream));
            if streams.len() == 1 {
                (Assembly::Direct, "flv")
            } else {
                (Assembly::Concat, "mkv")
            }
        } else {
            if let Some(v) = &variant.video {
                streams.push(bili_stream(v));
            }
            if let Some(a) = &variant.audio {
                streams.push(bili_stream(a));
            }
            if variant.video.is_some() && variant.audio.is_none() {
                continue;
            }
            if streams.len() == 2 {
                (Assembly::Merge, "mp4")
            } else {
                (Assembly::Direct, "m4a")
            }
        };
        let height = variant.height.unwrap_or(0);
        let kind = if audio_only {
            "仅音频".into()
        } else if let Some(description) = variant
            .video
            .as_ref()
            .and_then(|v| v.stream_id)
            .and_then(|id| entry.qualities.iter().find(|q| q.id == id))
            .and_then(|q| q.description.clone())
        {
            description
        } else if height > 0 {
            format!("{height}p")
        } else {
            "视频".into()
        };
        let codec = variant
            .video
            .as_ref()
            .or(variant.audio.as_ref())
            .and_then(|v| v.codecs.as_deref())
            .unwrap_or("");
        let label = format!("{kind} · {} · {}", ext.to_uppercase(), codec_label(codec));
        plans.push((
            height,
            audio_only,
            MediaPlan {
                source_url: url.into(),
                source_id: entry.cache_key.content_id.clone(),
                platform: "Bilibili".into(),
                title: title.clone(),
                format_id: variant.id.clone(),
                label,
                extension: ext.into(),
                assembly,
                streams,
                extracted_at: crate::service::now_ms(),
            },
        ));
    }
    let mut seen_audio = std::collections::HashSet::new();
    for audio in entry.variants.iter().filter_map(|v| v.audio.as_ref()) {
        if !seen_audio.insert(audio.url.clone())
            || plans.iter().any(|(_, audio_only, p)| {
                *audio_only && p.streams.first().is_some_and(|s| s.url == audio.url)
            })
        {
            continue;
        }
        let rate = audio
            .bandwidth
            .map(|rate| format!(" · {} kbps", rate / 1000))
            .unwrap_or_default();
        plans.push((
            0,
            true,
            MediaPlan {
                source_url: url.into(),
                source_id: entry.cache_key.content_id.clone(),
                platform: "Bilibili".into(),
                title: title.clone(),
                format_id: format!(
                    "audio-{}-{}",
                    audio.stream_id.unwrap_or(0),
                    &format!("{:x}", Sha256::digest(&audio.url))[..12]
                ),
                label: format!(
                    "仅音频{rate} · M4A · {}",
                    codec_label(audio.codecs.as_deref().unwrap_or(""))
                ),
                extension: "m4a".into(),
                assembly: Assembly::Direct,
                streams: vec![bili_stream(audio)],
                extracted_at: crate::service::now_ms(),
            },
        ));
    }
    Ok(Resolved {
        title,
        duration: entry.duration_seconds.map(u64::from),
        plans,
    })
}

fn codec_label(codec: &str) -> &str {
    if codec.starts_with("avc") {
        "H.264"
    } else if codec.starts_with("hev") || codec.starts_with("hvc") {
        "HEVC"
    } else if codec.starts_with("av01") {
        "AV1"
    } else if codec.starts_with("vp9") || codec.starts_with("vp09") {
        "VP9"
    } else if codec.starts_with("mp4a") {
        "AAC"
    } else if codec.is_empty() {
        "默认编码"
    } else {
        codec
    }
}

pub fn media_filename(title: &str, ext: &str) -> String {
    let mut stem: String = title
        .chars()
        .map(|c| {
            if c.is_control()
                || "/\\:*?\"<>|".contains(c)
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    stem = stem.trim_matches(['.', ' ']).to_owned();
    let max = 165usize.saturating_sub(ext.len() + 1);
    while stem.len() > max {
        stem.pop();
    }
    if stem.is_empty() {
        stem = "视频".into();
    }
    if crate::filename::is_windows_reserved_name(&stem) {
        stem.insert(0, '_');
    }
    format!("{stem}.{ext}")
}

pub fn find_ffmpeg() -> Option<PathBuf> {
    let mut paths = Vec::new();
    let executable = if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    if let Some(path) = std::env::var_os("FFDM_FFMPEG") {
        paths.push(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path).map(|p| p.join(executable)));
    }
    paths.extend([
        PathBuf::from("/opt/homebrew/bin/ffmpeg"),
        PathBuf::from("/usr/local/bin/ffmpeg"),
    ]);
    paths.into_iter().find(|p| p.is_file())
}

pub fn safe_error(message: &str) -> String {
    // Upstream errors may include signed URLs. Keep diagnostics but strip addresses.
    let mut out = String::new();
    let mut rest = message;
    while let Some(pos) = rest
        .find("http://")
        .into_iter()
        .chain(rest.find("https://"))
        .min()
    {
        out.push_str(&rest[..pos]);
        out.push_str("[媒体地址]");
        let tail = &rest[pos..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '>'))
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out.chars().take(500).collect()
}

fn ytdown_error(error: ytdown::Error) -> anyhow::Error {
    use ytdown::{error::UnavailableReason, Error};
    let message = match &error {
        Error::Cipher(_) => "YouTube 签名解析失败，当前解析库尚未适配该播放器版本".to_owned(),
        Error::Unavailable {
            reason: UnavailableReason::BotCheck,
            ..
        } => "平台要求登录或人机验证，当前版本支持无需登录的公开视频".into(),
        Error::Unavailable {
            reason: UnavailableReason::AgeRestricted,
            ..
        } => "该视频需要年龄验证，当前版本暂不支持登录".into(),
        Error::Unavailable {
            reason: UnavailableReason::Live,
            ..
        } => "当前版本暂不支持直播录制".into(),
        Error::Unavailable { .. } => {
            format!("视频当前不可访问：{}", safe_error(&error.to_string()))
        }
        _ => format!("视频解析失败：{}", safe_error(&error.to_string())),
    };
    anyhow::anyhow!(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Request, response::IntoResponse, Json, Router};
    use serde_json::json;

    #[test]
    fn routing_names_and_headers_reject_unsafe_inputs() {
        assert_eq!(platform("https://youtu.be/abcdefghijk"), Some("YouTube"));
        assert_eq!(platform("https://b23.tv/abc"), Some("Bilibili"));
        assert_eq!(platform("https://v.douyin.com/example"), Some("Douyin"));
        assert_eq!(
            platform("https://xhslink.cn/o/example"),
            Some("Xiaohongshu")
        );
        assert_eq!(
            platform("https://weixin.qq.com/sph/example"),
            Some("WeChat Channels")
        );
        assert_eq!(
            platform("https://youtube.com.evil.example/watch?v=abc"),
            None
        );
        assert_eq!(platform("file:///tmp/example"), None);
        let name = media_filename(&format!("../访谈:{}\u{202e}", "长".repeat(100)), "mp4");
        assert!(
            name.len() <= 165
                && name.ends_with(".mp4")
                && !name.contains('/')
                && !name.starts_with('.')
        );
        assert_eq!(media_filename("CON", "mp4"), "_CON.mp4");
        assert!(
            crate::download::validate_headers(&[("Range".into(), "bytes=0-1".into())]).is_err()
        );
        assert!(crate::download::validate_headers(&[(
            "Referer".into(),
            "ok\r\nX-Injected: yes".into()
        )])
        .is_err());
        assert_eq!(
            safe_error("failed https://cdn.example/v?sig=secret then http://x/secret"),
            "failed [媒体地址] then [媒体地址]"
        );
    }

    #[test]
    fn pair_audio_and_video_without_presenting_silent_tracks_or_manifests() {
        let mut video = Format::default();
        video.itag = Some(137);
        video.url = "https://cdn.example/video".into();
        video.container = Some(Container::Mp4);
        let mut v = ytdown::VideoStream::default();
        v.height = Some(1080);
        v.codec = "avc1.640028".into();
        video.video = Some(v);
        let mut audio = Format::default();
        audio.itag = Some(140);
        audio.url = "https://cdn.example/audio".into();
        audio.container = Some(Container::M4a);
        let mut a = ytdown::AudioStream::default();
        a.codec = "mp4a.40.2".into();
        audio.audio = Some(a);
        let silent = normalize_ytdown(
            "https://youtube.com/watch?v=abcdefghijk",
            "YouTube",
            "id",
            "Title",
            None,
            &[video.clone()],
        )
        .unwrap();
        assert!(silent.plans.is_empty());
        let mut hls = video.clone();
        hls.url = "https://cdn.example/video.m3u8".into();
        let result = normalize_ytdown(
            "https://youtube.com/watch?v=abcdefghijk",
            "YouTube",
            "id",
            "Title",
            None,
            &[video, audio, hls],
        )
        .unwrap();
        assert_eq!(result.plans.len(), 2);
        let merged = &result.plans[0].2;
        assert_eq!(merged.assembly, Assembly::Merge);
        assert_eq!(merged.format_id, "137+140");
        assert_eq!(merged.extension, "mp4");
        assert!(result.plans[1].1);
        assert_eq!(result.plans[1].2.extension, "m4a");
    }

    async fn platform_fixture(request: Request) -> axum::response::Response {
        let base = format!("http://{}", request.headers()["host"].to_str().unwrap());
        if matches!(request.uri().path(), "/cdn/video" | "/cdn/audio") {
            let length = if request.uri().path() == "/cdn/video" {
                1234
            } else {
                123
            };
            let data = crate::test_server::fixture_data(length);
            if let Some(range) = request.headers().get("range") {
                let range = range.to_str().unwrap().strip_prefix("bytes=").unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start: usize = start.parse().unwrap();
                let end: usize = end.parse().unwrap();
                return axum::http::Response::builder()
                    .status(206)
                    .header("content-range", format!("bytes {start}-{end}/{length}"))
                    .body(axum::body::Body::from(data.slice(start..=end)))
                    .unwrap();
            }
            return axum::body::Body::from(data).into_response();
        }
        let value = match request.uri().path() {
            "/youtubei/v1/player" => json!({
                "playabilityStatus":{"status":"OK"},
                "videoDetails":{"videoId":"abcdefghijk","title":"原生解析测试","lengthSeconds":"3","isLiveContent":false},
                "streamingData":{"adaptiveFormats":[
                    {"itag":137,"url":format!("{base}/cdn/video?sig=private"),"mimeType":"video/mp4; codecs=\"avc1.640028\"","width":1920,"height":1080,"fps":30,"contentLength":"1234"},
                    {"itag":140,"url":format!("{base}/cdn/audio?sig=private"),"mimeType":"audio/mp4; codecs=\"mp4a.40.2\"","audioQuality":"AUDIO_QUALITY_MEDIUM","contentLength":"123"}
                ]}
            }),
            "/x/web-interface/nav" => {
                json!({"code":0,"data":{"wbi_img":{"img_url":"https://i0.hdslb.com/bfs/wbi/0123456789abcdef0123456789abcdef.png","sub_url":"https://i0.hdslb.com/bfs/wbi/fedcba9876543210fedcba9876543210.png"}}})
            }
            "/x/web-interface/view" | "/x/web-interface/wbi/view" => {
                json!({"code":0,"data":{"aid":170001,"bvid":"BV17x411w7KC","cid":9988,"title":"B 站解析测试","pic":"","desc":"","pages":[{"cid":9988,"page":1,"part":"Part 1","duration":3}],"owner":{"mid":1,"name":"Fixture"}}})
            }
            "/x/player/playurl" | "/x/player/wbi/playurl" => {
                json!({"code":0,"data":{"dash":{"duration":3,
                    "video":[{"id":80,"baseUrl":"https://cdn.example/video","mimeType":"video/mp4","codecs":"avc1.640028","width":1920,"height":1080}],
                    "audio":[{"id":30280,"baseUrl":"https://cdn.example/audio","mimeType":"audio/mp4","codecs":"mp4a.40.2"}]
                }}})
            }
            _ => return axum::http::StatusCode::NOT_FOUND.into_response(),
        };
        Json(value).into_response()
    }

    #[tokio::test]
    async fn both_native_extractors_produce_cached_plans_and_expired_results_are_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(platform_fixture))
                .await
                .unwrap();
        });
        let mut resolver = MediaResolver::new().unwrap();
        resolver.youtube = crate::youtube::YoutubeResolver::with_base_url(
            reqwest_media::Client::new(),
            resolver.probe.clone(),
            &base,
        );
        resolver.bilibili = BiliClient::new(
            ClientConfig::default()
                .with_endpoints(bbdown_core::EndpointConfig::default().with_api_base(&base)),
        );
        // Selection does not execute this binary; presence only enables merged plans.
        resolver.ffmpeg = Some(PathBuf::from("ffmpeg"));
        let youtube = resolver
            .resolve("https://www.youtube.com/watch?v=abcdefghijk")
            .await
            .unwrap();
        assert_eq!(youtube.title, "原生解析测试");
        let video = youtube.formats.iter().find(|f| !f.audio_only).unwrap();
        let plan = resolver.select(&youtube.id, &video.id).unwrap();
        assert_eq!(plan.total_size(), Some(1357));
        assert_eq!(plan.assembly, Assembly::Merge);
        assert!(!serde_json::to_string(&youtube).unwrap().contains("private"));
        let bili = resolver
            .resolve("https://www.bilibili.com/video/av170001")
            .await
            .unwrap();
        let plan = resolver.select(&bili.id, &bili.formats[0].id).unwrap();
        assert_eq!(plan.streams.len(), 2);
        assert!(plan.streams[0]
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("referer") && v.contains("bilibili.com")));
        assert!(resolver.select(&bili.id, "unknown").is_err());
        resolver
            .cache
            .lock()
            .unwrap()
            .get_mut(&bili.id)
            .unwrap()
            .expires_at = 0;
        assert!(resolver.select(&bili.id, &bili.formats[0].id).is_err());
        server.abort();
    }
}
