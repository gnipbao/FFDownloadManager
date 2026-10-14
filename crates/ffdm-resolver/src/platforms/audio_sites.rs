//! Guest playback adapters for individual Bandcamp and SoundCloud tracks.
//! Based on their public player data layouts, also documented by yt-dlp's
//! native extractors. Page scripts are inspected as text and never executed.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{anyhow, bail, ensure, Context, Result};
use futures_util::StreamExt;
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};
use tokio::{sync::Mutex, time::Instant};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_FORMATS: usize = 32;
const MAX_STREAM_REQUESTS: usize = 8;

pub(crate) struct AudioSitesResolver {
    client: Client,
    soundcloud_client_id: Mutex<Option<(String, Instant)>>,
}

impl AudioSitesResolver {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(6))
                .timeout(Duration::from_secs(12))
                .user_agent(crate::resource::USER_AGENT)
                .redirect(Policy::custom(|attempt| {
                    let downgrade = attempt.previous().last().is_some_and(|previous| {
                        previous.scheme() == "https" && attempt.url().scheme() == "http"
                    });
                    if attempt.previous().len() >= 5 || downgrade || !metadata_url(attempt.url()) {
                        attempt.error("audio metadata redirect rejected")
                    } else {
                        attempt.follow()
                    }
                }))
                .build()?,
            soundcloud_client_id: Mutex::new(None),
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        match platform {
            "Bandcamp" => self.bandcamp(input).await,
            "SoundCloud" => self.soundcloud(input).await,
            _ => bail!("未接入这个音频平台"),
        }
    }

    async fn fetch(&self, url: Url, platform: &str) -> Result<(Url, String)> {
        ensure!(metadata_url(&url), "音频播放信息不在平台官方域名内");
        let mut response = self
            .client
            .get(url)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|_| anyhow!("{platform} 公开播放信息请求失败"))?;
        match response.status().as_u16() {
            401 | 403 => bail!("{platform} 要求登录、访问验证或当前公开播放授权不可用"),
            404 | 410 => bail!("{platform} 作品不存在或已经删除"),
            429 => bail!("{platform} 请求过于频繁，请稍后再试"),
            status => ensure!(
                response.status().is_success(),
                "{platform} 返回 HTTP {status}"
            ),
        }
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_BYTES as u64,
            "音频播放信息超过 4 MiB 上限"
        );
        let final_url = response.url().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("读取 {platform} 播放信息失败"))?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
                "音频播放信息超过 4 MiB 上限"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok((
            final_url,
            String::from_utf8(bytes).context("音频播放信息不是 UTF-8 文本")?,
        ))
    }

    async fn bandcamp(&self, input: &Url) -> Result<GenericResource> {
        let expected = bandcamp_identity(input)?;
        let mut page = input.clone();
        page.set_scheme("https")
            .map_err(|_| anyhow!("Bandcamp 链接格式无效"))?;
        page.set_query(None);
        page.set_fragment(None);
        let (final_page, html) = self.fetch(page, "Bandcamp").await?;
        ensure!(
            bandcamp_identity(&final_page)? == expected,
            "Bandcamp 页面跳转到了其他作品"
        );
        parse_bandcamp(&html, &final_page)
    }

    async fn soundcloud(&self, input: &Url) -> Result<GenericResource> {
        let (user, slug) = soundcloud_identity(input)?;
        let page = Url::parse(&format!("https://soundcloud.com/{user}/{slug}"))?;
        let (final_page, html) = self.fetch(page, "SoundCloud").await?;
        ensure!(
            soundcloud_identity(&final_page)? == (user, slug),
            "SoundCloud 页面跳转到了其他作品"
        );
        let track = soundcloud_track(&html, &final_page)?;
        let transcodings = soundcloud_transcodings(&track.data, &track.id)?;
        ensure!(
            !transcodings.is_empty(),
            "SoundCloud 未提供完整的公开 MP3 或 AAC 播放格式；预览、加密及未知编码不支持"
        );
        let client_id = self.client_id(&html).await?;
        let authorization = text(&track.data["track_authorization"]);
        let pending = futures_util::stream::iter(transcodings.into_iter().map(|transcoding| {
            let client_id = &client_id;
            let page = &final_page;
            async move {
                let mut endpoint = transcoding.endpoint.clone();
                {
                    let has_client = endpoint.query_pairs().any(|(name, _)| name == "client_id");
                    let has_authorization = endpoint
                        .query_pairs()
                        .any(|(name, _)| name == "track_authorization");
                    let mut query = endpoint.query_pairs_mut();
                    if !has_client {
                        query.append_pair("client_id", client_id);
                    }
                    if let Some(value) = authorization.filter(|_| !has_authorization) {
                        query.append_pair("track_authorization", value);
                    }
                }
                let (_, body) = self.fetch(endpoint, "SoundCloud").await?;
                let data: Value =
                    serde_json::from_str(&body).context("SoundCloud 未返回公开音频播放地址")?;
                resolved_soundcloud(&data, &transcoding, page)
            }
        }))
        .buffer_unordered(2);
        tokio::pin!(pending);
        let deadline = Instant::now() + Duration::from_secs(16);
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        let mut last_error = None;
        while let Ok(Some(result)) = tokio::time::timeout_at(deadline, pending.next()).await {
            match result {
                Ok(candidate) if seen.insert(candidate.url.clone()) => candidates.push(candidate),
                Ok(_) => {}
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        ensure!(
            !candidates.is_empty(),
            "SoundCloud 公开音频暂不可访问：{}",
            last_error
                .as_deref()
                .unwrap_or("播放信息请求超时，请稍后重试")
        );
        Ok(GenericResource {
            id: track.id,
            title: track.title,
            duration_seconds: Some(track.duration_ms.div_ceil(1000)),
            candidates,
            embed: None,
        })
    }

    async fn client_id(&self, html: &str) -> Result<String> {
        let mut cached = self.soundcloud_client_id.lock().await;
        if let Some((id, cached_at)) = &*cached {
            if cached_at.elapsed() < Duration::from_secs(1800) {
                return Ok(id.clone());
            }
        }
        let assets = soundcloud_assets(html)?;
        for asset in assets.into_iter().rev().take(3) {
            let result =
                tokio::time::timeout(Duration::from_secs(6), self.fetch(asset, "SoundCloud")).await;
            if let Ok(Ok((_, body))) = result {
                if let Some(id) = public_client_id(&body)? {
                    *cached = Some((id.clone(), Instant::now()));
                    return Ok(id);
                }
            }
        }
        bail!("SoundCloud 页面未提供当前公开播放器标识；请稍后重试")
    }
}

fn metadata_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.host_str().is_some_and(|host| {
            matches!(
                host,
                "soundcloud.com"
                    | "www.soundcloud.com"
                    | "m.soundcloud.com"
                    | "api-v2.soundcloud.com"
                    | "api.soundcloud.com"
                    | "a-v2.sndcdn.com"
            ) || bandcamp_artist(host).is_some()
        })
}

fn bandcamp_artist(host: &str) -> Option<&str> {
    let artist = host.strip_suffix(".bandcamp.com")?;
    (!artist.is_empty()
        && artist.len() <= 63
        && !matches!(artist, "www" | "daily")
        && artist
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
    .then_some(artist)
}

fn bandcamp_identity(input: &Url) -> Result<(String, String)> {
    let host = input.host_str().context("Bandcamp 链接缺少域名")?;
    ensure!(
        bandcamp_artist(host).is_some(),
        "请输入艺术家 Bandcamp 单曲链接"
    );
    let path = input.path().trim_end_matches('/');
    let slug = path
        .strip_prefix("/track/")
        .context("Bandcamp 目前支持 /track/ 单曲；专辑与合集不支持")?;
    ensure!(
        !slug.is_empty() && slug.len() <= 400 && !slug.contains('/'),
        "请输入 Bandcamp 单曲链接"
    );
    let decoded = percent_decode_str(slug).decode_utf8()?;
    ensure!(
        !decoded.contains('/') && !decoded.chars().any(char::is_control),
        "Bandcamp 单曲标识不合法"
    );
    Ok((host.to_owned(), slug.to_owned()))
}

fn soundcloud_identity(input: &Url) -> Result<(String, String)> {
    ensure!(
        matches!(
            input.host_str(),
            Some("soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com")
        ),
        "请输入 SoundCloud 公开单曲页面"
    );
    ensure!(
        !input
            .query_pairs()
            .any(|(key, _)| key == "secret_token" || key == "oauth_token"),
        "SoundCloud 私密或登录授权链接暂不支持"
    );
    let pieces: Vec<_> = input.path().trim_matches('/').split('/').collect();
    let [user, slug] = pieces.as_slice() else {
        bail!("SoundCloud 目前支持 /用户/单曲；合集和私密链接不支持")
    };
    let valid = |value: &str| {
        !value.is_empty()
            && value.len() <= 200
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    };
    ensure!(
        valid(user)
            && valid(slug)
            && !matches!(
                *user,
                "search" | "discover" | "you" | "stations" | "settings"
            )
            && !matches!(
                *slug,
                "tracks" | "sets" | "albums" | "reposts" | "likes" | "spotlight" | "comments"
            ),
        "请输入 SoundCloud 公开单曲链接"
    );
    Ok(((*user).to_owned(), (*slug).to_owned()))
}

fn parse_bandcamp(html: &str, page: &Url) -> Result<GenericResource> {
    ensure!(html.len() <= MAX_BYTES, "Bandcamp 页面超过解析上限");
    let requested = bandcamp_identity(page)?;
    let attribute = Regex::new(r#"(?is)\bdata-tralbum\s*=\s*(?:"([^"]*)"|'([^']*)')"#)?;
    let matches: Vec<_> = attribute.captures_iter(html).take(2).collect();
    ensure!(
        matches.len() == 1,
        "Bandcamp 页面没有唯一的公开单曲播放器；可能需要浏览器验证"
    );
    let json = matches[0]
        .get(1)
        .or_else(|| matches[0].get(2))
        .unwrap()
        .as_str();
    let data: Value =
        serde_json::from_str(&decode_entities(json)).context("Bandcamp 单曲播放信息格式已变化")?;
    ensure!(
        text(&data["current"]["type"]) == Some("track")
            && text(&data["item_type"]).is_none_or(|kind| kind == "track"),
        "Bandcamp 返回的是专辑，不能将多首歌当作同一作品"
    );
    ensure!(
        !flag(&data["is_private_stream"])
            && !flag(&data["tralbum_subscriber_only"])
            && !flag(&data["current"]["private"])
            && !flag(&data["current"]["killed"]),
        "Bandcamp 该曲目未公开或要求订阅授权"
    );
    if let Some(raw) = text(&data["url"]) {
        let canonical = page.join(raw)?;
        ensure!(
            bandcamp_identity(&canonical)? == requested,
            "Bandcamp 返回了其他单曲的信息"
        );
    }
    let current_id = number_id(&data["current"]["id"])
        .or_else(|| number_id(&data["id"]))
        .context("Bandcamp 单曲缺少作品标识")?;
    if let Some(id) = number_id(&data["id"]) {
        ensure!(id == current_id, "Bandcamp 单曲标识不一致");
    }
    let tracks = data["trackinfo"]
        .as_array()
        .context("Bandcamp 没有公开音轨信息")?;
    ensure!(tracks.len() <= 128, "Bandcamp 页面包含过多音轨");
    let matching: Vec<_> = tracks
        .iter()
        .filter(|track| {
            number_id(&track["track_id"])
                .or_else(|| number_id(&track["id"]))
                .as_deref()
                == Some(current_id.as_str())
        })
        .collect();
    ensure!(matching.len() == 1, "Bandcamp 没有唯一对应的目标音轨");
    let track = matching[0];
    for value in [&track["id"], &track["track_id"]] {
        if let Some(id) = number_id(value) {
            ensure!(id == current_id, "Bandcamp 目标音轨的标识不一致");
        }
    }
    ensure!(
        !flag(&track["private"])
            && !flag(&track["is_draft"])
            && !flag(&track["is_capped"])
            && !flag(&track["is_preview"])
            && !flag(&track["preview_only"]),
        "Bandcamp 目标音轨未公开、仅提供预览或已达到播放限制"
    );
    if let Some(raw) = text(&track["title_link"]) {
        ensure!(
            bandcamp_identity(&page.join(raw)?)? == requested,
            "Bandcamp 音轨与请求的单曲不一致"
        );
    }
    let title = text(&track["title"]).context("Bandcamp 目标音轨缺少标题")?;
    let duration = finite_number(&track["duration"])
        .filter(|value| *value > 0.0)
        .context("Bandcamp 未公开完整音轨时长")?;
    let files = track["file"]
        .as_object()
        .context("Bandcamp 未提供可公开读取的完整音频")?;
    ensure!(
        files.len() <= MAX_FORMATS,
        "Bandcamp 单曲格式超过 32 个上限"
    );
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for (format, value) in files {
        let Some(raw) = text(value) else { continue };
        let Some((extension, bitrate)) = bandcamp_encoding(format) else {
            continue;
        };
        let url = page.join(raw)?;
        if !audio_cdn(&url, "Bandcamp") || preview_url(&url) || !seen.insert(url.to_string()) {
            continue;
        }
        candidates.push(GenericCandidate {
            id: format!("bandcamp-{current_id}-{format}"),
            label: bitrate.map_or_else(
                || extension.to_ascii_uppercase(),
                |rate| format!("{} · {rate} kbps", extension.to_ascii_uppercase()),
            ),
            height: 0,
            url: url.to_string(),
            extension: extension.to_owned(),
            audio_only: true,
            headers: media_headers(page),
            size: None,
        });
    }
    ensure!(
        !candidates.is_empty(),
        "Bandcamp 未提供受支持的完整公开音频格式"
    );
    Ok(GenericResource {
        id: current_id,
        title: text(&data["artist"])
            .map_or_else(|| title.to_owned(), |artist| format!("{artist} - {title}")),
        duration_seconds: Some(duration.ceil() as u64),
        candidates,
        embed: None,
    })
}

fn bandcamp_encoding(format: &str) -> Option<(&'static str, Option<u32>)> {
    if format.len() > 64
        || !format
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return None;
    }
    let (encoding, bitrate) = format.split_once('-').unwrap_or((format, ""));
    let extension = match encoding {
        "mp3" => "mp3",
        "aac" | "m4a" => "m4a",
        "flac" => "flac",
        "wav" => "wav",
        "opus" => "opus",
        "ogg" | "vorbis" => "ogg",
        _ => return None,
    };
    Some((extension, bitrate.parse().ok()))
}

struct SoundcloudTrack {
    id: String,
    title: String,
    duration_ms: u64,
    data: Value,
}

struct Transcoding {
    id: String,
    label: String,
    extension: &'static str,
    endpoint: Url,
}

fn soundcloud_track(html: &str, page: &Url) -> Result<SoundcloudTrack> {
    ensure!(html.len() <= MAX_BYTES, "SoundCloud 页面超过解析上限");
    let expected = soundcloud_identity(page)?;
    let assignment = Regex::new(r#"(?:window\.)?__sc_hydration\s*=\s*"#)?;
    let captures: Vec<_> = assignment.find_iter(html).take(2).collect();
    ensure!(captures.len() == 1, "SoundCloud 页面没有唯一的公开单曲信息");
    let raw = balanced_json(&html[captures[0].end()..])?;
    let data: Value = serde_json::from_str(raw).context("SoundCloud 单曲信息格式已变化")?;
    let entries = data
        .as_array()
        .context("SoundCloud 单曲信息不是公开播放器列表")?;
    ensure!(entries.len() <= 128, "SoundCloud 页面包含过多播放对象");
    let selected: Vec<_> = entries
        .iter()
        .filter(|entry| {
            text(&entry["hydratable"]) == Some("sound")
                && text(&entry["data"]["permalink_url"])
                    .and_then(|raw| Url::parse(raw).ok())
                    .and_then(|url| soundcloud_identity(&url).ok())
                    .as_ref()
                    == Some(&expected)
        })
        .collect();
    ensure!(
        selected.len() == 1,
        "SoundCloud 没有唯一对应的目标单曲，推荐曲目不会自动下载"
    );
    let track = &selected[0]["data"];
    ensure!(
        text(&track["kind"]) == Some("track") && text(&track["sharing"]) == Some("public"),
        "SoundCloud 私密曲目或合集暂不支持"
    );
    ensure!(
        text(&track["policy"]) == Some("ALLOW")
            && !flag(&track["is_preview"])
            && !flag(&track["snipped"])
            && !flag(&track["drm"]),
        "SoundCloud 该单曲受访问限制、仅提供预览或使用加密播放"
    );
    ensure!(
        text(&track["state"]).is_none_or(|state| state == "finished"),
        "SoundCloud 目标单曲尚未完成处理"
    );
    let id = number_id(&track["id"]).context("SoundCloud 目标单曲缺少标识")?;
    let duration_ms = integer(&track["duration"])
        .filter(|value| *value > 0 && *value <= 604800000)
        .context("SoundCloud 未公开完整曲目时长")?;
    let full_duration = integer(&track["full_duration"]).unwrap_or(duration_ms);
    ensure!(
        full_duration > 0 && duration_ms.saturating_add(1000) >= full_duration,
        "SoundCloud 当前仅开放试听片段，不能作为完整曲目下载"
    );
    Ok(SoundcloudTrack {
        id,
        title: text(&track["title"])
            .context("SoundCloud 单曲缺少标题")?
            .to_owned(),
        duration_ms,
        data: track.clone(),
    })
}

fn soundcloud_transcodings(track: &Value, track_id: &str) -> Result<Vec<Transcoding>> {
    let transcodings = track["media"]["transcodings"]
        .as_array()
        .context("SoundCloud 未公开音频格式")?;
    ensure!(
        transcodings.len() <= MAX_FORMATS,
        "SoundCloud 音频格式超过 32 个上限"
    );
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for (index, transcoding) in transcodings.iter().enumerate() {
        if found.len() >= MAX_STREAM_REQUESTS {
            break;
        }
        if flag(&transcoding["snipped"])
            || flag(&transcoding["is_preview"])
            || flag(&transcoding["drm"])
        {
            continue;
        }
        let Some(raw) = text(&transcoding["url"]) else {
            continue;
        };
        let Ok(endpoint) = crate::validation::parse_url(raw) else {
            continue;
        };
        let decoded_path = percent_decode_str(endpoint.path()).decode_utf8()?;
        if !matches!(
            endpoint.host_str(),
            Some("api-v2.soundcloud.com" | "api.soundcloud.com")
        ) || endpoint.scheme() != "https"
            || !decoded_path.starts_with(&format!("/media/soundcloud:tracks:{track_id}/"))
            || preview_url(&endpoint)
            || decoded_path.to_ascii_lowercase().contains("encrypted")
        {
            continue;
        }
        let protocol = text(&transcoding["format"]["protocol"]).unwrap_or("");
        let preset = text(&transcoding["preset"]).unwrap_or("");
        if preset.len() > 64
            || !preset
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            continue;
        }
        let mime = text(&transcoding["format"]["mime_type"])
            .unwrap_or("")
            .to_ascii_lowercase();
        let base_mime = mime.split(';').next().unwrap_or("").trim();
        let extension = match protocol {
            "progressive" if !decoded_path.contains("/hls") => match base_mime {
                "audio/mpeg" if preset.starts_with("mp3") => "mp3",
                "audio/mp4" if preset.starts_with("aac") || preset.starts_with("m4a") => "m4a",
                "audio/ogg" if preset.starts_with("opus") => "opus",
                _ => continue,
            },
            "hls"
                if preset.starts_with("aac_")
                    && ((base_mime == "audio/mp4" && mime.contains("mp4a."))
                        || base_mime == "audio/aac") =>
            {
                "m3u8"
            }
            _ => continue,
        };
        if preset.to_ascii_lowercase().contains("preview") || !seen.insert(endpoint.to_string()) {
            continue;
        }
        if let Some(length) = integer(&transcoding["duration"]) {
            if integer(&track["duration"]).is_some_and(|full| length.saturating_add(1000) < full) {
                continue;
            }
        }
        found.push(Transcoding {
            id: format!("soundcloud-{track_id}-{index}"),
            label: if extension == "m3u8" {
                format!("AAC · HLS · {preset}")
            } else {
                format!("{} · {preset}", extension.to_ascii_uppercase())
            },
            extension,
            endpoint,
        });
    }
    Ok(found)
}

fn resolved_soundcloud(
    data: &Value,
    transcoding: &Transcoding,
    page: &Url,
) -> Result<GenericCandidate> {
    let raw = text(&data["url"]).context("SoundCloud 没有公开该格式的播放地址")?;
    let url = crate::validation::parse_url(raw)?;
    let decoded_path = percent_decode_str(url.path())
        .decode_utf8_lossy()
        .to_ascii_lowercase();
    ensure!(
        audio_cdn(&url, "SoundCloud") && !preview_url(&url) && !decoded_path.contains("encrypted"),
        "SoundCloud 返回的地址不是完整公开音频"
    );
    let manifest = decoded_path.ends_with(".m3u8") || decoded_path.contains("/playlist/");
    ensure!(
        transcoding.extension == "m3u8" || !manifest,
        "SoundCloud 该格式实际是流媒体清单，不能当作音频文件"
    );
    Ok(GenericCandidate {
        id: transcoding.id.clone(),
        label: transcoding.label.clone(),
        height: 0,
        url: url.to_string(),
        extension: transcoding.extension.to_owned(),
        audio_only: true,
        headers: media_headers(page),
        size: None,
    })
}

fn soundcloud_assets(html: &str) -> Result<Vec<Url>> {
    let scripts = Regex::new(r#"(?is)<script\b[^>]*\bsrc\s*=\s*["']([^"']+)["']"#)?;
    let mut seen = HashSet::new();
    Ok(scripts
        .captures_iter(html)
        .filter_map(|capture| {
            let url = Url::parse(&decode_entities(&capture[1])).ok()?;
            (url.scheme() == "https"
                && url.host_str() == Some("a-v2.sndcdn.com")
                && url.path().starts_with("/assets/")
                && url.path().ends_with(".js")
                && url.username().is_empty()
                && url.password().is_none()
                && seen.insert(url.to_string()))
            .then_some(url)
        })
        .take(32)
        .collect())
}

fn public_client_id(script: &str) -> Result<Option<String>> {
    ensure!(script.len() <= MAX_BYTES, "SoundCloud 播放脚本超过解析上限");
    let regex = Regex::new(r#"\b(?:client_id|clientId)\s*[:=]\s*["']([a-zA-Z0-9]{32})["']"#)?;
    let ids: HashSet<_> = regex
        .captures_iter(script)
        .map(|capture| capture[1].to_owned())
        .collect();
    ensure!(ids.len() <= 1, "SoundCloud 播放脚本存在多个不同标识");
    Ok(ids.into_iter().next())
}

fn audio_cdn(url: &Url, platform: &str) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| match platform {
            "Bandcamp" => host == "bcbits.com" || host.ends_with(".bcbits.com"),
            "SoundCloud" => {
                host == "sndcdn.com"
                    || host.ends_with(".sndcdn.com")
                    || host == "playback.media-streaming.soundcloud.cloud"
            }
            _ => false,
        })
}

fn preview_url(url: &Url) -> bool {
    let path = percent_decode_str(url.path())
        .decode_utf8_lossy()
        .to_ascii_lowercase();
    path.contains("/preview/")
        || path.contains("/playlist/0/30/")
        || url
            .query_pairs()
            .any(|(key, value)| key == "preview" && matches!(value.as_ref(), "1" | "true"))
}

fn media_headers(page: &Url) -> Vec<(String, String)> {
    let mut referer = page.clone();
    referer.set_query(None);
    referer.set_fragment(None);
    vec![
        (
            "User-Agent".to_owned(),
            crate::resource::USER_AGENT.to_owned(),
        ),
        ("Referer".to_owned(), referer.to_string()),
    ]
}

fn text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
fn integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}
fn number_id(value: &Value) -> Option<String> {
    integer(value)
        .filter(|number| *number > 0)
        .map(|number| number.to_string())
}
fn finite_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|number| number.is_finite() && (0.0..=604800.0).contains(number))
}
fn flag(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_u64().is_none_or(|value| value != 0),
        Value::String(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false"
        ),
        Value::Object(_) | Value::Array(_) => true,
    }
}

fn balanced_json(raw: &str) -> Result<&str> {
    let raw = raw.trim_start();
    ensure!(raw.starts_with('['), "SoundCloud 播放器信息不是静态 JSON");
    let mut depth = 0u32;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in raw.bytes().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    ensure!(depth <= 64, "SoundCloud 播放信息嵌套过深");
                }
                b']' | b'}' => {
                    depth = depth.checked_sub(1).context("SoundCloud JSON 括号不匹配")?;
                    if depth == 0 {
                        return Ok(&raw[..=index]);
                    }
                }
                _ => {}
            }
        }
    }
    bail!("SoundCloud 播放信息不完整")
}

fn decode_entities(raw: &str) -> String {
    let regex = Regex::new(r"&(?:quot|apos|amp|lt|gt|#\d+|#x[0-9a-fA-F]+);").unwrap();
    regex
        .replace_all(raw, |capture: &regex::Captures<'_>| {
            let entity = &capture[0];
            match entity {
                "&quot;" => "\"".to_owned(),
                "&apos;" => "'".to_owned(),
                "&amp;" => "&".to_owned(),
                "&lt;" => "<".to_owned(),
                "&gt;" => ">".to_owned(),
                _ => {
                    let body = &entity[2..entity.len() - 1];
                    let number = body
                        .strip_prefix('x')
                        .and_then(|number| u32::from_str_radix(number, 16).ok())
                        .or_else(|| body.parse().ok());
                    number
                        .and_then(char::from_u32)
                        .map_or_else(|| entity.to_owned(), |character| character.to_string())
                }
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bandcamp_html(data: &Value) -> String {
        format!(
            "<script data-tralbum=\"{}\"></script>",
            data.to_string()
                .replace('&', "&amp;")
                .replace('"', "&quot;")
        )
    }
    fn bandcamp_data() -> Value {
        json!({"id":123,"item_type":"track","artist":"An artist","url":"https://artist.bandcamp.com/track/song","current":{"type":"track","id":123},"trackinfo":[{"id":123,"title":"A \"song\" & 作品","title_link":"/track/song","duration":9.8,"file":{"mp3-128":"https://t4.bcbits.com/stream/track/mp3-128?sig=a%2Bb&x=1"}}]})
    }
    fn soundcloud_data() -> Value {
        json!({"id":123,"kind":"track","sharing":"public","policy":"ALLOW","state":"finished","title":"A track","permalink_url":"https://soundcloud.com/user/song","duration":60000,"full_duration":60000,"media":{"transcodings":[
            {"preset":"mp3_128","snipped":false,"url":"https://api-v2.soundcloud.com/media/soundcloud:tracks:123/hash/stream/progressive","format":{"protocol":"progressive","mime_type":"audio/mpeg"}},
            {"preset":"aac_96k","snipped":false,"url":"https://api-v2.soundcloud.com/media/soundcloud:tracks:123/hash/stream/hls","format":{"protocol":"hls","mime_type":"audio/mp4; codecs=\"mp4a.40.2\""}},
            {"preset":"abr_sq","url":"https://api-v2.soundcloud.com/media/soundcloud:tracks:123/hash/stream/abr-hls","format":{"protocol":"hls","mime_type":"audio/mpegurl"}}
        ]}})
    }
    fn soundcloud_html(data: &Value) -> String {
        format!(
            "<script>window.__sc_hydration = {};</script>",
            json!([{"hydratable":"sound","data":data}])
        )
    }

    #[test]
    fn bandcamp_matches_single_track_and_preserves_signed_mp3_and_title() {
        let data = bandcamp_data();
        let page = Url::parse("https://artist.bandcamp.com/track/song").unwrap();
        let resource = parse_bandcamp(&bandcamp_html(&data), &page).unwrap();
        assert_eq!(resource.id, "123");
        assert_eq!(resource.title, "An artist - A \"song\" & 作品");
        assert_eq!(resource.duration_seconds, Some(10));
        assert_eq!(resource.candidates.len(), 1);
        assert!(resource.candidates[0].audio_only);
        assert_eq!(resource.candidates[0].extension, "mp3");
        assert!(resource.candidates[0].url.ends_with("?sig=a%2Bb&x=1"));
        let mut album = data.clone();
        album["current"]["type"] = json!("album");
        assert!(parse_bandcamp(&bandcamp_html(&album), &page).is_err());
        let mut wrong = data.clone();
        wrong["trackinfo"][0]["id"] = json!(999);
        assert!(parse_bandcamp(&bandcamp_html(&wrong), &page).is_err());
        let mut private = data.clone();
        private["trackinfo"][0]["is_capped"] = json!(true);
        assert!(parse_bandcamp(&bandcamp_html(&private), &page).is_err());
    }

    #[test]
    fn bandcamp_does_not_select_album_neighbors_or_external_download_services() {
        let page = Url::parse("https://artist.bandcamp.com/track/song").unwrap();
        let mut data = bandcamp_data();
        data["trackinfo"].as_array_mut().unwrap().insert(0,json!({"id":999,"title":"Other song","duration":12,"file":{"mp3-128":"https://t4.bcbits.com/other.mp3"}}));
        assert_eq!(
            parse_bandcamp(&bandcamp_html(&data), &page)
                .unwrap()
                .candidates
                .len(),
            1
        );
        data["trackinfo"][1]["file"]["mp3-128"] = json!("https://t4.bcbits.com.evil.test/song.mp3");
        assert!(parse_bandcamp(&bandcamp_html(&data), &page).is_err());
        assert!(
            bandcamp_identity(&Url::parse("https://artist.bandcamp.com/album/song").unwrap())
                .is_err()
        );
        assert!(
            bandcamp_identity(&Url::parse("https://www.bandcamp.com/track/song").unwrap()).is_err()
        );
    }

    #[test]
    fn source_query_and_fragment_are_not_forwarded_but_media_signatures_are_preserved() {
        let page =
            Url::parse("https://artist.bandcamp.com/track/song?session=private#player").unwrap();
        let resource = parse_bandcamp(&bandcamp_html(&bandcamp_data()), &page).unwrap();
        let candidate = &resource.candidates[0];
        assert_eq!(
            candidate.headers[1],
            (
                "Referer".to_owned(),
                "https://artist.bandcamp.com/track/song".to_owned()
            )
        );
        assert!(candidate.url.ends_with("?sig=a%2Bb&x=1"));
        assert!(candidate
            .headers
            .iter()
            .all(|(_, value)| !value.contains("private") && !value.contains('#')));
        crate::hls::validate_headers(&candidate.headers).unwrap();
        let soundcloud_page =
            Url::parse("https://soundcloud.com/user/song?utm_source=share#t=12").unwrap();
        assert_eq!(
            media_headers(&soundcloud_page)[1].1,
            "https://soundcloud.com/user/song"
        );
    }

    #[test]
    fn soundcloud_hydration_selects_target_and_rejects_private_or_short_previews() {
        let page = Url::parse("https://soundcloud.com/user/song").unwrap();
        let data = soundcloud_data();
        let track = soundcloud_track(&soundcloud_html(&data), &page).unwrap();
        assert_eq!(track.id, "123");
        let mut wrong = data.clone();
        wrong["permalink_url"] = json!("https://soundcloud.com/user/other");
        assert!(soundcloud_track(&soundcloud_html(&wrong), &page).is_err());
        let mut preview = data.clone();
        preview["duration"] = json!(30000);
        assert!(soundcloud_track(&soundcloud_html(&preview), &page).is_err());
        let mut private = data.clone();
        private["sharing"] = json!("private");
        assert!(soundcloud_track(&soundcloud_html(&private), &page).is_err());
        assert!(soundcloud_identity(
            &Url::parse("https://soundcloud.com/user/song/s-secret").unwrap()
        )
        .is_err());
        assert!(
            soundcloud_identity(&Url::parse("https://soundcloud.com/user/sets").unwrap()).is_err()
        );
    }

    #[test]
    fn soundcloud_only_full_progressive_or_explicit_aac_hls_is_exposed() {
        let mut data = soundcloud_data();
        let formats = soundcloud_transcodings(&data, "123").unwrap();
        assert_eq!(formats.len(), 2);
        assert_eq!(formats[0].extension, "mp3");
        assert_eq!(formats[1].extension, "m3u8");
        assert!(soundcloud_transcodings(&data, "999").unwrap().is_empty());
        data["media"]["transcodings"][0]["snipped"] = json!(true);
        data["media"]["transcodings"][1]["format"]["protocol"] = json!("cbc-hls");
        assert!(soundcloud_transcodings(&data, "123").unwrap().is_empty());
        let page = Url::parse("https://soundcloud.com/user/song").unwrap();
        let candidate = resolved_soundcloud(
            &json!({"url":"https://cf-hls-media.sndcdn.com/playlist/full.m3u8?sig=a%2Bb"}),
            &formats[1],
            &page,
        )
        .unwrap();
        assert!(candidate.audio_only);
        assert_eq!(candidate.extension, "m3u8");
        assert!(candidate.url.ends_with("?sig=a%2Bb"));
        assert!(resolved_soundcloud(
            &json!({"url":"https://playback.media-streaming.soundcloud.cloud/playlist/full.m3u8?sig=opaque"}),
            &formats[1],
            &page,
        ).is_ok());
        assert!(resolved_soundcloud(
            &json!({"url":"https://cf-hls-media.sndcdn.com/playlist/0/30/preview.m3u8"}),
            &formats[1],
            &page
        )
        .is_err());
        assert!(resolved_soundcloud(
            &json!({"url":"https://cf-hls-media.sndcdn.com/playlist/full.m3u8"}),
            &formats[0],
            &page
        )
        .is_err());
    }

    #[test]
    fn public_client_id_is_read_without_script_execution_and_only_from_official_assets() {
        let id = "0123456789abcdefghijklmnopqrstuv";
        assert_eq!(
            public_client_id(&format!("const config={{client_id:\"{id}\"}};"))
                .unwrap()
                .as_deref(),
            Some(id)
        );
        let assets=soundcloud_assets(r#"<script src="https://a-v2.sndcdn.com/assets/player.js"></script><script src="https://evil.test/player.js"></script><script src="https://a-v2.sndcdn.com.evil.test/assets/player.js"></script>"#).unwrap();
        assert_eq!(assets.len(), 1);
        assert!(balanced_json("evil()").is_err());
        assert!(serde_json::from_str::<Value>(
            balanced_json("[{\"data\":fetch(\"evil\")}];").unwrap()
        )
        .is_err());
        assert_eq!(
            balanced_json(r#"[{"title":"bracket ] and escaped \" quote"}];alert('never run')"#)
                .unwrap(),
            r#"[{"title":"bracket ] and escaped \" quote"}]"#
        );
    }

    #[tokio::test]
    #[ignore = "reads public guest Bandcamp and SoundCloud playback metadata"]
    async fn live_public_audio_metadata() {
        let resolver = AudioSitesResolver::new().unwrap();
        for (platform, raw) in [
            (
                "Bandcamp",
                "https://youtube-dl.bandcamp.com/track/youtube-dl-test-song",
            ),
            (
                "SoundCloud",
                "https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy",
            ),
        ] {
            let resource = resolver
                .resolve(&Url::parse(raw).unwrap(), platform)
                .await
                .unwrap();
            assert!(!resource.candidates.is_empty());
            assert!(resource.candidates.iter().all(|format| format.audio_only));
            println!(
                "{platform}: {} public full-audio format(s), {} s",
                resource.candidates.len(),
                resource.duration_seconds.unwrap_or(0)
            );
        }
    }
}
