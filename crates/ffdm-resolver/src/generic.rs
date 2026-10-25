//! Bounded, static webpage extraction. No page script is evaluated.
//! Inspired by the media tags and structured metadata used by yt-dlp's
//! generic extractor; unknown pages never become automatic download targets.
use anyhow::{ensure, Context, Result};
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

const USER_AGENT: &str = crate::resource::USER_AGENT;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CANDIDATES: usize = 32;
const MAX_GROUPS: usize = 64;

pub(crate) struct GenericResource {
    pub id: String,
    pub title: String,
    pub duration_seconds: Option<u64>,
    pub candidates: Vec<GenericCandidate>,
    pub embed: Option<Url>,
}

#[derive(Clone)]
pub(crate) struct GenericCandidate {
    pub id: String,
    pub label: String,
    pub height: u32,
    pub url: String,
    pub extension: String,
    pub audio_only: bool,
    pub headers: Vec<(String, String)>,
    pub size: Option<u64>,
    pub transform: Option<crate::media::StreamTransform>,
}

pub(crate) struct GenericResolver {
    client: Client,
    local_client: Client,
}

impl GenericResolver {
    pub fn new() -> Result<Self> {
        let make = || {
            Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .user_agent(USER_AGENT)
                .redirect(Policy::custom(|attempt| {
                    let target = attempt.url();
                    if attempt.previous().len() >= 7
                        || !matches!(target.scheme(), "http" | "https")
                        || !target.username().is_empty()
                        || target.password().is_some()
                        || attempt.previous().last().is_some_and(|previous| {
                            previous.scheme() == "https" && target.scheme() == "http"
                        })
                    {
                        attempt.stop()
                    } else {
                        attempt.follow()
                    }
                }))
        };
        Ok(Self {
            client: make().build()?,
            local_client: make().no_proxy().build()?,
        })
    }

    pub async fn resolve(&self, input: &Url) -> Result<GenericResource> {
        let input = crate::validation::parse_url(input.as_str())?;
        let client = if input.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }) {
            &self.local_client
        } else {
            &self.client
        };
        let mut response = client
            .get(input.clone())
            .header(header::ACCEPT, "text/html,application/xhtml+xml;q=0.9")
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .context("通用网页解析请求失败")?;
        ensure!(
            response.status().is_success(),
            "网页无法读取（HTTP {}）",
            response.status()
        );
        let final_url = crate::validation::parse_url(response.url().as_str())?;
        if final_url != input && crate::routing::platform(final_url.as_str()).is_some() {
            return Ok(GenericResource {
                id: stable_id("page", final_url.as_str()),
                title: final_url.host_str().unwrap_or("网页视频").to_owned(),
                duration_seconds: None,
                candidates: Vec::new(),
                embed: Some(final_url),
            });
        }
        if let Some(size) = response.content_length() {
            ensure!(size <= MAX_PAGE_BYTES as u64, "网页超过 2 MiB 解析上限");
        }
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        ensure!(
            content_type.is_empty()
                || content_type.split(';').next().is_some_and(|mime| {
                    matches!(
                        mime.trim().to_ascii_lowercase().as_str(),
                        "text/html" | "application/xhtml+xml" | "text/plain"
                    )
                }),
            "该地址不是 HTML 页面，请使用文件直链下载"
        );
        let encoding = response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("identity");
        ensure!(
            encoding.eq_ignore_ascii_case("identity"),
            "网页返回了不支持的压缩编码"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context("读取网页失败")? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_PAGE_BYTES,
                "网页超过 2 MiB 解析上限"
            );
            bytes.extend_from_slice(&chunk);
        }
        let html = String::from_utf8_lossy(&bytes);
        parse_page(&final_url, &html)
    }
}

#[derive(Default)]
struct MediaGroup {
    title: Option<String>,
    duration: Option<u64>,
    candidates: Vec<GenericCandidate>,
    embeds: Vec<Url>,
}

fn parse_page(page: &Url, html: &str) -> Result<GenericResource> {
    ensure!(html.len() <= MAX_PAGE_BYTES, "网页超过 2 MiB 解析上限");
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    let mut base = page.clone();
    let mut base_seen = false;
    let mut title = None;
    let mut meta_title = None;
    let mut html_groups = Vec::new();
    let mut current: Option<(String, bool, MediaGroup)> = None;
    let mut structured_groups = Vec::new();
    let mut og_groups = Vec::<MediaGroup>::new();
    let mut og_current: Option<(bool, MediaGroup)> = None;
    let mut embeds = Vec::new();
    let mut deferred_meta = Vec::new();
    while let Some(tag) = next_tag(html, &mut cursor) {
        if tag.closing {
            if current
                .as_ref()
                .is_some_and(|(name, _, _)| name == &tag.name)
            {
                html_groups.push(current.take().unwrap().2);
            }
            continue;
        }
        match tag.name.as_str() {
            "base" if !base_seen => {
                base_seen = true;
                if let Some(url) = tag.attr("href").and_then(|value| resolve_url(page, value)) {
                    base = url;
                }
            }
            "title" => {
                if let Some(end) = lower[cursor..].find("</title") {
                    title.get_or_insert_with(|| {
                        clean_text(&decode_entities(&html[cursor..cursor + end]))
                    });
                    cursor += end;
                }
            }
            "video" | "audio" => {
                if let Some((_, _, group)) = current.take() {
                    html_groups.push(group);
                }
                let audio_only = tag.name == "audio";
                let mut group = MediaGroup::default();
                if let Some(value) = tag.attr("src") {
                    add_candidate(
                        &mut group,
                        &base,
                        page,
                        value,
                        SourceHint {
                            mime: tag.attr("type"),
                            audio_only,
                            label: tag.attr("label"),
                            height: tag.attr("height"),
                        },
                        &mut embeds,
                    );
                }
                if tag.self_closing {
                    html_groups.push(group);
                } else {
                    current = Some((tag.name.clone(), audio_only, group));
                }
            }
            "source" => {
                if let (Some((_, audio_only, group)), Some(value)) = (&mut current, tag.attr("src"))
                {
                    add_candidate(
                        group,
                        &base,
                        page,
                        value,
                        SourceHint {
                            mime: tag.attr("type"),
                            audio_only: *audio_only,
                            label: tag.attr("label").or_else(|| tag.attr("data-res")),
                            height: tag
                                .attr("height")
                                .or_else(|| tag.attr("res"))
                                .or_else(|| tag.attr("data-res")),
                        },
                        &mut embeds,
                    );
                }
            }
            "meta" => {
                let property = tag
                    .attr("property")
                    .or_else(|| tag.attr("name"))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let value = tag
                    .attr("content")
                    .or_else(|| tag.attr("value"))
                    .unwrap_or("");
                if matches!(property.as_str(), "og:title" | "twitter:title") && meta_title.is_none()
                {
                    meta_title = Some(clean_text(&decode_entities(value)));
                }
                deferred_meta.push((property, value.to_owned()));
            }
            "iframe" | "embed" | "object" => {
                if let Some(value) = tag.attr("src").or_else(|| tag.attr("data")) {
                    if let Some(url) = resolve_url(&base, value) {
                        let hints = ["id", "class", "title", "name"]
                            .iter()
                            .filter_map(|name| tag.attr(name))
                            .collect::<Vec<_>>()
                            .join(" ")
                            .to_ascii_lowercase();
                        if crate::routing::platform(url.as_str()).is_some()
                            || ["video", "player", "embed"]
                                .iter()
                                .any(|hint| hints.contains(hint))
                            || is_embed_path(&url)
                        {
                            embeds.push(url);
                        }
                    }
                }
            }
            "script" => {
                let Some(end) = lower[cursor..].find("</script") else {
                    break;
                };
                let script = &html[cursor..cursor + end];
                let kind = tag.attr("type").unwrap_or("").trim().to_ascii_lowercase();
                if kind == "application/ld+json" {
                    if let Ok(value) = serde_json::from_str::<Value>(script.trim()) {
                        collect_json_ld(
                            &value,
                            &base,
                            page,
                            0,
                            &mut structured_groups,
                            &mut embeds,
                        );
                    }
                } else if matches!(
                    kind.as_str(),
                    "application/json" | "application/jwplayer+json"
                ) {
                    if let Ok(value) = serde_json::from_str::<Value>(script.trim()) {
                        collect_jw(&value, &base, page, &mut structured_groups, &mut embeds);
                    }
                } else if kind.is_empty()
                    || matches!(kind.as_str(), "text/javascript" | "application/javascript")
                {
                    collect_jw_setup(script, &base, page, &mut structured_groups, &mut embeds);
                }
                cursor += end;
            }
            "style" | "textarea" | "template" | "noscript" => {
                if let Some(end) = lower[cursor..].find(&format!("</{}", tag.name)) {
                    cursor += end;
                }
            }
            _ => {}
        }
        ensure!(
            html_groups.len() + structured_groups.len() + og_groups.len() < MAX_GROUPS,
            "网页包含过多独立媒体，请提供单个作品页面"
        );
    }
    if let Some((_, _, group)) = current {
        html_groups.push(group);
    }
    let mut video_type = None;
    let mut audio_type = None;
    for (property, value) in &deferred_meta {
        match property.as_str() {
            "og:video:type" | "twitter:player:stream:content_type" => {
                video_type = Some(value.as_str())
            }
            "og:audio:type" => audio_type = Some(value.as_str()),
            _ => {}
        }
    }
    for (property, value) in &deferred_meta {
        match property.as_str() {
            "og:video" | "og:video:url" | "og:audio" | "og:audio:url" | "twitter:player:stream" => {
                let audio_only = property.starts_with("og:audio");
                if let Some((_, group)) = og_current.take() {
                    og_groups.push(group);
                }
                let mut group = MediaGroup::default();
                add_candidate(
                    &mut group,
                    &base,
                    page,
                    value,
                    SourceHint {
                        mime: if audio_only { audio_type } else { video_type },
                        audio_only,
                        ..Default::default()
                    },
                    &mut embeds,
                );
                og_current = Some((audio_only, group));
            }
            "og:video:secure_url" | "og:audio:secure_url" => {
                let audio_only = property.starts_with("og:audio");
                if let Some((same_audio, group)) = &mut og_current {
                    if *same_audio == audio_only {
                        add_candidate(
                            group,
                            &base,
                            page,
                            value,
                            SourceHint {
                                mime: if audio_only { audio_type } else { video_type },
                                audio_only,
                                ..Default::default()
                            },
                            &mut embeds,
                        );
                    }
                } else {
                    let mut group = MediaGroup::default();
                    add_candidate(
                        &mut group,
                        &base,
                        page,
                        value,
                        SourceHint {
                            mime: if audio_only { audio_type } else { video_type },
                            audio_only,
                            ..Default::default()
                        },
                        &mut embeds,
                    );
                    og_current = Some((audio_only, group));
                }
            }
            "twitter:player" => {
                if let Some(url) = resolve_url(&base, value) {
                    embeds.push(url);
                }
            }
            _ => {}
        }
    }
    if let Some((_, group)) = og_current {
        og_groups.push(group);
    }
    // HTML elements describe actual players. Their alternatives take priority
    // over secondary recommendation objects in page metadata.
    let group = if has_media(&html_groups) {
        choose_one_group(html_groups)?
    } else if has_media(&structured_groups) {
        choose_one_group(structured_groups)?
    } else {
        choose_one_group(og_groups)?
    };
    let mut candidates = group.candidates;
    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.url.clone()));
    ensure!(
        candidates.len() <= MAX_CANDIDATES,
        "单个作品超过 32 个格式，请提供更明确的播放器页面"
    );
    let embed = if candidates.is_empty() {
        embeds.extend(group.embeds);
        choose_embed(page, embeds)?
    } else {
        None
    };
    ensure!(
        !candidates.is_empty() || embed.is_some(),
        "网页未发现可直接读取的音视频；请提供作品链接或媒体直链"
    );
    Ok(GenericResource {
        id: stable_id("page", page.as_str()),
        title: group
            .title
            .or(meta_title)
            .or(title)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| page.host_str().unwrap_or("网页媒体").to_owned()),
        duration_seconds: group.duration,
        candidates,
        embed,
    })
}

fn has_media(groups: &[MediaGroup]) -> bool {
    groups.iter().any(|group| !group.candidates.is_empty())
}

fn choose_one_group(groups: Vec<MediaGroup>) -> Result<MediaGroup> {
    let mut selected: Option<MediaGroup> = None;
    for group in groups
        .into_iter()
        .filter(|group| !group.candidates.is_empty() || !group.embeds.is_empty())
    {
        if let Some(previous) = &mut selected {
            let overlaps = group.candidates.iter().any(|next| {
                previous
                    .candidates
                    .iter()
                    .any(|old| media_identity(&old.url) == media_identity(&next.url))
            }) || group.embeds.iter().any(|next| {
                previous
                    .embeds
                    .iter()
                    .any(|old| embed_identity(old) == embed_identity(next))
            });
            ensure!(
                overlaps,
                "网页包含多个不同音视频，请提供单个作品或播放器链接"
            );
            previous.candidates.extend(group.candidates);
            previous.embeds.extend(group.embeds);
            if previous.title.is_none() {
                previous.title = group.title;
            }
            if previous.duration.is_none() {
                previous.duration = group.duration;
            }
        } else {
            selected = Some(group);
        }
    }
    Ok(selected.unwrap_or_default())
}

#[derive(Default)]
struct SourceHint<'a> {
    mime: Option<&'a str>,
    audio_only: bool,
    label: Option<&'a str>,
    height: Option<&'a str>,
}

fn add_candidate(
    group: &mut MediaGroup,
    base: &Url,
    page: &Url,
    raw: &str,
    hint: SourceHint<'_>,
    embeds: &mut Vec<Url>,
) {
    let SourceHint {
        mime,
        audio_only: declared_audio,
        label,
        height,
    } = hint;
    if group.candidates.len() > MAX_CANDIDATES {
        return;
    }
    let Some(url) = resolve_url(base, raw) else {
        return;
    };
    let (extension, mime_audio) = media_extension(&url, mime);
    // The public router also identifies direct HLS playlists. Those are
    // candidates for native manifest expansion, not embedded website pages.
    if crate::routing::platform(url.as_str()).is_some_and(|platform| platform != "HLS")
        || mime.is_some_and(|value| value.trim().eq_ignore_ascii_case("text/html"))
        || extension.is_empty() && is_embed_path(&url)
    {
        group.embeds.push(url.clone());
        embeds.push(url);
        return;
    }
    // A file in a declarative media element may have no suffix. Leave its
    // container unknown so the outer resolver can verify response metadata.
    if mime.is_some_and(|value| {
        !value.trim().is_empty()
            && extension.is_empty()
            && !value.trim().starts_with("video/")
            && !value.trim().starts_with("audio/")
            && value.trim() != "application/octet-stream"
    }) {
        return;
    }
    if matches!(
        extension.as_str(),
        "jpg" | "png" | "gif" | "svg" | "js" | "html" | "vtt" | "srt"
    ) {
        return;
    }
    let explicitly_video =
        mime.is_some_and(|mime| mime.trim().to_ascii_lowercase().starts_with("video/"));
    let audio_only = declared_audio
        || mime_audio
        || !explicitly_video
            && matches!(
                extension.as_str(),
                "m4a" | "mp3" | "ogg" | "opus" | "aac" | "flac" | "wav" | "ape"
            );
    let height = height.or(label).and_then(parse_height).unwrap_or(0);
    let label = if height > 0 {
        format!("{height}p")
    } else if audio_only {
        "音频".to_owned()
    } else if extension == "m3u8" {
        "HLS 视频".to_owned()
    } else {
        "网页视频".to_owned()
    };
    let mut referer = page.clone();
    referer.set_query(None);
    referer.set_fragment(None);
    let headers = vec![
        ("User-Agent".to_owned(), USER_AGENT.to_owned()),
        ("Referer".to_owned(), referer.to_string()),
    ];
    if crate::validation::validate_headers(&headers).is_err() {
        return;
    }
    group.candidates.push(GenericCandidate {
        id: stable_id("generic", url.as_str()),
        label,
        height,
        url: url.to_string(),
        extension,
        audio_only,
        headers,
        size: None,
        transform: None,
    });
}

fn collect_json_ld(
    value: &Value,
    base: &Url,
    page: &Url,
    depth: usize,
    groups: &mut Vec<MediaGroup>,
    embeds: &mut Vec<Url>,
) {
    if depth > 16 || groups.len() >= MAX_GROUPS {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items.iter().take(128) {
                collect_json_ld(item, base, page, depth + 1, groups, embeds);
            }
        }
        Value::Object(object) => {
            let types: Vec<&str> = match object.get("@type") {
                Some(Value::String(value)) => vec![value.as_str()],
                Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            let audio = types
                .iter()
                .any(|kind| kind.rsplit('/').next() == Some("AudioObject"));
            let video = types
                .iter()
                .any(|kind| kind.rsplit('/').next() == Some("VideoObject"));
            if audio || video {
                let mut group = MediaGroup {
                    title: object.get("name").and_then(Value::as_str).map(clean_text),
                    duration: object.get("duration").and_then(parse_duration),
                    ..Default::default()
                };
                let mime = object.get("encodingFormat").and_then(Value::as_str);
                if let Some(content) = object.get("contentUrl") {
                    for raw in string_values(content) {
                        add_candidate(
                            &mut group,
                            base,
                            page,
                            raw,
                            SourceHint {
                                mime,
                                audio_only: audio,
                                height: object.get("height").and_then(Value::as_str),
                                ..Default::default()
                            },
                            embeds,
                        );
                    }
                }
                if let Some(content) = object.get("embedUrl") {
                    for raw in string_values(content) {
                        if let Some(url) = resolve_url(base, raw) {
                            group.embeds.push(url.clone());
                            embeds.push(url);
                        }
                    }
                }
                if !group.candidates.is_empty() || !group.embeds.is_empty() {
                    groups.push(group);
                }
            } else {
                for value in object.values().take(128) {
                    if value.is_array() || value.is_object() {
                        collect_json_ld(value, base, page, depth + 1, groups, embeds);
                    }
                }
            }
        }
        _ => {}
    }
}

fn collect_jw(
    value: &Value,
    base: &Url,
    page: &Url,
    groups: &mut Vec<MediaGroup>,
    embeds: &mut Vec<Url>,
) {
    let Some(object) = value.as_object() else {
        return;
    };
    if let Some(playlist) = object.get("playlist").and_then(Value::as_array) {
        for item in playlist.iter().take(MAX_GROUPS) {
            collect_jw(item, base, page, groups, embeds);
        }
        return;
    }
    // Strings in arbitrary JSON state are not searched. Only the documented
    // JW media item shape (file/sources/playlist) is interpreted.
    if !object.contains_key("sources") && !object.contains_key("file") {
        return;
    }
    let mut group = MediaGroup {
        title: object.get("title").and_then(Value::as_str).map(clean_text),
        duration: object.get("duration").and_then(parse_duration),
        ..Default::default()
    };
    let sources: Vec<&Value> = object
        .get("sources")
        .and_then(Value::as_array)
        .map(|sources| sources.iter().take(MAX_CANDIDATES + 1).collect())
        .unwrap_or_default();
    if let Some(raw) = object.get("file").and_then(Value::as_str) {
        add_candidate(
            &mut group,
            base,
            page,
            raw,
            SourceHint {
                mime: object.get("type").and_then(Value::as_str),
                label: object.get("label").and_then(Value::as_str),
                height: object.get("height").and_then(Value::as_str),
                ..Default::default()
            },
            embeds,
        );
    }
    for source in sources {
        let raw = source.as_str().or_else(|| {
            source
                .get("file")
                .or_else(|| source.get("src"))
                .and_then(Value::as_str)
        });
        if let Some(raw) = raw {
            add_candidate(
                &mut group,
                base,
                page,
                raw,
                SourceHint {
                    mime: source.get("type").and_then(Value::as_str),
                    label: source.get("label").and_then(Value::as_str),
                    height: source.get("height").and_then(Value::as_str),
                    ..Default::default()
                },
                embeds,
            );
        }
    }
    if !group.candidates.is_empty() || !group.embeds.is_empty() {
        groups.push(group);
    }
}

fn collect_jw_setup(
    script: &str,
    base: &Url,
    page: &Url,
    groups: &mut Vec<MediaGroup>,
    embeds: &mut Vec<Url>,
) {
    // Locate a documented setup call, then let serde_json delimit its JSON
    // value. No JavaScript-to-JSON rewriting or script execution is allowed.
    let setup =
        Regex::new(r#"(?i)\bjwplayer\s*\(\s*(?:"[^"]*"|'[^']*')\s*\)\s*\.\s*setup\s*\("#).unwrap();
    for matched in setup.find_iter(script).take(MAX_GROUPS) {
        let rest = script[matched.end()..].trim_start();
        let mut json = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        if let Some(Ok(value)) = json.next() {
            if rest[json.byte_offset()..].trim_start().starts_with(')') {
                collect_jw(&value, base, page, groups, embeds);
            }
        }
    }
}

fn string_values(value: &Value) -> Vec<&str> {
    match value {
        Value::String(value) => vec![value.as_str()],
        Value::Array(values) => values
            .iter()
            .take(MAX_CANDIDATES + 1)
            .filter_map(Value::as_str)
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_duration(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_f64() {
        return (number.is_finite() && (0.0..=604800.0).contains(&number))
            .then_some(number.round() as u64);
    }
    let text = value.as_str()?;
    if let Ok(seconds) = text.parse::<f64>() {
        return (seconds.is_finite() && (0.0..=604800.0).contains(&seconds))
            .then_some(seconds.round() as u64);
    }
    let text = text.strip_prefix("PT")?;
    let mut number = String::new();
    let mut total = 0.0;
    let mut seen = false;
    for character in text.chars() {
        if character.is_ascii_digit() || character == '.' {
            number.push(character);
        } else {
            let factor = match character {
                'H' => 3600.0,
                'M' => 60.0,
                'S' => 1.0,
                _ => return None,
            };
            total += number.parse::<f64>().ok()? * factor;
            number.clear();
            seen = true;
        }
    }
    (seen && number.is_empty() && total.is_finite() && (0.0..=604800.0).contains(&total))
        .then_some(total.round() as u64)
}

fn parse_height(value: &str) -> Option<u32> {
    let token = value.trim().strip_suffix('p').unwrap_or(value.trim());
    token
        .parse::<u32>()
        .ok()
        .filter(|height| (1..=8640).contains(height))
}

fn media_extension(url: &Url, mime: Option<&str>) -> (String, bool) {
    let mime = mime
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mime_ext = match mime.as_str() {
        "video/mp4" | "mp4" => "mp4",
        "video/webm" | "webm" => "webm",
        "video/ogg" => "ogg",
        "video/quicktime" => "mov",
        "video/x-flv" => "flv",
        "video/x-matroska" => "mkv",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/mpeg" | "audio/mp3" | "mp3" => "mp3",
        "audio/ogg" | "ogg" => "ogg",
        "audio/opus" | "opus" => "opus",
        "audio/aac" | "audio/aacp" | "aac" => "aac",
        "audio/flac" | "audio/x-flac" | "flac" => "flac",
        "audio/wav" | "audio/x-wav" | "audio/wave" | "wav" => "wav",
        "application/vnd.apple.mpegurl"
        | "application/x-mpegurl"
        | "audio/mpegurl"
        | "audio/x-mpegurl"
        | "hls"
        | "m3u8" => "m3u8",
        "application/dash+xml" | "dash" | "mpd" => "mpd",
        _ => "",
    };
    let extension = url
        .path()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let supported = [
        "mp4", "webm", "mkv", "mov", "m4a", "mp3", "ogg", "flv", "flac", "wav", "aac", "opus",
        "ape", "m3u8", "mpd", "jpg", "png", "gif", "svg", "js", "html", "vtt", "srt",
    ];
    (
        if !mime_ext.is_empty() {
            mime_ext.to_owned()
        } else if supported.contains(&extension.as_str()) {
            extension
        } else {
            String::new()
        },
        mime.starts_with("audio/") && mime_ext != "m3u8",
    )
}

fn resolve_url(base: &Url, raw: &str) -> Option<Url> {
    let decoded = decode_entities(raw);
    let raw = decoded.trim();
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return None;
    }
    let joined = base.join(raw).ok()?;
    crate::validation::parse_url(joined.as_str()).ok()
}

fn is_embed_path(url: &Url) -> bool {
    url.path().split('/').any(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            "embed" | "player" | "iframe"
        )
    })
}

fn choose_embed(page: &Url, urls: Vec<Url>) -> Result<Option<Url>> {
    let mut identities = HashSet::new();
    let mut chosen = None;
    for url in urls {
        if url == *page {
            continue;
        }
        let identity = embed_identity(&url);
        if identities.insert(identity) {
            ensure!(
                chosen.is_none(),
                "网页包含多个不同播放器，请提供单个作品或播放器链接"
            );
            chosen = Some(url);
        }
    }
    Ok(chosen)
}

fn embed_identity(url: &Url) -> String {
    if crate::routing::platform(url.as_str()) == Some("YouTube") {
        let id = if matches!(url.host_str(), Some("youtu.be" | "www.youtu.be")) {
            url.path()
                .trim_matches('/')
                .split('/')
                .next()
                .map(str::to_owned)
        } else {
            url.query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, value)| value.to_string())
                .or_else(|| {
                    url.path()
                        .split('/')
                        .filter(|part| !part.is_empty())
                        .nth(1)
                        .map(str::to_owned)
                })
        };
        if let Some(id) = id {
            return format!("YouTube:{id}");
        }
    }
    let mut normalized = url.clone();
    normalized.set_fragment(None);
    normalized.to_string()
}

fn media_identity(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return raw.to_owned();
    };
    // Secure/ordinary OpenGraph aliases for one media URL are alternatives.
    if url.scheme() == "http" {
        let _ = url.set_scheme("https");
    }
    url.set_fragment(None);
    url.to_string()
}

fn stable_id(prefix: &str, raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    format!("{prefix}-{:x}", digest)[..prefix.len() + 1 + 24].to_owned()
}

fn clean_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(offset) = value[cursor..].find('&') {
        let start = cursor + offset;
        output.push_str(&value[cursor..start]);
        let end = value[start..]
            .find(';')
            .filter(|end| *end <= 12)
            .map(|end| start + end);
        let decoded = end.and_then(|end| {
            let entity = &value[start + 1..end];
            match entity {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "nbsp" => Some(' '),
                "sol" => Some('/'),
                "colon" => Some(':'),
                _ => {
                    let number = entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .and_then(|number| u32::from_str_radix(number, 16).ok())
                        .or_else(|| {
                            entity
                                .strip_prefix('#')
                                .and_then(|number| number.parse().ok())
                        });
                    number.and_then(char::from_u32)
                }
            }
        });
        if let (Some(end), Some(character)) = (end, decoded) {
            output.push(character);
            cursor = end + 1;
        } else {
            output.push('&');
            cursor = start + 1;
        }
    }
    output.push_str(&value[cursor..]);
    output
}

struct Tag {
    name: String,
    attrs: HashMap<String, String>,
    closing: bool,
    self_closing: bool,
}
impl Tag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.get(name).map(String::as_str)
    }
}

fn next_tag(html: &str, cursor: &mut usize) -> Option<Tag> {
    let bytes = html.as_bytes();
    while *cursor < bytes.len() {
        let start = *cursor + html[*cursor..].find('<')?;
        *cursor = start + 1;
        if html[start..].starts_with("<!--") {
            *cursor = html[start + 4..]
                .find("-->")
                .map_or(bytes.len(), |end| start + 4 + end + 3);
            continue;
        }
        let mut position = start + 1;
        let closing = bytes.get(position) == Some(&b'/');
        if closing {
            position += 1;
        }
        let name_start = position;
        while bytes
            .get(position)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            position += 1;
        }
        if position == name_start {
            continue;
        }
        let name = html[name_start..position].to_ascii_lowercase();
        let mut attrs = HashMap::new();
        let mut self_closing = false;
        loop {
            while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
                position += 1;
            }
            match bytes.get(position) {
                Some(b'>') => {
                    position += 1;
                    break;
                }
                Some(b'/') if bytes.get(position + 1) == Some(&b'>') => {
                    self_closing = true;
                    position += 2;
                    break;
                }
                None => {
                    *cursor = bytes.len();
                    return None;
                }
                _ => {}
            }
            let start = position;
            while bytes
                .get(position)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || b":_-.".contains(byte))
            {
                position += 1;
            }
            if start == position {
                position += 1;
                continue;
            }
            let key = html[start..position].to_ascii_lowercase();
            while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
                position += 1;
            }
            let mut value = String::new();
            if bytes.get(position) == Some(&b'=') {
                position += 1;
                while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
                    position += 1;
                }
                match bytes.get(position).copied() {
                    Some(quote @ (b'"' | b'\'')) => {
                        position += 1;
                        let start = position;
                        while bytes.get(position).is_some_and(|byte| *byte != quote) {
                            position += 1;
                        }
                        value = html[start..position].to_owned();
                        if position < bytes.len() {
                            position += 1;
                        }
                    }
                    Some(_) => {
                        let start = position;
                        while bytes
                            .get(position)
                            .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>')
                        {
                            position += 1;
                        }
                        value = html[start..position].to_owned();
                    }
                    None => {}
                }
            }
            // HTML uses the first value when an attribute appears twice.
            attrs.entry(key).or_insert(value);
        }
        *cursor = position;
        return Some(Tag {
            name,
            attrs,
            closing,
            self_closing,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(html: &str) -> Result<GenericResource> {
        parse_page(
            &Url::parse("https://example.test/article/page?token=secret").unwrap(),
            html,
        )
    }

    #[test]
    fn html5_sources_preserve_signatures_containers_and_alternatives() {
        let resource = page(r#"<TITLE> Rust &amp; media </TITLE><base href="/files/">
            <video controls><source src="movie-720.mp4?sig=a%2Bb&amp;expires=123" type='video/mp4' label="720p">
            <source src="movie-1080.webm?sig=a&#x26;b=c" type="video/webm" data-res="1080"></video>"#).unwrap();
        assert_eq!(resource.title, "Rust & media");
        assert_eq!(resource.candidates.len(), 2);
        assert_eq!(
            resource.candidates[0].url,
            "https://example.test/files/movie-720.mp4?sig=a%2Bb&expires=123"
        );
        assert_eq!(resource.candidates[1].extension, "webm");
        assert_eq!(resource.candidates[1].height, 1080);
        assert!(resource
            .candidates
            .iter()
            .all(|candidate| !candidate.label.contains("sig")));
        assert_eq!(
            resource.candidates[0].headers[1].1,
            "https://example.test/article/page"
        );
        assert_eq!(
            resource.candidates[0].id,
            page(r#"<video src="/files/movie-720.mp4?sig=a%2Bb&amp;expires=123"></video>"#)
                .unwrap()
                .candidates[0]
                .id
        );
    }

    #[test]
    fn audio_and_unknown_container_are_not_renamed_as_mp4() {
        let resource = page(r#"<audio src="/track?token=xyz" type="audio/mpeg"></audio>"#).unwrap();
        assert!(resource.candidates[0].audio_only);
        assert_eq!(resource.candidates[0].extension, "mp3");
        let unknown = page(r#"<video src="/stream?id=123"></video>"#).unwrap();
        assert!(unknown.candidates[0].extension.is_empty());
        let ogg_video = page(r#"<video src="/movie.ogg" type="video/ogg"></video>"#).unwrap();
        assert!(!ogg_video.candidates[0].audio_only);
        assert_eq!(ogg_video.candidates[0].extension, "ogg");
    }

    #[test]
    fn unrelated_players_and_unsafe_schemes_are_rejected() {
        assert!(page(r#"<video src="/one.mp4"></video><video src="/two.mp4"></video>"#).is_err());
        for scheme in [
            "javascript:alert(1)",
            "file:///tmp/movie.mp4",
            "data:video/mp4,AA",
            "https://user:pass@example.test/a.mp4",
        ] {
            assert!(page(&format!("<video src=\"{scheme}\"></video>")).is_err());
        }
        assert!(page(r#"<!-- <video src="/ad.mp4"> --><script>const s='<video src="/ad.mp4">';</script><style>x::after{content:'<video src="/ad.mp4">';}</style>"#).is_err());
    }

    #[test]
    fn open_graph_secure_source_and_twitter_card_are_structured_media() {
        let resource = page(
            r#"<meta property="og:title" content="A &amp; B">
            <meta content="video/mp4" property="og:video:type">
            <meta property="og:video" content="http://cdn.example.test/movie.mp4">
            <meta property="og:video:secure_url" content="https://cdn.example.test/movie.mp4">"#,
        )
        .unwrap();
        assert_eq!(resource.title, "A & B");
        assert_eq!(resource.candidates.len(), 2);
        let audio = page(r#"<meta name="og:audio" content="/song"><meta property="og:audio:type" content="audio/flac">"#).unwrap();
        assert_eq!(audio.candidates[0].extension, "flac");
        assert!(audio.candidates[0].audio_only);
        assert!(page(r#"<meta property="og:video" content="/one.mp4"><meta property="og:video" content="/two.mp4">"#).is_err());
    }

    #[test]
    fn json_ld_graph_supports_single_media_and_rejects_unrelated_items() {
        assert_eq!(parse_duration(&serde_json::json!("PT1M2.5S")), Some(63));
        let resource = page(r#"<script type="application/ld+json">{"@graph":[{"@type":["CreativeWork","VideoObject"],"name":"Schema video","duration":"PT1M2.5S","contentUrl":"/movie.m3u8?sig=abcd"}]}</script>"#).unwrap();
        assert_eq!(resource.duration_seconds, Some(63));
        assert_eq!(resource.title, "Schema video");
        assert_eq!(resource.candidates[0].extension, "m3u8");
        assert!(page(r#"<script type="application/ld+json">[{"@type":"VideoObject","contentUrl":"/a.mp4"},{"@type":"VideoObject","contentUrl":"/b.mp4"}]</script>"#).is_err());
        let audio = page(r#"<script type="application/ld+json">{"@type":"https://schema.org/AudioObject","contentUrl":"/song","encodingFormat":"audio/ogg"}</script>"#).unwrap();
        assert!(audio.candidates[0].audio_only);
        assert_eq!(audio.candidates[0].extension, "ogg");
    }

    #[test]
    fn jwplayer_parses_json_and_never_executes_javascript() {
        let resource = page(r#"<script>jwplayer('main').setup({"playlist":[{"title":"JW movie","duration":4,"sources":[{"file":"/480.mp4","label":"480p"},{"file":"/720.webm","label":"720p","type":"video/webm"}]}]});</script>"#).unwrap();
        assert_eq!(resource.title, "JW movie");
        assert_eq!(resource.candidates.len(), 2);
        assert_eq!(resource.candidates[1].height, 720);
        assert!(page(
            r#"<script>jwplayer('main').setup({file:'/movie.mp4',sources:evil()});</script>"#
        )
        .is_err());
        assert!(page(
            r#"<script type="application/json">{"unrelated":{"file":"/ad.mp4"}}</script>"#
        )
        .is_err());
        assert!(page(r#"<script type="application/json">{"playlist":[{"file":"/a.mp4"},{"file":"/b.mp4"}]}</script>"#).is_err());
    }

    #[test]
    fn known_embeds_deduplicate_by_video_identity() {
        let resource = page(r#"<iframe src="https://www.youtube.com/embed/YE7VzlLtp-4?autoplay=1"></iframe>
            <meta name="twitter:player" content="https://www.youtube.com/embed/YE7VzlLtp-4?autoplay=0">"#).unwrap();
        assert_eq!(resource.embed.unwrap().host_str(), Some("www.youtube.com"));
        assert!(resource.candidates.is_empty());
        assert!(page(r#"<iframe src="https://www.youtube.com/embed/YE7VzlLtp-4"></iframe><iframe src="https://www.youtube.com/embed/SECOND_VIDEO"></iframe>"#).is_err());
        assert!(page(r#"<iframe src="https://ads.example.test/frame"></iframe>"#).is_err());
    }

    #[test]
    fn candidate_count_and_document_size_are_bounded() {
        let sources = (0..40)
            .map(|n| format!("<source src=\"/{n}.mp4\">"))
            .collect::<String>();
        assert!(page(&format!("<video>{sources}</video>")).is_err());
        assert!(page(&"x".repeat(MAX_PAGE_BYTES + 1)).is_err());
    }

    #[tokio::test]
    async fn http_fetch_uses_final_relative_base_and_does_not_forward_cookie() {
        use axum::{
            http::{HeaderMap, StatusCode},
            response::{Html, IntoResponse},
            routing::get,
            Router,
        };
        let app = Router::new()
            .route("/start", get(|| async { (StatusCode::FOUND, [("location", "/nested/page"), ("set-cookie", "private=must-not-forward; Path=/")]).into_response() }))
            .route("/nested/page", get(|headers: HeaderMap| async move {
                assert!(headers.get(header::COOKIE).is_none());
                Html(r#"<video><source src="clip.webm?signature=a%2Bb&amp;t=1" type="video/webm"></video>"#).into_response()
            }))
            .route("/large", get(|| async { Html("x".repeat(MAX_PAGE_BYTES + 1)) }))
            .route("/binary", get(|| async { ([(header::CONTENT_TYPE, "application/octet-stream")], "not html") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let resolver = GenericResolver::new().unwrap();
        let result = resolver
            .resolve(&Url::parse(&format!("http://{address}/start")).unwrap())
            .await
            .unwrap();
        assert_eq!(
            result.candidates[0].url,
            format!("http://{address}/nested/clip.webm?signature=a%2Bb&t=1")
        );
        assert!(resolver
            .resolve(&Url::parse(&format!("http://{address}/large")).unwrap())
            .await
            .is_err());
        assert!(resolver
            .resolve(&Url::parse(&format!("http://{address}/binary")).unwrap())
            .await
            .is_err());
        server.abort();
    }
}
