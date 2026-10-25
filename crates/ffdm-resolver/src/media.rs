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

/// Post-download processing references contain no account or playback secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamTransform {
    SunoAudio { content_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaStream {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub size: Option<u64>,
    #[serde(default)]
    pub decrypt_key: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<StreamTransform>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Assembly {
    Direct,
    Merge,
    Concat,
    Hls,
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
            !self.streams.is_empty()
                && self.streams.len()
                    <= if self.assembly == Assembly::Hls {
                        4096
                    } else {
                        128
                    },
            "视频没有可下载的媒体轨道"
        );
        ensure!(
            (self.assembly == Assembly::Direct
                && !self.extension.is_empty()
                && self.extension.len() <= 32
                && self.extension.split('.').all(
                    |part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric())
                ))
                || matches!(
                    self.extension.as_str(),
                    "mp4"
                        | "webm"
                        | "mkv"
                        | "mov"
                        | "m4a"
                        | "mp3"
                        | "ogg"
                        | "flv"
                        | "flac"
                        | "wav"
                        | "aac"
                        | "opus"
                        | "ape"
                ),
            "不支持或不安全的文件扩展名"
        );
        ensure!(
            self.assembly != Assembly::Direct || self.streams.len() == 1,
            "单文件格式包含多个地址"
        );
        ensure!(
            self.assembly != Assembly::Merge || self.streams.len() == 2,
            "音视频合并需要两个轨道"
        );
        ensure!(
            self.assembly != Assembly::Hls || matches!(self.extension.as_str(), "mp4" | "m4a"),
            "HLS 需要封装为 MP4 视频或 M4A 音频"
        );
        for stream in &self.streams {
            let url = crate::validation::parse_url(&stream.url)?;
            ensure!(
                !is_manifest(&url, None),
                "此格式是流媒体清单，当前版本只下载完整媒体文件"
            );
            crate::validation::validate_headers(&stream.headers)?;
            if self.assembly == Assembly::Hls {
                crate::hls::validate_headers(&stream.headers)?;
            }
            ensure!(
                stream.decrypt_key.is_none()
                    || (self.platform == "WeChat Channels" && self.assembly == Assembly::Direct),
                "加密媒体只能使用视频号单文件格式"
            );
            if let Some(StreamTransform::SunoAudio { content_id }) = &stream.transform {
                let song = reqwest::Url::parse(&format!("https://suno.com/song/{content_id}"))?;
                ensure!(
                    self.platform == "Suno"
                        && self.assembly == Assembly::Direct
                        && self.extension == "m4a"
                        && stream.decrypt_key.is_none()
                        && content_id.len() == 36
                        && self.source_id == format!("suno-{content_id}")
                        && crate::platforms::suno_song_page(&song),
                    "Suno 音频处理信息与当前下载作品不一致"
                );
            }
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

/// A verified selectable format and its private executor input.
#[derive(Clone, Debug)]
pub struct ExtractedFormat {
    pub height: u32,
    pub audio_only: bool,
    pub plan: MediaPlan,
}

/// Uncached extraction output for hosts with their own queue and format selection.
/// `plan.streams` contain expiring addresses; do not publish them in a UI or logs.
#[derive(Clone, Debug)]
pub struct ExtractedResource {
    pub source_url: String,
    pub title: String,
    pub platform: String,
    pub duration_seconds: Option<u64>,
    pub formats: Vec<ExtractedFormat>,
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
    qishui: crate::qishui::QishuiResolver,
    platforms: crate::platforms::PlatformResolver,
    generic: crate::generic::GenericResolver,
    custom: Vec<std::sync::Arc<dyn crate::ResourceExtractor>>,
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
            youtube: crate::youtube::YoutubeResolver::new(),
            other_platforms: Ytdown::builder().client(client).build()?,
            probe,
            bilibili: BiliClient::new(
                ClientConfig::default().with_request_timeout(Duration::from_secs(15)),
            ),
            chinese: crate::chinese_video::ChineseResolver::new()?,
            qishui: crate::qishui::QishuiResolver::new(),
            platforms: crate::platforms::PlatformResolver::new()?,
            generic: crate::generic::GenericResolver::new()?,
            custom: Vec::new(),
            cache: Mutex::new(HashMap::new()),
            slots: Semaphore::new(2),
            ffmpeg,
        })
    }

    pub async fn resolve(&self, raw: &str) -> Result<MediaPreview> {
        self.resolve_with_cookie(raw, None).await
    }

    /// Prefer a host adapter for its matching URLs, then use built-in and generic extraction.
    pub fn with_extractor(mut self, extractor: impl crate::ResourceExtractor + 'static) -> Self {
        self.custom.push(std::sync::Arc::new(extractor));
        self
    }

    pub async fn resolve_with_cookie(
        &self,
        raw: &str,
        platform_cookie: Option<&str>,
    ) -> Result<MediaPreview> {
        let extracted = self.extract_with_cookie(raw, platform_cookie).await?;
        self.cache_resolved(
            &extracted.platform,
            Resolved {
                title: extracted.title,
                duration: extracted.duration_seconds,
                plans: extracted
                    .formats
                    .into_iter()
                    .map(|f| (f.height, f.audio_only, f.plan))
                    .collect(),
            },
        )
    }

    /// Parse without a UI cache or application task state.
    pub async fn extract(&self, raw: &str) -> Result<ExtractedResource> {
        self.extract_with_cookie(raw, None).await
    }

    /// Credentials are used only by the selected platform adapter for this request.
    pub async fn extract_with_cookie(
        &self,
        raw: &str,
        platform_cookie: Option<&str>,
    ) -> Result<ExtractedResource> {
        let input = crate::validation::parse_url(raw)?;
        let _permit = self
            .slots
            .try_acquire()
            .context("已有两个音视频链接正在解析，请稍后再试")?;
        let work = async {
            let mut url = input.clone();
            let mut visited = std::collections::HashSet::new();
            for _ in 0..3 {
                ensure!(
                    visited.insert(url.to_string()),
                    "网页嵌入出现循环，请使用单作品链接"
                );
                if let Some(extractor) = self.custom.iter().find(|e| e.supports(&url)) {
                    ensure!(
                        platform_cookie.is_none_or(|c| c.trim().is_empty()),
                        "自定义解析器通过自身配置提供授权，不能转发平台 Cookie"
                    );
                    let resource = extractor.extract(&url).await?;
                    return finalize_resource(
                        input.as_str(),
                        &resource.platform,
                        Resolved {
                            title: resource.title,
                            duration: resource.duration_seconds,
                            plans: resource
                                .formats
                                .into_iter()
                                .map(|f| (f.height, f.audio_only, f.plan))
                                .collect(),
                        },
                    );
                }
                let (site, resolved) = if let Some(site) = crate::routing::parser_platform(&url) {
                    if site == "Suno" {
                        let resource = self.platforms.resolve_suno(&url).await?;
                        (
                            site,
                            self.normalize_generic(url.as_str(), site, resource).await?,
                        )
                    } else if matches!(
                        site,
                        "CCTV"
                            | "Ixigua"
                            | "PearVideo"
                            | "Archive.org"
                            | "TED"
                            | "WikimediaCommons"
                            | "Bluesky"
                            | "PeerTube"
                            | "Medal"
                            | "9GAG"
                            | "Mastodon"
                            | "Tumblr"
                            | "Bandcamp"
                            | "SoundCloud"
                            | "Sohu"
                            | "Haokan"
                            | "Clyp"
                            | "AudioBoom"
                            | "Audius"
                            | "HuyaVideo"
                            | "NetEaseNews"
                            | "Rumble"
                            | "Odysee"
                            | "OK.ru"
                    ) {
                        let resource = self.platforms.resolve_resource(&url, site).await?;
                        if let Some(embed) = resource.embed {
                            ensure!(
                                resource.candidates.is_empty()
                                    && crate::routing::platform(embed.as_str()).is_some(),
                                "平台返回了无法识别的嵌入媒体"
                            );
                            ensure!(
                                platform_cookie.is_none_or(|cookie| cookie.trim().is_empty()),
                                "外部播放器不转发原平台的 Cookie，请使用播放器自身的作品链接"
                            );
                            url = embed;
                            continue;
                        }
                        (
                            site,
                            self.normalize_generic(url.as_str(), site, resource).await?,
                        )
                    } else {
                        (site, self.extract_known(&url, site, platform_cookie).await?)
                    }
                } else {
                    ensure!(
                        platform_cookie.is_none_or(|c| c.trim().is_empty()),
                        "通用网页解析不接收平台登录 Cookie，请使用对应平台的作品链接"
                    );
                    match crate::resource::inspect(&url).await? {
                        crate::resource::ResourceKind::Direct {
                            title,
                            candidate,
                            manifest,
                        } => {
                            let site = if manifest { "HLS" } else { "Direct File" };
                            let resource = crate::generic::GenericResource {
                                id: candidate.id.clone(),
                                title,
                                duration_seconds: None,
                                candidates: vec![candidate],
                                embed: None,
                            };
                            (
                                site,
                                self.normalize_generic(url.as_str(), site, resource).await?,
                            )
                        }
                        crate::resource::ResourceKind::Page(page) => {
                            if crate::routing::parser_platform(&page).is_some() {
                                url = page;
                                continue;
                            }
                            if crate::routing::is_peertube_page(&page) {
                                if let Ok(resource) =
                                    self.platforms.resolve_resource(&page, "PeerTube").await
                                {
                                    let resolved = self
                                        .normalize_generic(page.as_str(), "PeerTube", resource)
                                        .await?;
                                    return finalize_resource(input.as_str(), "PeerTube", resolved);
                                }
                            }
                            if crate::routing::is_mastodon_page(&page) {
                                if let Ok(resource) =
                                    self.platforms.resolve_resource(&page, "Mastodon").await
                                {
                                    let resolved = self
                                        .normalize_generic(page.as_str(), "Mastodon", resource)
                                        .await?;
                                    return finalize_resource(input.as_str(), "Mastodon", resolved);
                                }
                            }
                            let resource = self.generic.resolve(&page).await?;
                            if let Some(embed) = resource.embed {
                                ensure!(
                                    resource.candidates.is_empty(),
                                    "网页包含多个资源，请使用单作品链接"
                                );
                                ensure!(
                                    crate::routing::platform(embed.as_str()).is_some(),
                                    "网页嵌入的平台暂未接入，请使用播放器的直接媒体地址"
                                );
                                url = embed;
                                continue;
                            }
                            (
                                "Generic",
                                self.normalize_generic(page.as_str(), "Generic", resource)
                                    .await?,
                            )
                        }
                    }
                };
                return finalize_resource(input.as_str(), site, resolved);
            }
            bail!("网页嵌入层级过多，请使用单作品链接")
        };
        tokio::time::timeout(Duration::from_secs(50), work)
            .await
            .context("音视频解析超时，请检查网络后重试")?
            .map_err(|error| anyhow::anyhow!(safe_error(&format!("{error:#}"))))
    }

    async fn extract_known(
        &self,
        url: &reqwest::Url,
        platform: &str,
        platform_cookie: Option<&str>,
    ) -> Result<Resolved> {
        if platform == "Qishui Music" {
            let track = self.qishui.resolve(url).await.map_err(|error| {
                anyhow::anyhow!("汽水音乐解析失败：{}", safe_error(&format!("{error:#}")))
            })?;
            Ok(normalize_qishui(track))
        } else if matches!(
            platform,
            "Kuaishou" | "Weibo" | "AcFun" | "Vimeo" | "Dailymotion" | "Streamable" | "Imgur"
        ) {
            let video = self
                .platforms
                .resolve(url, platform)
                .await
                .map_err(|error| {
                    anyhow::anyhow!("{platform} 解析失败：{}", safe_error(&format!("{error:#}")))
                })?;
            self.normalize_platform(url.as_str(), platform, video).await
        } else if matches!(platform, "Douyin" | "Xiaohongshu" | "WeChat Channels") {
            let video = self
                .chinese
                .resolve(url, platform, platform_cookie)
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
            let (info, youtube_probe) = if platform == "YouTube" {
                let resolved = self
                    .youtube
                    .resolve_with_cookie(url, platform_cookie)
                    .await
                    .map_err(youtube_error)?;
                (resolved.info, Some(resolved.probe))
            } else {
                let info = self
                    .other_platforms
                    .resolve(url.as_str())
                    .await
                    .map_err(ytdown_error)?;
                (info, None)
            };
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
                    if let Some(probe) = youtube_probe {
                        use futures_util::{stream, StreamExt};
                        let checked: Vec<_> =
                            stream::iter(resolved.plans.into_iter().map(|plan| async {
                                for media in &plan.2.streams {
                                    if probe.check(media).await.is_err() {
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
    }

    fn cache_resolved(&self, platform: &str, mut resolved: Resolved) -> Result<MediaPreview> {
        prepare_resolved(&mut resolved)?;
        let now = crate::utils::now_ms();
        let id = crate::utils::unique_id()?;
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
            .filter(|e| e.expires_at > crate::utils::now_ms())
            .context("解析结果已过期，请重新解析链接")?;
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
        if matches!(
            plan.platform.as_str(),
            "YouTube"
                | "Qishui Music"
                | "Kuaishou"
                | "Weibo"
                | "AcFun"
                | "Vimeo"
                | "Dailymotion"
                | "Streamable"
                | "Imgur"
                | "Generic"
                | "HLS"
                | "CCTV"
                | "Ixigua"
                | "PearVideo"
                | "Archive.org"
                | "TED"
                | "WikimediaCommons"
                | "Bluesky"
                | "PeerTube"
                | "Medal"
                | "9GAG"
                | "Mastodon"
                | "Tumblr"
                | "Bandcamp"
                | "SoundCloud"
                | "Sohu"
                | "Haokan"
                | "Clyp"
                | "AudioBoom"
                | "Audius"
                | "Suno"
                | "HuyaVideo"
                | "NetEaseNews"
                | "Rumble"
                | "Odysee"
                | "OK.ru"
        ) {
            let streams: Vec<_> = if plan.assembly == Assembly::Hls {
                // Thousands of segments must not turn task creation into a
                // second complete playlist traversal. Check its ends now;
                // the Rust downloader validates each response as it arrives.
                plan.streams
                    .first()
                    .into_iter()
                    .chain(plan.streams.last())
                    .collect()
            } else {
                plan.streams.iter().collect()
            };
            for stream in streams {
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

    async fn normalize_generic(
        &self,
        url: &str,
        site: &str,
        resource: crate::generic::GenericResource,
    ) -> Result<Resolved> {
        use futures_util::{stream, StreamExt};
        let mut failures = Vec::new();
        let probe_concurrency = if site == "WikimediaCommons" { 1 } else { 4 };
        let prepared = stream::iter(resource.candidates.into_iter().take(32).map(
            |mut candidate| async move {
                let work = async {
                    ensure!(candidate.extension != "mpd", "暂不支持通用 DASH 清单");
                    if candidate.extension.is_empty() {
                        match crate::resource::inspect_with_headers(
                            &crate::validation::parse_url(&candidate.url)?,
                            &candidate.headers,
                        )
                        .await?
                        {
                            crate::resource::ResourceKind::Direct {
                                candidate: identified,
                                ..
                            } => {
                                candidate.extension = identified.extension;
                                candidate.size = identified.size;
                                candidate.audio_only = identified.audio_only;
                            }
                            _ => bail!("媒体地址返回了网页，不能作为媒体文件"),
                        }
                    }
                    if candidate.extension == "m3u8" {
                        let variants =
                            crate::manifest::variants(&candidate.url, &candidate.headers).await?;
                        Ok::<_, anyhow::Error>(
                            variants
                                .into_iter()
                                .map(|variant| {
                                    let mut candidate = candidate.clone();
                                    candidate.id = format!("{}-{}", candidate.id, variant.id);
                                    if variant.height > 0
                                        || (candidate.height == 0 && !candidate.audio_only)
                                    {
                                        candidate.label = variant.label;
                                    }
                                    if variant.height > 0 {
                                        candidate.height = variant.height;
                                    }
                                    candidate.url = variant.url;
                                    candidate.headers = variant.headers;
                                    candidate
                                })
                                .collect::<Vec<_>>(),
                        )
                    } else {
                        Ok(vec![candidate])
                    }
                };
                tokio::time::timeout(Duration::from_secs(12), work)
                    .await
                    .context("资源地址识别超时")?
            },
        ))
        .buffer_unordered(probe_concurrency)
        .flat_map(|result| {
            stream::iter(match result {
                Ok(candidates) => candidates.into_iter().map(Ok).collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            })
        });
        let mut attempts = prepared
            .map(|candidate| {
                let source_id = &resource.id;
                let title = &resource.title;
                async move {
                    let work = async {
                        let candidate = candidate?;
                        let hls = candidate.extension == "m3u8";
                        let streams = if hls {
                            crate::hls::expand(&candidate.url, &candidate.headers).await?
                        } else {
                            vec![MediaStream {
                                url: candidate.url,
                                headers: candidate.headers,
                                size: candidate.size,
                                decrypt_key: None,
                                transform: candidate.transform,
                            }]
                        };
                        if site != "Direct File" {
                            ensure!(
                                hls || matches!(
                                    candidate.extension.as_str(),
                                    "mp4"
                                        | "webm"
                                        | "mkv"
                                        | "mov"
                                        | "m4a"
                                        | "mp3"
                                        | "ogg"
                                        | "oga"
                                        | "flv"
                                        | "flac"
                                        | "wav"
                                        | "aac"
                                        | "opus"
                                        | "ape"
                                        | "avi"
                                        | "mpg"
                                        | "mpeg"
                                        | "ogv"
                                        | "m4v"
                                        | "3gp"
                                        | "3g2"
                                        | "ts"
                                        | "m2ts"
                                        | "weba"
                                ),
                                "网页媒体格式暂不支持，请使用文件直链下载"
                            );
                            for stream in streams.first().into_iter().chain(streams.last()) {
                                self.probe.check(stream).await?;
                            }
                        }
                        Ok::<_, anyhow::Error>((
                            candidate.height,
                            candidate.audio_only,
                            MediaPlan {
                                source_url: url.into(),
                                source_id: source_id.clone(),
                                platform: site.into(),
                                title: title.clone(),
                                format_id: candidate.id,
                                label: if hls
                                    || candidate
                                        .label
                                        .to_ascii_uppercase()
                                        .contains(&candidate.extension.to_ascii_uppercase())
                                {
                                    candidate.label
                                } else {
                                    format!(
                                        "{} · {}",
                                        candidate.label,
                                        candidate.extension.to_ascii_uppercase()
                                    )
                                },
                                extension: if hls {
                                    if candidate.audio_only { "m4a" } else { "mp4" }.into()
                                } else {
                                    candidate.extension
                                },
                                assembly: if hls { Assembly::Hls } else { Assembly::Direct },
                                streams,
                                extracted_at: crate::utils::now_ms(),
                            },
                        ))
                    };
                    tokio::time::timeout(Duration::from_secs(12), work)
                        .await
                        .context("资源格式检查超时")?
                }
            })
            .buffer_unordered(probe_concurrency);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(24);
        let mut plans = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, attempts.next()).await {
                Ok(Some(Ok(plan))) => plans.push(plan),
                Ok(Some(Err(error))) => failures.push(safe_error(&format!("{error:#}"))),
                Ok(None) | Err(_) => break,
            }
        }
        drop(attempts);
        ensure!(
            !plans.is_empty(),
            "没有找到可下载的网页资源。{}",
            failures
                .first()
                .map(String::as_str)
                .unwrap_or("请使用单作品页面或文件直链")
        );
        Ok(Resolved {
            title: resource.title,
            duration: resource.duration_seconds,
            plans,
        })
    }

    async fn normalize_platform(
        &self,
        url: &str,
        site: &str,
        video: crate::chinese_video::NativeVideo,
    ) -> Result<Resolved> {
        use futures_util::{stream, StreamExt};
        let mut plans = Vec::new();
        let mut failures = Vec::new();
        let source_id = &video.id;
        let title = &video.title;
        let mut attempts = stream::iter(video.variants.into_iter().take(32).map(
            |variant| async move {
                let check = async {
                    let media_url = crate::validation::parse_url(&variant.url)?;
                    let hls = is_manifest(&media_url, None);
                    let streams = if hls {
                        crate::hls::expand(&variant.url, &variant.headers).await?
                    } else {
                        vec![MediaStream {
                            url: variant.url,
                            headers: variant.headers,
                            size: variant.size,
                            decrypt_key: None,
                            transform: None,
                        }]
                    };
                    for stream in streams.first().into_iter().chain(streams.last()) {
                        self.probe.check(stream).await?;
                    }
                    Ok::<_, anyhow::Error>((
                        variant.height,
                        false,
                        MediaPlan {
                            source_url: url.into(),
                            source_id: source_id.clone(),
                            platform: site.into(),
                            title: title.clone(),
                            format_id: variant.id,
                            label: if hls && !variant.label.contains("HLS") {
                                format!("{} · HLS", variant.label)
                            } else {
                                variant.label
                            },
                            extension: "mp4".into(),
                            assembly: if hls { Assembly::Hls } else { Assembly::Direct },
                            streams,
                            extracted_at: crate::utils::now_ms(),
                        },
                    ))
                };
                tokio::time::timeout(Duration::from_secs(12), check)
                    .await
                    .context("此清晰度的可读性检查超时")?
            },
        ))
        .buffer_unordered(4);
        // Keep already verified choices when one CDN is slow. The outer parser
        // timeout includes metadata lookup; format checks have a smaller budget.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(24);
        loop {
            match tokio::time::timeout_at(deadline, attempts.next()).await {
                Ok(Some(Ok(plan))) => plans.push(plan),
                Ok(Some(Err(error))) => failures.push(safe_error(&format!("{error:#}"))),
                Ok(None) => break,
                Err(_) => {
                    failures.push("部分清晰度检查超时，请稍后重新解析".into());
                    break;
                }
            }
        }
        drop(attempts);
        ensure!(
            !plans.is_empty(),
            "没有找到当前可下载的公开视频格式。{}",
            failures
                .first()
                .map(String::as_str)
                .unwrap_or("平台没有提供可访问的资源")
        );
        Ok(Resolved {
            title: video.title,
            duration: video.duration_seconds,
            plans,
        })
    }
}

fn prepare_resolved(resolved: &mut Resolved) -> Result<()> {
    resolved
        .plans
        .retain(|(_, _, plan)| plan.validate().is_ok());
    ensure!(
        !resolved.plans.is_empty(),
        "没有找到可下载的完整媒体文件；该链接可能需要登录，或仅提供暂不支持的流媒体格式"
    );
    resolved.plans.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| {
                let original = |plan: &MediaPlan| {
                    plan.platform == "Xiaohongshu"
                        && plan.format_id == crate::chinese_video::XHS_ORIGINAL_FORMAT
                };
                original(&b.2).cmp(&original(&a.2))
            })
            .then(b.0.cmp(&a.0))
            .then((!a.2.label.contains("H.264")).cmp(&(!b.2.label.contains("H.264"))))
            .then(a.2.extension.cmp(&b.2.extension))
    });
    let mut seen = std::collections::HashSet::new();
    resolved
        .plans
        .retain(|(_, _, p)| seen.insert(p.format_id.clone()));
    resolved.plans.truncate(160);
    Ok(())
}

fn finalize_resource(
    url: &str,
    platform: &str,
    mut resolved: Resolved,
) -> Result<ExtractedResource> {
    prepare_resolved(&mut resolved)?;
    Ok(ExtractedResource {
        source_url: url.into(),
        title: resolved.title,
        platform: platform.into(),
        duration_seconds: resolved.duration,
        formats: resolved
            .plans
            .into_iter()
            .map(|(height, audio_only, plan)| ExtractedFormat {
                height,
                audio_only,
                plan,
            })
            .collect(),
    })
}

pub fn platform(raw: &str) -> Option<&'static str> {
    crate::routing::platform(raw)
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
        transform: None,
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
            crate::validation::parse_url(&f.url)
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
                extracted_at: crate::utils::now_ms(),
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
        let media_url = crate::validation::parse_url(&variant.url)?;
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
                    transform: None,
                }],
                extracted_at: crate::utils::now_ms(),
            },
        ));
    }
    Ok(Resolved {
        title: video.title,
        duration: video.duration_seconds,
        plans,
    })
}

fn normalize_qishui(track: crate::qishui::QishuiTrack) -> Resolved {
    let kind = if track.preview {
        "网页试听片段"
    } else {
        "网页音频"
    };
    let label = track.bitrate_kbps.map_or_else(
        || format!("{kind} · {}", track.extension.to_uppercase()),
        |rate| {
            format!(
                "{kind} · {} · 约 {rate} kbps",
                track.extension.to_uppercase()
            )
        },
    );
    let plan = MediaPlan {
        source_url: format!(
            "https://music.douyin.com/qishui/share/track?track_id={}",
            track.id
        ),
        source_id: track.id,
        platform: "Qishui Music".into(),
        title: track.title.clone(),
        format_id: "qishui-audio".into(),
        label,
        extension: track.extension.into(),
        assembly: Assembly::Direct,
        streams: vec![MediaStream {
            url: track.media_url,
            headers: track.headers,
            size: track.size,
            decrypt_key: None,
            transform: None,
        }],
        extracted_at: crate::utils::now_ms(),
    };
    Resolved {
        title: track.title,
        duration: track.duration_seconds,
        plans: vec![(0, true, plan)],
    }
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
        transform: None,
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
                extracted_at: crate::utils::now_ms(),
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
                extracted_at: crate::utils::now_ms(),
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
    if crate::validation::is_windows_reserved_name(&stem) {
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

fn youtube_error(error: anyhow::Error) -> anyhow::Error {
    use ytdown::{error::UnavailableReason, Error};
    match error.downcast::<Error>() {
        Ok(Error::Unavailable { reason: UnavailableReason::BotCheck, .. }) => anyhow::anyhow!(
            "YouTube 要求验证当前访问会话。请在同一网络的浏览器中打开此视频，完成平台登录或验证后，将该页面请求的 Cookie 填入上方「YouTube 网页 Cookie」并重新解析。浏览器登录状态不会自动共享；若仍被拒绝，请检查 Cookie 是否过期以及浏览器与客户端的网络/代理是否一致。"
        ),
        Ok(Error::Unavailable { reason: UnavailableReason::AgeRestricted, .. }) => anyhow::anyhow!(
            "YouTube 要求登录或年龄验证。请先在浏览器中用有访问权限的账号确认此视频可以播放，再填写上方「YouTube 网页 Cookie」并重新解析。"
        ),
        Ok(error) => ytdown_error(error),
        Err(error) => error,
    }
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
    fn youtube_login_errors_offer_the_authorization_path_without_echoing_server_data() {
        let error = youtube_error(
            ytdown::Error::Unavailable {
                reason: ytdown::error::UnavailableReason::BotCheck,
                message: "Sign in to confirm you're not a bot: private-response-marker".into(),
            }
            .into(),
        )
        .to_string();
        assert!(error.contains("YouTube 网页 Cookie"));
        assert!(error.contains("网络/代理"));
        assert!(!error.contains("private-response-marker"));
        assert!(!error.contains("当前版本支持无需登录的公开视频"));
    }

    #[test]
    fn xiaohongshu_original_is_selected_before_higher_resolution_playback() {
        use crate::chinese_video::{NativeVariant, NativeVideo, XHS_ORIGINAL_FORMAT};
        let resolver = MediaResolver::new().unwrap();
        let video = NativeVideo {
            id: "note".into(),
            title: "小红书原始文件".into(),
            duration_seconds: None,
            variants: [("xhs-h264-0", 1080), (XHS_ORIGINAL_FORMAT, 0)]
                .into_iter()
                .map(|(id, height)| NativeVariant {
                    id: id.into(),
                    label: id.into(),
                    height,
                    url: format!("https://sns-video-bd.xhscdn.com/pre_post/{id}"),
                    size: None,
                    headers: vec![],
                    decrypt_key: None,
                })
                .collect(),
        };
        let resolved = normalize_native(
            "https://www.xiaohongshu.com/explore/note",
            "Xiaohongshu",
            video,
        )
        .unwrap();
        let preview = resolver.cache_resolved("Xiaohongshu", resolved).unwrap();
        assert_eq!(preview.formats[0].id, XHS_ORIGINAL_FORMAT);
        let plan = resolver
            .select(&preview.id, &preview.formats[0].id)
            .unwrap();
        assert_eq!(plan.assembly, Assembly::Direct);
        assert!(plan.streams[0].url.ends_with(XHS_ORIGINAL_FORMAT));
        assert!(!serde_json::to_string(&preview)
            .unwrap()
            .contains("xhscdn.com"));
    }

    #[test]
    fn qishui_audio_is_selected_with_a_music_filename_and_no_sharer_id() {
        let resolver = MediaResolver::new().unwrap();
        let resolved = normalize_qishui(crate::qishui::QishuiTrack {
            id: "7687463141597169715".into(),
            title: "测试歌手 - 测试曲目".into(),
            duration_seconds: Some(153),
            media_url: "https://v26-luna.douyinvod.com/audio?br=126&mime_type=audio_mp4".into(),
            extension: "m4a",
            bitrate_kbps: Some(126),
            size: Some(2533293),
            headers: vec![("Referer".into(), "https://music.douyin.com/".into())],
            preview: false,
        });
        let preview = resolver.cache_resolved("Qishui Music", resolved).unwrap();
        assert_eq!(preview.formats.len(), 1);
        assert_eq!(preview.formats[0].filename, "测试歌手 - 测试曲目.m4a");
        assert!(preview.formats[0].audio_only);
        let plan = resolver
            .select(&preview.id, &preview.formats[0].id)
            .unwrap();
        assert_eq!(plan.assembly, Assembly::Direct);
        assert_eq!(plan.extension, "m4a");
        assert!(!plan.source_url.contains("sec_sharer_id"));
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn routing_names_and_headers_reject_unsafe_inputs() {
        assert_eq!(platform("https://youtu.be/abcdefghijk"), Some("YouTube"));
        assert_eq!(platform("https://b23.tv/abc"), Some("Bilibili"));
        assert_eq!(platform("https://v.douyin.com/example"), Some("Douyin"));
        assert_eq!(
            platform("https://music.douyin.com/qishui/share/track?track_id=7687463141597169715"),
            Some("Qishui Music")
        );
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
            crate::validation::validate_headers(&[("Range".into(), "bytes=0-1".into())]).is_err()
        );
        assert!(crate::validation::validate_headers(&[(
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
        if request.uri().path() == "/cdn/stalled.m3u8" {
            tokio::time::sleep(Duration::from_secs(15)).await;
            return axum::http::StatusCode::GATEWAY_TIMEOUT.into_response();
        }
        if request.uri().path() == "/cdn/vod.m3u8" {
            return "#EXTM3U\n#EXTINF:1,\nvideo?sig=private\n#EXTINF:1,\naudio?sig=private\n#EXT-X-ENDLIST\n".into_response();
        }
        if request.uri().path() == "/cdn/login.mp4" {
            return (
                [("Content-Type", "text/html")],
                "<html>login required</html>",
            )
                .into_response();
        }
        if matches!(request.uri().path(), "/cdn/video" | "/cdn/audio") {
            let length = if request.uri().path() == "/cdn/video" {
                1234
            } else {
                123
            };
            let data = bytes::Bytes::from(vec![0x6b; length]);
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
    async fn new_platform_hls_is_verified_cached_and_requires_local_remuxing() {
        use crate::chinese_video::{NativeVariant, NativeVideo};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().fallback(platform_fixture))
                .await
                .unwrap();
        });
        let resolver = MediaResolver::with_ffmpeg(None).unwrap();
        let video = NativeVideo {
            id: "35457073".into(),
            title: "平台 HLS 回归".into(),
            duration_seconds: Some(2),
            variants: vec![
                NativeVariant {
                    id: "acfun-720".into(),
                    label: "720p · MP4".into(),
                    height: 720,
                    url: format!("{base}/cdn/vod.m3u8?sig=private"),
                    size: None,
                    headers: vec![("Referer".into(), "https://www.acfun.cn/".into())],
                    decrypt_key: None,
                },
                NativeVariant {
                    id: "broken-higher-format".into(),
                    label: "1080p · MP4".into(),
                    height: 1080,
                    url: format!("{base}/cdn/login.mp4"),
                    size: None,
                    headers: vec![],
                    decrypt_key: None,
                },
                NativeVariant {
                    id: "stalled-higher-format".into(),
                    label: "2160p · HLS".into(),
                    height: 2160,
                    url: format!("{base}/cdn/stalled.m3u8"),
                    size: None,
                    headers: vec![],
                    decrypt_key: None,
                },
            ],
        };
        let started = tokio::time::Instant::now();
        let resolved = resolver
            .normalize_platform("https://www.acfun.cn/v/ac35457073", "AcFun", video)
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(14),
            "a stalled quality must time out independently while usable HLS is retained"
        );
        assert_eq!(resolved.plans.len(), 1);
        assert_eq!(resolved.plans[0].2.assembly, Assembly::Hls);
        assert_eq!(resolved.plans[0].2.streams.len(), 2);
        assert!(resolved.plans[0].2.streams[0]
            .url
            .ends_with("/cdn/video?sig=private"));
        let preview = resolver.cache_resolved("AcFun", resolved).unwrap();
        assert!(preview.formats[0].needs_merge);
        assert!(!preview.ffmpeg_available);
        assert!(!serde_json::to_string(&preview).unwrap().contains("private"));
        assert!(resolver
            .select(&preview.id, &preview.formats[0].id)
            .unwrap_err()
            .to_string()
            .contains("FFmpeg"));
        server.abort();
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
