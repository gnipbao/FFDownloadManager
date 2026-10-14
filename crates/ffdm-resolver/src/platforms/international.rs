//! Public player metadata adapters. Protocol references are the yt-dlp
//! extractors for Vimeo, Dailymotion, Streamable and Imgur; extraction and
//! bounded HTTP requests are implemented here in Rust.
use crate::chinese_video::{NativeVariant, NativeVideo};
use anyhow::{bail, ensure, Context, Result};
use regex::Regex;
use reqwest_media::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use std::{collections::HashSet, time::Duration};

const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;

pub struct InternationalResolver {
    client: Client,
}

impl InternationalResolver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .use_native_tls()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .user_agent(USER_AGENT)
                .redirect(Policy::custom(|attempt| {
                    if attempt.previous().len() >= 5 {
                        attempt.error("too many public player redirects")
                    } else if !allowed_fetch_url(attempt.url()) {
                        attempt.error("public player redirected outside its official domains")
                    } else {
                        attempt.follow()
                    }
                }))
                .build()?,
        })
    }

    pub async fn resolve(&self, url: &Url, platform: &str) -> Result<NativeVideo> {
        match platform {
            "Vimeo" => self.vimeo(url).await,
            "Dailymotion" => self.dailymotion(url).await,
            "Streamable" => self.streamable(url).await,
            "Imgur" => self.imgur(url).await,
            _ => bail!("不支持的视频平台"),
        }
    }

    async fn fetch(&self, url: Url, platform: &str, referer: &str) -> Result<(Url, String)> {
        ensure!(allowed_fetch_url(&url), "{platform} 播放地址不在官方域名内");
        let mut response = self
            .client
            .get(url)
            .header(header::ACCEPT, "*/*")
            .header(header::REFERER, referer)
            .send()
            .await
            .with_context(|| format!("{platform} 公开播放接口连接失败"))?;
        let status = response.status();
        match status.as_u16() {
            401 | 403 => bail!("{platform} 要求登录、访问验证或受地区限制"),
            404 | 410 => bail!("{platform} 作品不存在或已经删除"),
            429 => bail!("{platform} 请求过于频繁，请稍后重试"),
            _ => ensure!(status.is_success(), "{platform} 播放接口返回 HTTP {status}"),
        }
        ensure!(
            allowed_fetch_url(response.url()),
            "{platform} 返回了非官方播放接口"
        );
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_METADATA_BYTES as u64,
            "{platform} 播放信息超过大小限制"
        );
        let final_url = response.url().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_METADATA_BYTES,
                "{platform} 播放信息超过大小限制"
            );
            bytes.extend_from_slice(&chunk);
        }
        let text = String::from_utf8(bytes).context("公开播放信息不是有效的 UTF-8")?;
        Ok((final_url, text))
    }

    async fn fetch_json(&self, url: Url, platform: &str, referer: &str) -> Result<Value> {
        let (_, text) = self.fetch(url, platform, referer).await?;
        serde_json::from_str(&text)
            .with_context(|| format!("{platform} 没有返回有效播放信息；可能触发了访问验证"))
    }

    async fn streamable(&self, input: &Url) -> Result<NativeVideo> {
        let id = streamable_id(input).context("请输入 Streamable 公开单视频链接")?;
        let data = self
            .fetch_json(
                Url::parse(&format!("https://ajax.streamable.com/videos/{id}"))?,
                "Streamable",
                "https://streamable.com/",
            )
            .await?;
        parse_streamable(&data, &id)
    }

    async fn imgur(&self, input: &Url) -> Result<NativeVideo> {
        let id =
            imgur_id(input).context("请输入 Imgur 单个视频或 gifv 链接；相册与画廊暂不支持")?;
        // This is the public site's media metadata endpoint, not an account API.
        let api = Url::parse(&format!("https://api.imgur.com/post/v1/media/{id}?client_id=546c25a59c58ad7&include=media,account"))?;
        let data = self.fetch_json(api, "Imgur", "https://imgur.com/").await?;
        let mut video = parse_imgur(&data, &id)?;
        if video.variants.is_empty() {
            // Animated GIF metadata may point to the original GIF; use only a
            // source advertised by that same item's gifv player, never guess a
            // replacement extension or scan unrelated page recommendations.
            let (_, html) = self
                .fetch(
                    Url::parse(&format!("https://i.imgur.com/{id}.gifv"))?,
                    "Imgur",
                    "https://imgur.com/",
                )
                .await?;
            video.variants = imgur_player_sources(&html, &id)?;
            let original_height = data["media"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|media| nonempty(&media["id"]) == Some(id.as_str()))
                .map(|media| height(&media["height"]))
                .unwrap_or(0);
            if original_height > 0 {
                for variant in &mut video.variants {
                    variant.height = original_height;
                    variant.label = format!("{original_height}p · MP4");
                }
            }
        }
        ensure!(
            !video.variants.is_empty(),
            "Imgur 单作品页面没有公开的 MP4 播放资源"
        );
        Ok(video)
    }

    async fn dailymotion(&self, input: &Url) -> Result<NativeVideo> {
        let id = dailymotion_id(input).context("请输入 Dailymotion 公开单视频链接")?;
        let metadata = Url::parse(&format!(
            "https://www.dailymotion.com/player/metadata/video/{id}?app=com.dailymotion.neon"
        ))?;
        let data = self
            .fetch_json(metadata, "Dailymotion", "https://www.dailymotion.com/")
            .await?;
        let mut video = parse_dailymotion(&data, &id)?;
        self.expand_playlists(&mut video, "Dailymotion", "https://www.dailymotion.com/")
            .await?;
        Ok(video)
    }

    async fn vimeo(&self, input: &Url) -> Result<NativeVideo> {
        let (id, hash) =
            vimeo_id(input).context("请输入 Vimeo 公开单视频或 player.vimeo.com 嵌入链接")?;
        let mut config = Url::parse(&format!("https://player.vimeo.com/video/{id}/config"))?;
        if let Some(hash) = hash {
            config.query_pairs_mut().append_pair("h", &hash);
        }
        let data = self
            .fetch_json(config, "Vimeo", "https://vimeo.com/")
            .await?;
        let mut video = parse_vimeo(&data, &id)?;
        self.expand_playlists(&mut video, "Vimeo", "https://vimeo.com/")
            .await?;
        Ok(video)
    }

    async fn expand_playlists(
        &self,
        video: &mut NativeVideo,
        platform: &str,
        referer: &str,
    ) -> Result<()> {
        let initial = std::mem::take(&mut video.variants);
        let mut errors = Vec::new();
        for variant in initial {
            if !is_playlist(&variant.url) {
                video.variants.push(variant);
                continue;
            }
            let result = async {
                let (url, text) = self
                    .fetch(Url::parse(&variant.url)?, platform, referer)
                    .await?;
                expand_hls_master(&url, &text, &variant, platform)
            }
            .await;
            match result {
                Ok(variants) => video.variants.extend(variants),
                Err(error) => errors.push(error.to_string()),
            }
        }
        deduplicate_variants(&mut video.variants);
        if video.variants.is_empty() {
            if let Some(error) = errors.first() {
                bail!("{platform} 当前没有完整可下载的公开音视频：{error}");
            }
            bail!("{platform} 当前没有可下载的公开音视频");
        }
        Ok(())
    }
}

fn host_matches(url: &Url, domain: &str) -> bool {
    url.host_str()
        .is_some_and(|host| host == domain || host.ends_with(&format!(".{domain}")))
}

fn allowed_fetch_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && [
            "vimeo.com",
            "vimeocdn.com",
            "dailymotion.com",
            "dmcdn.net",
            "dai.ly",
            "streamable.com",
            "imgur.com",
        ]
        .iter()
        .any(|host| host_matches(url, host))
}

fn media_url(raw: &str, platform: &str) -> Result<String> {
    let raw = if raw.starts_with("//") {
        format!("https:{raw}")
    } else {
        raw.to_owned()
    };
    let url = Url::parse(&raw).context("平台返回了无效媒体地址")?;
    let domains: &[&str] = match platform {
        "Vimeo" => &["vimeocdn.com", "vimeo.com"],
        "Dailymotion" => &["dailymotion.com", "dmcdn.net"],
        "Streamable" => &["streamable.com"],
        "Imgur" => &["imgur.com"],
        _ => &[],
    };
    ensure!(
        allowed_fetch_url(&url) && domains.iter().any(|domain| host_matches(&url, domain)),
        "{platform} 返回了非官方媒体地址"
    );
    // Do not rebuild the query: CDN signatures and expiry parameters are exact.
    Ok(url.into())
}

fn nonempty(value: &Value) -> Option<&str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn duration(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| s.round() as u64)
        .or_else(|| number(value))
}

fn height(value: &Value) -> u32 {
    number(value).and_then(|n| n.try_into().ok()).unwrap_or(0)
}

fn variant(
    id: String,
    height: u32,
    url: String,
    size: Option<u64>,
    referer: &str,
    hls: bool,
) -> NativeVariant {
    NativeVariant {
        label: if height == 0 {
            if hls {
                "HLS · MP4".into()
            } else {
                "MP4".into()
            }
        } else {
            format!("{height}p · {}", if hls { "HLS" } else { "MP4" })
        },
        id,
        height,
        url,
        size,
        headers: vec![
            ("Referer".into(), referer.into()),
            ("Accept".into(), "*/*".into()),
            ("User-Agent".into(), USER_AGENT.into()),
        ],
        decrypt_key: None,
    }
}

fn deduplicate_variants(variants: &mut Vec<NativeVariant>) {
    let mut seen = HashSet::new();
    variants.retain(|item| seen.insert(item.url.clone()));
    variants.sort_by_key(|item| std::cmp::Reverse(item.height));
}

fn streamable_id(url: &Url) -> Option<String> {
    if !host_matches(url, "streamable.com") {
        return None;
    }
    let re =
        Regex::new(r"^/(?:e/)?([A-Za-z0-9]+)/?$|^/s/([A-Za-z0-9]+)(?:/[A-Za-z0-9]+)?/?$").ok()?;
    let captures = re.captures(url.path())?;
    let id = captures.get(1).or_else(|| captures.get(2))?.as_str();
    (id.len() <= 32).then(|| id.to_owned())
}

fn dailymotion_id(url: &Url) -> Option<String> {
    let pattern = if host_matches(url, "dai.ly") {
        r"^/([A-Za-z0-9]+)/?$"
    } else if host_matches(url, "dailymotion.com") {
        r"^/(?:embed/)?video/([A-Za-z0-9]+)(?:_[^/]*)?/?$"
    } else {
        return None;
    };
    let re = Regex::new(pattern).ok()?;
    let captures = re.captures(url.path())?;
    let id = captures.get(1)?.as_str();
    (id.len() <= 32).then(|| id.to_owned())
}

fn imgur_id(url: &Url) -> Option<String> {
    if !host_matches(url, "imgur.com") {
        return None;
    }
    let re = Regex::new(r"^/(?:[^/]+-)?([A-Za-z0-9]+)(?:\.(?:gifv|mp4|webm))?/?$").ok()?;
    let captures = re.captures(url.path())?;
    let id = captures.get(1)?.as_str();
    (!matches!(id, "a" | "gallery" | "t" | "topic" | "r") && id.len() <= 32).then(|| id.to_owned())
}

fn vimeo_id(url: &Url) -> Option<(String, Option<String>)> {
    if !host_matches(url, "vimeo.com") {
        return None;
    }
    let re = Regex::new(
        r"^/(?:video/|channels/[^/]+/|groups/[^/]+/videos/)?([0-9]+)(?:/([a-fA-F0-9]{10}))?/?$",
    )
    .ok()?;
    let captures = re.captures(url.path())?;
    let id = captures.get(1)?.as_str();
    if id.len() > 20 {
        return None;
    }
    let hash = captures.get(2).map(|m| m.as_str().to_owned()).or_else(|| {
        url.query_pairs()
            .find(|(key, _)| key == "h")
            .and_then(|(_, value)| {
                (value.len() == 10 && value.bytes().all(|c| c.is_ascii_hexdigit()))
                    .then(|| value.into_owned())
            })
    });
    Some((id.to_owned(), hash))
}

fn parse_streamable(data: &Value, expected_id: &str) -> Result<NativeVideo> {
    ensure!(
        nonempty(&data["shortcode"]) == Some(expected_id),
        "Streamable 返回了其他作品"
    );
    match number(&data["status"]) {
        Some(2) => {}
        Some(0 | 1) => bail!("Streamable 视频仍在上传或处理中，请稍后重试"),
        _ => bail!("Streamable 视频不可用或已经删除"),
    }
    ensure!(
        data["privacy_settings"]["password_protected"] != true,
        "Streamable 视频需要密码，当前仅支持公开作品"
    );
    ensure!(
        data["plan_limits"]["should_restrict_playback"] != true,
        "Streamable 视频播放受到限制"
    );
    let mut variants = Vec::new();
    if let Some(files) = data["files"].as_object() {
        for (key, info) in files {
            if !key.starts_with("mp4") {
                continue;
            }
            let Some(raw) = nonempty(&info["url"]) else {
                continue;
            };
            let url = media_url(raw, "Streamable")?;
            variants.push(variant(
                format!("streamable-{key}"),
                height(&info["height"]),
                url,
                number(&info["size"]),
                "https://streamable.com/",
                false,
            ));
        }
    }
    deduplicate_variants(&mut variants);
    ensure!(!variants.is_empty(), "Streamable 作品没有就绪的 MP4 文件");
    Ok(NativeVideo {
        id: expected_id.into(),
        title: nonempty(&data["reddit_title"])
            .or_else(|| nonempty(&data["title"]))
            .unwrap_or(expected_id)
            .into(),
        duration_seconds: duration(&data["duration"]),
        variants,
    })
}

fn parse_imgur(data: &Value, expected_id: &str) -> Result<NativeVideo> {
    ensure!(
        nonempty(&data["id"]) == Some(expected_id),
        "Imgur 返回了其他作品"
    );
    ensure!(
        data["is_album"] != true,
        "Imgur 相册暂不支持，请使用单个视频链接"
    );
    let items = data["media"].as_array().context("Imgur 没有返回作品媒体")?;
    let media = items
        .iter()
        .find(|item| nonempty(&item["id"]) == Some(expected_id))
        .context("Imgur 媒体 ID 与作品不匹配")?;
    ensure!(
        nonempty(&media["type"]) == Some("video") || media["metadata"]["is_animated"] == true,
        "这个 Imgur 链接是静态图片，不是视频"
    );
    let mut variants = Vec::new();
    if let Some(raw) = nonempty(&media["url"]) {
        let parsed = Url::parse(raw).ok();
        if parsed
            .as_ref()
            .is_some_and(|url| url.path().to_ascii_lowercase().ends_with(".mp4"))
        {
            variants.push(variant(
                "imgur-mp4".into(),
                height(&media["height"]),
                media_url(raw, "Imgur")?,
                number(&media["size"]),
                "https://imgur.com/",
                false,
            ));
        }
    }
    Ok(NativeVideo {
        id: expected_id.into(),
        title: nonempty(&media["metadata"]["title"])
            .or_else(|| nonempty(&data["title"]))
            .unwrap_or(expected_id)
            .into(),
        duration_seconds: duration(&media["metadata"]["duration"]).filter(|s| *s > 0),
        variants,
    })
}

fn imgur_player_sources(html: &str, expected_id: &str) -> Result<Vec<NativeVariant>> {
    let re = Regex::new(r#"(?is)<source\b[^>]*\bsrc\s*=\s*["']([^"']+)["'][^>]*>"#)?;
    let mut variants = Vec::new();
    for capture in re.captures_iter(html) {
        let raw = capture[1].replace("&amp;", "&");
        let url = media_url(&raw, "Imgur")?;
        let parsed = Url::parse(&url)?;
        if parsed.path() != format!("/{expected_id}.mp4") {
            continue;
        }
        variants.push(variant(
            "imgur-mp4".into(),
            0,
            url,
            None,
            "https://imgur.com/",
            false,
        ));
    }
    deduplicate_variants(&mut variants);
    Ok(variants)
}

fn parse_dailymotion(data: &Value, expected_id: &str) -> Result<NativeVideo> {
    if let Some(error) = data.get("error").filter(|e| !e.is_null()) {
        let message = nonempty(&error["title"])
            .or_else(|| nonempty(&error["raw_message"]))
            .unwrap_or("公开播放不可用");
        bail!("Dailymotion：{message}");
    }
    ensure!(
        nonempty(&data["id"]) == Some(expected_id),
        "Dailymotion 返回了其他作品"
    );
    ensure!(
        data["is_password_protected"] != true && data["private"] != true,
        "Dailymotion 视频需要登录或密码，当前仅支持公开作品"
    );
    ensure!(
        nonempty(&data["mode"]) != Some("live"),
        "Dailymotion 直播暂不支持"
    );
    ensure!(
        data["protected_delivery"] != true,
        "Dailymotion 视频使用受保护播放，当前不支持 DRM"
    );
    let mut variants = Vec::new();
    let qualities = data["qualities"]
        .as_object()
        .context("Dailymotion 没有返回公开播放资源")?;
    for (quality, entries) in qualities {
        for entry in entries.as_array().into_iter().flatten() {
            let Some(raw) = nonempty(&entry["url"]) else {
                continue;
            };
            let mime = nonempty(&entry["type"]).unwrap_or("");
            let hls = matches!(
                mime.to_ascii_lowercase().as_str(),
                "application/x-mpegurl" | "application/vnd.apple.mpegurl"
            );
            if !hls && mime != "video/mp4" {
                continue;
            }
            let h = quality.trim_end_matches('p').parse().unwrap_or(0);
            variants.push(variant(
                format!("dailymotion-{}-{quality}", if hls { "hls" } else { "mp4" }),
                h,
                media_url(raw, "Dailymotion")?,
                number(&entry["size"]),
                "https://www.dailymotion.com/",
                hls,
            ));
        }
    }
    deduplicate_variants(&mut variants);
    ensure!(
        !variants.is_empty(),
        "Dailymotion 没有公开 MP4/HLS 资源；直播和 DRM 暂不支持"
    );
    Ok(NativeVideo {
        id: expected_id.into(),
        title: nonempty(&data["title"]).unwrap_or(expected_id).into(),
        duration_seconds: duration(&data["duration"]),
        variants,
    })
}

fn parse_vimeo(data: &Value, expected_id: &str) -> Result<NativeVideo> {
    let video = &data["video"];
    let actual_id = video["id"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| number(&video["id"]).map(|id| id.to_string()));
    ensure!(
        actual_id.as_deref() == Some(expected_id),
        "Vimeo 返回了其他作品"
    );
    ensure!(
        nonempty(&video["live_event"]["status"]) != Some("started"),
        "Vimeo 直播暂不支持"
    );
    let files = video
        .get("files")
        .filter(|f| f.is_object())
        .unwrap_or(&data["request"]["files"]);
    let mut variants = Vec::new();
    for file in files["progressive"].as_array().into_iter().flatten() {
        let Some(raw) = nonempty(&file["url"]) else {
            continue;
        };
        let h = height(&file["height"]);
        variants.push(variant(
            format!(
                "vimeo-mp4-{}",
                nonempty(&file["quality"]).unwrap_or("source")
            ),
            h,
            media_url(raw, "Vimeo")?,
            number(&file["size"]),
            "https://vimeo.com/",
            false,
        ));
    }
    if let Some(cdns) = files["hls"]["cdns"].as_object() {
        // Pick one CDN from the supplied map; duplicate CDN manifests needlessly
        // add latency. No path mutation is used to strip DRM or change delivery.
        let preferred = nonempty(&files["hls"]["default_cdn"])
            .and_then(|name| cdns.get(name))
            .or_else(|| cdns.values().next());
        if let Some(cdn) = preferred {
            if let Some(raw) = nonempty(&cdn["url"]) {
                if !raw.contains("/drm/") {
                    variants.push(variant(
                        "vimeo-hls".into(),
                        0,
                        media_url(raw, "Vimeo")?,
                        None,
                        "https://vimeo.com/",
                        true,
                    ));
                }
            }
        }
    }
    deduplicate_variants(&mut variants);
    if variants.is_empty()
        && data["request"]
            .get("drm")
            .is_some_and(|value| !value.is_null() && value != false)
    {
        bail!("Vimeo 视频使用 DRM 受保护播放，当前不支持下载");
    }
    ensure!(
        !variants.is_empty(),
        "Vimeo 未返回公开播放资源；作品可能已删除、需要登录/密码或限制嵌入来源"
    );
    Ok(NativeVideo {
        id: expected_id.into(),
        title: nonempty(&video["title"]).unwrap_or(expected_id).into(),
        duration_seconds: duration(&video["duration"]),
        variants,
    })
}

fn is_playlist(raw: &str) -> bool {
    Url::parse(raw)
        .ok()
        .is_some_and(|url| url.path().to_ascii_lowercase().ends_with(".m3u8"))
}

fn expand_hls_master(
    base: &Url,
    text: &str,
    source: &NativeVariant,
    platform: &str,
) -> Result<Vec<NativeVariant>> {
    ensure!(
        text.trim_start_matches('\u{feff}')
            .trim_start()
            .starts_with("#EXTM3U"),
        "平台播放清单不是有效 HLS"
    );
    ensure!(
        !text.lines().any(|line| line.contains("METHOD=SAMPLE-AES")
            || (line.starts_with("#EXT-X-KEY:") && !line.contains("METHOD=NONE"))),
        "HLS 使用加密或 DRM，当前仅支持未加密资源"
    );
    if !text.contains("#EXT-X-STREAM-INF:") {
        ensure!(
            text.contains("#EXTINF:") && text.contains("#EXT-X-ENDLIST"),
            "HLS 直播或空播放清单暂不支持"
        );
        return Ok(vec![variant(
            source.id.clone(),
            source.height,
            media_url(base.as_str(), platform)?,
            source.size,
            source
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("Referer"))
                .map(|(_, value)| value.as_str())
                .unwrap_or(""),
            true,
        )]);
    }
    let attr_re = Regex::new(r#"([A-Z0-9-]+)=(?:"([^"]*)"|([^,]*))(?:,|$)"#)?;
    let mut pending: Option<(u32, String)> = None;
    let mut variants = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let mut h = 0;
            let mut audio_group = false;
            let mut codecs = String::new();
            for capture in attr_re.captures_iter(attrs) {
                let value = capture
                    .get(2)
                    .or_else(|| capture.get(3))
                    .map(|m| m.as_str())
                    .unwrap_or("");
                match &capture[1] {
                    "AUDIO" => audio_group = true,
                    "RESOLUTION" => {
                        h = value
                            .split_once('x')
                            .and_then(|(_, h)| h.parse().ok())
                            .unwrap_or(0)
                    }
                    "CODECS" => codecs = value.to_ascii_lowercase(),
                    _ => {}
                }
            }
            // An external audio group cannot be represented by NativeVariant;
            // do not expose a silent video as a complete MP4 option.
            let video_codec = ["avc", "hvc", "hev", "vp", "av01"]
                .iter()
                .any(|codec| codecs.contains(codec));
            let audio_codec = ["mp4a", "ac-3", "ec-3", "opus", "vorbis"]
                .iter()
                .any(|codec| codecs.contains(codec));
            // The current HLS contract represents a complete audio/video
            // stream. Unknown or explicitly video-only codec sets cannot prove
            // that audio is carried in-band, so leave those unsupported.
            pending =
                (!audio_group && (h > 0 || video_codec) && audio_codec).then_some((h, codecs));
        } else if !line.is_empty() && !line.starts_with('#') {
            if let Some((h, _)) = pending.take() {
                let child = base.join(line)?;
                ensure!(is_playlist(child.as_str()), "HLS 子清单地址格式暂不支持");
                variants.push(variant(
                    format!("{}-{h}-{}", source.id, variants.len()),
                    h,
                    media_url(child.as_str(), platform)?,
                    None,
                    source
                        .headers
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("Referer"))
                        .map(|(_, value)| value.as_str())
                        .unwrap_or(""),
                    true,
                ));
            }
        }
    }
    deduplicate_variants(&mut variants);
    ensure!(
        !variants.is_empty(),
        "HLS 只有独立音视频轨或不支持的变体，当前无法生成完整有声文件"
    );
    Ok(variants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn public_item_ids_reject_playlists_and_unrelated_paths() {
        assert_eq!(
            streamable_id(&Url::parse("https://streamable.com/s/dnd1/player").unwrap()).as_deref(),
            Some("dnd1")
        );
        assert!(
            streamable_id(&Url::parse("https://streamable.com/accounts/123").unwrap()).is_none()
        );
        assert_eq!(
            dailymotion_id(&Url::parse("https://www.dailymotion.com/video/x5mbk1l_title").unwrap())
                .as_deref(),
            Some("x5mbk1l")
        );
        assert!(dailymotion_id(
            &Url::parse("https://www.dailymotion.com/playlist/x5mbk1l").unwrap()
        )
        .is_none());
        assert_eq!(
            imgur_id(&Url::parse("https://imgur.com/a-title-A61SaA1").unwrap()).as_deref(),
            Some("A61SaA1")
        );
        assert!(imgur_id(&Url::parse("https://imgur.com/gallery/A61SaA1").unwrap()).is_none());
        assert_eq!(
            vimeo_id(&Url::parse("https://player.vimeo.com/video/123?h=012345abcd").unwrap()),
            Some(("123".into(), Some("012345abcd".into())))
        );
        assert!(vimeo_id(&Url::parse("https://vimeo.com/ondemand/123").unwrap()).is_none());
        assert!(!allowed_fetch_url(
            &Url::parse("https://streamable.com.evil.test/videos/1").unwrap()
        ));
        assert!(!allowed_fetch_url(
            &Url::parse("http://ajax.streamable.com/videos/1").unwrap()
        ));
    }

    #[test]
    fn streamable_keeps_signature_and_processing_status() {
        let mut data = json!({"shortcode":"abc","status":2,"title":"Title","duration":12.5,"files":{"mp4":{"height":720,"size":2048,"url":"//cdn-cf-west.streamable.com/video/abc.mp4?Signature=a%2Bb&Expires=123"}}});
        let video = parse_streamable(&data, "abc").unwrap();
        assert_eq!(
            video.variants[0].url,
            "https://cdn-cf-west.streamable.com/video/abc.mp4?Signature=a%2Bb&Expires=123"
        );
        assert_eq!(video.variants[0].size, Some(2048));
        assert!(parse_streamable(&data, "other").is_err());
        data["status"] = json!(1);
        assert!(parse_streamable(&data, "abc")
            .unwrap_err()
            .to_string()
            .contains("处理中"));
    }

    #[test]
    fn imgur_uses_matching_media_and_matching_player_source_only() {
        let data = json!({"id":"A1","is_album":false,"media":[{"id":"other","type":"video","url":"https://i.imgur.com/other.mp4"},{"id":"A1","type":"image","metadata":{"is_animated":true,"title":"Animation"},"url":"https://i.imgur.com/A1.gif"}]});
        let video = parse_imgur(&data, "A1").unwrap();
        assert_eq!(video.title, "Animation");
        assert!(video.variants.is_empty());
        let formats = imgur_player_sources("<source src='https://i.imgur.com/other.mp4'><source type='video/mp4' src='https://i.imgur.com/A1.mp4?x=1&amp;y=2'>", "A1").unwrap();
        assert_eq!(formats.len(), 1);
        assert!(formats[0].url.ends_with("?x=1&y=2"));
        assert!(parse_imgur(&json!({"id":"A1","media":[{"id":"A1","type":"image","metadata":{"is_animated":false}}]}), "A1").is_err());
    }

    #[test]
    fn dailymotion_reports_restrictions_and_ignores_advertisements() {
        let data = json!({"id":"x1","title":"Item","duration":5,"advertising":{"url":"https://www.dailymotion.com/ad.mp4"},"qualities":{"720":[{"type":"video/mp4","url":"https://www.dailymotion.com/item.mp4?sig=x"}],"auto":[{"type":"application/vnd.lumberjack.manifest","url":"https://www.dailymotion.com/unsupported.mpd"}]}});
        let video = parse_dailymotion(&data, "x1").unwrap();
        assert_eq!(video.variants.len(), 1);
        assert_eq!(video.variants[0].height, 720);
        assert!(
            parse_dailymotion(&json!({"error":{"title":"Geo restricted"}}), "x1")
                .unwrap_err()
                .to_string()
                .contains("Geo restricted")
        );
        assert!(
            parse_dailymotion(&json!({"id":"x1","protected_delivery":true}), "x1")
                .unwrap_err()
                .to_string()
                .contains("DRM")
        );
    }

    #[test]
    fn vimeo_never_strips_drm_or_uses_another_video() {
        let data = json!({"video":{"id":12,"title":"Public"},"request":{"files":{"progressive":[{"quality":"1080p","height":1080,"url":"https://vod-progressive.vimeocdn.com/item.mp4?signature=abc"}],"hls":{"cdns":{"ak":{"url":"https://vod-adaptive-ak.vimeocdn.com/playlist/drm/cbcs/master.m3u8"}}}}}});
        let video = parse_vimeo(&data, "12").unwrap();
        assert_eq!(video.variants.len(), 1);
        assert_eq!(video.variants[0].height, 1080);
        assert!(parse_vimeo(&data, "13").is_err());
        let drm = json!({"video":{"id":12},"request":{"drm":{"enabled":true},"files":{"hls":{"cdns":{"ak":{"url":"https://vod-adaptive-ak.vimeocdn.com/playlist/drm/cbcs/master.m3u8"}}}}}});
        assert!(parse_vimeo(&drm, "12")
            .unwrap_err()
            .to_string()
            .contains("DRM"));
    }

    #[test]
    fn hls_expansion_excludes_external_audio_and_preserves_signed_child_queries() {
        let source = variant(
            "hls".into(),
            0,
            "https://cdn.dmcdn.net/master.m3u8?parent=1".into(),
            None,
            "https://www.dailymotion.com/",
            true,
        );
        let master = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=123,RESOLUTION=1280x720,CODECS=\"avc1,mp4a\"\n720.m3u8?sec=a%2Bb&expires=123\n#EXT-X-STREAM-INF:BANDWIDTH=456,RESOLUTION=1920x1080,AUDIO=\"audio\"\n1080.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=456,RESOLUTION=1920x1080,CODECS=\"avc1\"\n1080-silent.m3u8\n";
        let result = expand_hls_master(
            &Url::parse(&source.url).unwrap(),
            master,
            &source,
            "Dailymotion",
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].height, 720);
        assert_eq!(
            result[0].url,
            "https://cdn.dmcdn.net/720.m3u8?sec=a%2Bb&expires=123"
        );
        let protected = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\"\n#EXTINF:1,\nsegment.ts\n#EXT-X-ENDLIST";
        assert!(expand_hls_master(
            &Url::parse(&source.url).unwrap(),
            protected,
            &source,
            "Dailymotion"
        )
        .is_err());
        assert!(expand_hls_master(
            &Url::parse(&source.url).unwrap(),
            "#EXTM3U\n#EXTINF:1,\nlive.ts",
            &source,
            "Dailymotion"
        )
        .is_err());
    }
}
