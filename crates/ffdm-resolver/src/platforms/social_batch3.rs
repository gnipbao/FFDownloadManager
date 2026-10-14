//! Public single-post social media adapters. Tumblr's native player metadata
//! and provider delegation follow the extraction approach of yt-dlp's
//! tumblr.py. Mastodon uses the documented Status / MediaAttachment API.
//! No login token discovery, page JavaScript, or foreign extractor is run.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{bail, ensure, Context, Result};
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Duration};

const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_CANDIDATES: usize = 32;

pub(crate) struct SocialBatch3Resolver {
    client: Client,
    local_client: Client,
}

impl SocialBatch3Resolver {
    pub(crate) fn new() -> Result<Self> {
        let make = || {
            Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .user_agent(crate::resource::USER_AGENT)
                .redirect(Policy::none())
        };
        Ok(Self {
            client: make().build()?,
            local_client: make().no_proxy().build()?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        let input = crate::validation::parse_url(input.as_str())?;
        ensure!(
            input.scheme() == "https" || is_loopback(&input),
            "公开作品需要 HTTPS；仅本地测试支持 HTTP"
        );
        match platform {
            "Mastodon" => self.mastodon(&input).await,
            "Tumblr" => self.tumblr(&input).await,
            _ => bail!("暂不支持这个公开社交平台"),
        }
    }

    async fn fetch(&self, url: &Url, platform: &str, accept: &str) -> Result<String> {
        crate::validation::parse_url(url.as_str())?;
        let client = if is_loopback(url) {
            &self.local_client
        } else {
            &self.client
        };
        let mut response = client
            .get(url.clone())
            .header(header::ACCEPT, accept)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|error| {
                anyhow::anyhow!("{platform} 公开作品接口连接失败：{}", error.without_url())
            })?;
        let status = response.status();
        match status.as_u16() {
            401 | 403 => bail!("{platform} 作品需要授权或当前网络无法访问"),
            404 | 410 => bail!("{platform} 作品不存在或已经删除"),
            429 => bail!("{platform} 请求过于频繁，请稍后重试"),
            300..=399 => bail!("{platform} 接口跳转到了其他页面，无法确认公开作品来源"),
            _ => ensure!(status.is_success(), "{platform} 接口返回 HTTP {status}"),
        }
        ensure!(
            response.url().origin() == url.origin(),
            "作品接口来源发生变化"
        );
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_METADATA_BYTES as u64,
            "{platform} 播放信息超过 2 MiB 上限"
        );
        ensure!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .is_none_or(|value| { value.as_bytes().eq_ignore_ascii_case(b"identity") }),
            "{platform} 接口返回了不支持的压缩编码"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| anyhow::anyhow!("读取公开播放信息失败：{}", error.without_url()))?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_METADATA_BYTES,
                "{platform} 播放信息超过 2 MiB 上限"
            );
            bytes.extend_from_slice(&chunk);
        }
        String::from_utf8(bytes).context("公开播放信息不是有效的 UTF-8")
    }

    async fn mastodon(&self, input: &Url) -> Result<GenericResource> {
        let (author, id) = mastodon_post(input).context("请输入 Mastodon 单个公开帖子链接")?;
        // The caller discovers arbitrary instances only after an explicit HTML
        // page inspection. This endpoint is always the exact same origin.
        let endpoint = input.join(&format!("/api/v1/statuses/{id}"))?;
        let text = self
            .fetch(&endpoint, "Mastodon", "application/json")
            .await?;
        let data: Value =
            serde_json::from_str(&text).context("这个网站没有返回 Mastodon 公开帖子信息")?;
        parse_mastodon(&data, &author, &id, input)
    }

    async fn tumblr(&self, input: &Url) -> Result<GenericResource> {
        let (blog, id) = tumblr_post(input).context("请输入 Tumblr 单个公开帖子链接")?;
        let page = Url::parse(&format!("https://{blog}.tumblr.com/post/{id}"))?;
        let mut endpoint = page.join("/api/read/json")?;
        endpoint
            .query_pairs_mut()
            .append_pair("id", &id)
            .append_pair("num", "1");
        // The public legacy read endpoint needs no application key. Parse only
        // its fixed JSON assignment; never evaluate the returned JavaScript.
        match self
            .fetch(&endpoint, "Tumblr", "application/json,text/javascript")
            .await
        {
            Ok(text) => {
                if let Ok(mut data) = tumblr_json(&text) {
                    let target = tumblr_target(&data, &blog, &id)?;
                    let result = parse_tumblr(&data, &blog, &id, &page);
                    if result.is_ok() {
                        return result;
                    }
                    if target["content"].is_null() {
                        if let Some(html) = nonempty(&target["video-player"]) {
                            if let Some(iframe) = tumblr_native_iframe(html, &blog, &id, &page)? {
                                let player = self.fetch(&iframe, "Tumblr", "text/html").await?;
                                // Preserve the validated target identity, replacing
                                // only its exact native player HTML.
                                for post in data["posts"].as_array_mut().unwrap() {
                                    if value_id(&post["id"]).as_deref() == Some(&id) {
                                        post["video-player"] = Value::String(player);
                                        break;
                                    }
                                }
                                return parse_tumblr(&data, &blog, &id, &page);
                            }
                        }
                    }
                    return result;
                }
            }
            Err(error) => {
                // A rate-limit or authorization response is not a reason to
                // hammer a different endpoint or harvest a browser login.
                let message = error.to_string();
                if message.contains("过于频繁") || message.contains("需要授权") {
                    return Err(error);
                }
            }
        }
        let html = self.fetch(&page, "Tumblr", "text/html").await?;
        let (mut resource, iframe) = parse_tumblr_page(&html, &blog, &id, &page)?;
        if let Some(iframe) = iframe {
            let player = self.fetch(&iframe, "Tumblr", "text/html").await?;
            let (candidates, embed, duration) = parse_tumblr_player(&player, &page)?;
            resource.candidates = candidates;
            resource.embed = embed;
            resource.duration_seconds = duration;
        }
        ensure!(
            !resource.candidates.is_empty() || resource.embed.is_some(),
            "Tumblr 作品没有公开音视频，或公开页面需要登录"
        );
        Ok(resource)
    }
}

pub(crate) fn mastodon_post(url: &Url) -> Option<(String, String)> {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let (author, id) = match parts.as_slice() {
        [author, id] if author.starts_with('@') || author.starts_with("%40") => (
            author.trim_start_matches('@').trim_start_matches("%40"),
            *id,
        ),
        ["users", author, "statuses", id] => (*author, *id),
        _ => return None,
    };
    let author = percent_encoding::percent_decode_str(author)
        .decode_utf8()
        .ok()?;
    (valid_author(&author) && number(id)).then(|| (author.into_owned(), id.to_owned()))
}

pub(crate) fn tumblr_post(url: &Url) -> Option<(String, String)> {
    let host = url.host_str()?;
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let (blog, id) = if matches!(host, "tumblr.com" | "www.tumblr.com") {
        match parts.as_slice() {
            ["blog", "view", blog, id, ..] | ["video", blog, id, ..] => (*blog, *id),
            [blog, id, ..] => (*blog, *id),
            _ => return None,
        }
    } else {
        let blog = host.strip_suffix(".tumblr.com")?;
        if matches!(blog, "media" | "assets" | "api" | "va" | "vtt" | "www") {
            return None;
        }
        match parts.as_slice() {
            ["post" | "video", id, ..] => (blog, *id),
            _ => return None,
        }
    };
    (blog.len() <= 63
        && !blog.is_empty()
        && blog
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && number(id))
    .then(|| (blog.to_ascii_lowercase(), id.to_owned()))
}

fn valid_author(author: &str) -> bool {
    author.len() <= 320
        && !author.is_empty()
        && author.split('@').count() <= 2
        && author.split('@').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        })
}

fn number(value: &str) -> bool {
    !value.is_empty() && value.len() <= 32 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn parse_mastodon(data: &Value, author: &str, id: &str, page: &Url) -> Result<GenericResource> {
    ensure!(
        value_id(&data["id"]).as_deref() == Some(id),
        "Mastodon 返回的帖子与请求不一致"
    );
    let account = &data["account"];
    let actual_author = nonempty(&account["acct"]).context("Mastodon 作者信息缺失")?;
    ensure!(
        actual_author.eq_ignore_ascii_case(author),
        "Mastodon 返回的帖子作者与请求不一致"
    );
    ensure!(
        matches!(nonempty(&data["visibility"]), Some("public" | "unlisted")),
        "Mastodon 作品需要登录或额外授权"
    );
    ensure!(
        data["reblog"].is_null(),
        "Mastodon 转发帖不作为当前作品下载，请使用原始作品链接"
    );
    ensure!(data["deleted_at"].is_null(), "Mastodon 作品已经删除");
    let attachments = data["media_attachments"]
        .as_array()
        .context("Mastodon 作品没有公开媒体附件")?;
    ensure!(
        attachments.len() <= MAX_CANDIDATES,
        "Mastodon 附件数量超过解析上限"
    );
    let playable: Vec<_> = attachments
        .iter()
        .filter(|attachment| {
            matches!(
                nonempty(&attachment["type"]),
                Some("video" | "gifv" | "audio")
            )
        })
        .collect();
    ensure!(
        !playable.is_empty(),
        "这个 Mastodon 帖子本身没有公开音视频；引用的作品不会被下载"
    );
    ensure!(
        playable.len() == 1,
        "这个 Mastodon 帖子包含多个音视频附件，暂不支持将不同附件当作清晰度下载"
    );
    let attachment = playable[0];
    let raw =
        nonempty(&attachment["url"]).context("Mastodon 媒体尚未处理完成或未提供公开下载地址")?;
    let url = safe_media_url(page, raw)?;
    let extension = media_extension(&url, None).context("Mastodon 附件格式暂不支持")?;
    let audio_only = nonempty(&attachment["type"]) == Some("audio");
    let duration_seconds = duration_seconds(&attachment["meta"]["original"]["duration"])
        .or_else(|| duration_seconds(&attachment["meta"]["duration"]));
    let height = attachment["meta"]["original"]["height"]
        .as_u64()
        .or_else(|| attachment["meta"]["height"].as_u64())
        .unwrap_or(0)
        .min(16384) as u32;
    let source_id = stable_id(
        "mastodon",
        &format!("{}:{id}", page.origin().ascii_serialization()),
    );
    let title = nonempty(&data["spoiler_text"])
        .or_else(|| nonempty(&data["content"]))
        .or_else(|| nonempty(&attachment["description"]))
        .unwrap_or(author);
    Ok(GenericResource {
        id: source_id.clone(),
        title: clean_text(title),
        duration_seconds,
        candidates: vec![GenericCandidate {
            id: format!("{source_id}-original"),
            label: if audio_only {
                format!("音频 · {}", extension.to_ascii_uppercase())
            } else if height > 0 {
                format!("{height}p · {}", extension.to_ascii_uppercase())
            } else {
                format!("原始视频 · {}", extension.to_ascii_uppercase())
            },
            height,
            url: url.to_string(),
            extension: extension.to_owned(),
            audio_only,
            headers: media_headers(page),
            size: None,
        }],
        embed: None,
    })
}

fn tumblr_json(text: &str) -> Result<Value> {
    ensure!(
        text.len() <= MAX_METADATA_BYTES,
        "Tumblr 播放信息超过解析上限"
    );
    let text = text.trim();
    let json = if text.starts_with('{') {
        text
    } else {
        let body = text
            .strip_prefix("var tumblr_api_read")
            .context("Tumblr 公开接口没有返回播放 JSON")?
            .trim_start();
        let body = body
            .strip_prefix('=')
            .context("Tumblr 播放 JSON 包装无效")?
            .trim_start();
        body.strip_suffix(';').unwrap_or(body).trim_end()
    };
    serde_json::from_str(json).context("Tumblr 没有返回有效播放 JSON")
}

fn tumblr_target<'a>(data: &'a Value, blog: &str, id: &str) -> Result<&'a Value> {
    ensure!(
        nonempty(&data["tumblelog"]["name"]).is_some_and(|name| name.eq_ignore_ascii_case(blog)),
        "Tumblr 返回的博客与请求不一致"
    );
    let posts = data["posts"]
        .as_array()
        .context("Tumblr 公开作品不存在或页面需要登录")?;
    ensure!(posts.len() <= 32, "Tumblr 帖子数量超过解析上限");
    let mut matching = posts
        .iter()
        .filter(|post| value_id(&post["id"]).as_deref() == Some(id));
    let post = matching.next().context("Tumblr 返回的帖子与请求不一致")?;
    ensure!(matching.next().is_none(), "Tumblr 返回了重复帖子标识");
    if let Some(name) = nonempty(&post["tumblelog"]["name"]) {
        ensure!(
            name.eq_ignore_ascii_case(blog),
            "Tumblr 作品作者与请求不一致"
        );
    }
    let post_url = nonempty(&post["url"])
        .or_else(|| nonempty(&post["post_url"]))
        .context("Tumblr 作品原始链接缺失")?;
    let post_url = crate::validation::parse_url(post_url)?;
    ensure!(
        tumblr_post(&post_url) == Some((blog.to_owned(), id.to_owned())),
        "Tumblr 返回的作品来源与请求不一致"
    );
    ensure!(
        nonempty(&post["state"]).is_none_or(|state| state == "published"),
        "Tumblr 作品尚未公开"
    );
    ensure!(
        post["is_private"] != Value::Bool(true) && post["requires_login"] != Value::Bool(true),
        "Tumblr 作品需要登录"
    );
    Ok(post)
}

fn parse_tumblr(data: &Value, blog: &str, id: &str, page: &Url) -> Result<GenericResource> {
    let post = tumblr_target(data, blog, id)?;
    let title = [
        "summary",
        "video-caption",
        "audio-caption",
        "regular-title",
        "caption",
    ]
    .iter()
    .find_map(|key| nonempty(&post[*key]))
    .unwrap_or(blog);
    let (candidates, embed, duration) = if let Some(content) = post["content"].as_array() {
        parse_tumblr_npf(content, page)?
    } else {
        let html = nonempty(&post["video-player"])
            .or_else(|| nonempty(&post["audio-player"]))
            .context("这个 Tumblr 帖子没有公开的单个音视频播放器")?;
        parse_tumblr_player(html, page)?
    };
    ensure!(
        !candidates.is_empty() || embed.is_some(),
        "Tumblr 没有可公开下载的音视频或已支持的嵌入平台"
    );
    let source_id = stable_id("tumblr", &format!("{blog}:{id}"));
    let candidates = candidates
        .into_iter()
        .enumerate()
        .map(|(index, mut candidate)| {
            candidate.id = format!("{source_id}-{}", index + 1);
            candidate
        })
        .collect();
    Ok(GenericResource {
        id: source_id,
        title: clean_text(title),
        duration_seconds: duration,
        candidates,
        embed,
    })
}

type PlayerResource = (Vec<GenericCandidate>, Option<Url>, Option<u64>);

fn tumblr_native_iframe(html: &str, blog: &str, id: &str, page: &Url) -> Result<Option<Url>> {
    ensure!(
        html.len() <= MAX_METADATA_BYTES,
        "Tumblr 播放器超过解析上限"
    );
    let tags = Regex::new(r"(?is)<(video|audio|source|iframe)\b[^>]*>")?;
    let mut iframe = None;
    let mut count = 0;
    for (index, capture) in tags.captures_iter(html).enumerate() {
        ensure!(index < 256, "Tumblr 播放器标签超过解析上限");
        if !capture[1].eq_ignore_ascii_case("iframe") {
            return Ok(None);
        }
        count += 1;
        ensure!(
            count <= 1,
            "Tumblr 作品包含多个嵌入播放器，暂不支持批量解析"
        );
        if let Some(raw) = attribute(&capture[0], "src")? {
            let url = safe_media_url(page, &decode_entities(&raw))?;
            if url.host_str() == Some("www.tumblr.com")
                && url.port_or_known_default() == Some(443)
                && url.path().starts_with(&format!("/video/{blog}/{id}/"))
            {
                iframe = Some(url);
            }
        }
    }
    Ok(iframe)
}

fn parse_tumblr_npf(content: &[Value], page: &Url) -> Result<PlayerResource> {
    ensure!(content.len() <= 128, "Tumblr 内容区块超过解析上限");
    let playable: Vec<_> = content
        .iter()
        .filter(|block| matches!(nonempty(&block["type"]), Some("video" | "audio")))
        .collect();
    ensure!(
        playable.len() == 1,
        "Tumblr 作品含有多个音视频或没有音视频，请使用单个作品链接"
    );
    let block = playable[0];
    ensure!(
        block["drm"].is_null() || block["drm"] == Value::Bool(false),
        "不支持受 DRM 保护的 Tumblr 媒体"
    );
    let audio_only = nonempty(&block["type"]) == Some("audio");
    let duration = block["duration"]
        .as_u64()
        .map(|milliseconds| milliseconds / 1000);
    if let Some(raw) = nonempty(&block["media"]["url"]) {
        let candidate = native_tumblr_candidate(
            page,
            raw,
            nonempty(&block["media"]["type"]),
            block["media"]["height"].as_u64().unwrap_or(0).min(16384) as u32,
            audio_only,
        )?;
        return Ok((vec![candidate], None, duration));
    }
    if let Some(raw) = nonempty(&block["url"]) {
        let url = safe_media_url(page, raw)?;
        if crate::routing::platform(url.as_str())
            .is_some_and(|name| name != "Tumblr" && name != "HLS")
        {
            return Ok((Vec::new(), Some(url), duration));
        }
    }
    bail!("Tumblr 作品没有公开原始媒体或已支持的嵌入平台")
}

fn parse_tumblr_player(html: &str, page: &Url) -> Result<PlayerResource> {
    ensure!(
        html.len() <= MAX_METADATA_BYTES,
        "Tumblr 播放器超过解析上限"
    );
    let tags = Regex::new(r"(?is)<(video|audio|source|iframe|meta)\b[^>]*>")?;
    let mut candidates = Vec::new();
    let mut embeds = Vec::new();
    let mut seen = HashSet::new();
    let mut player_count = 0;
    let mut audio_only = false;
    let mut duration = None;
    for (tag_index, capture) in tags.captures_iter(html).enumerate() {
        ensure!(tag_index < 256, "Tumblr 播放器标签超过解析上限");
        ensure!(
            candidates.len() + embeds.len() < MAX_CANDIDATES,
            "Tumblr 媒体数量超过解析上限"
        );
        let tag = &capture[0];
        let name = capture[1].to_ascii_lowercase();
        if matches!(name.as_str(), "video" | "audio") {
            player_count += 1;
            ensure!(
                player_count == 1,
                "Tumblr 作品包含多个播放器，不能把不同作品当作清晰度下载"
            );
            audio_only = name == "audio";
            if let Some(options) = attribute(tag, "data-crt-options")? {
                let options: Value = serde_json::from_str(&decode_entities(&options))
                    .context("Tumblr 播放器信息无效")?;
                duration = duration_seconds(&options["duration"]);
                if let Some(raw) = nonempty(&options["hdUrl"]) {
                    let candidate = native_tumblr_candidate(
                        page,
                        raw,
                        Some("video/mp4"),
                        height_from_path(raw),
                        false,
                    )?;
                    if seen.insert(candidate.url.clone()) {
                        candidates.push(candidate);
                    }
                }
            }
        }
        match name.as_str() {
            "video" | "audio" | "source" => {
                if let Some(raw) = attribute(tag, "src")? {
                    let raw = decode_entities(&raw);
                    let mime = attribute(tag, "type")?;
                    let candidate = native_tumblr_candidate(
                        page,
                        &raw,
                        mime.as_deref(),
                        height_from_path(&raw),
                        audio_only,
                    )?;
                    if seen.insert(candidate.url.clone()) {
                        candidates.push(candidate);
                    }
                }
            }
            "iframe" => {
                if let Some(raw) = attribute(tag, "src")? {
                    let url = safe_media_url(page, &decode_entities(&raw))?;
                    if crate::routing::platform(url.as_str())
                        .is_some_and(|platform| platform != "Tumblr" && platform != "HLS")
                        && !embeds.contains(&url)
                    {
                        embeds.push(url);
                    }
                }
            }
            _ => (),
        }
    }
    ensure!(
        embeds.len() <= 1,
        "Tumblr 作品包含多个嵌入视频，暂不支持批量解析"
    );
    // A native video is the selected post's media; external embeds are only a
    // fallback. Thumbnail tags and unrelated provider URLs are not formats.
    let embed = if candidates.is_empty() {
        embeds.pop()
    } else {
        None
    };
    Ok((candidates, embed, duration))
}

fn parse_tumblr_page(
    html: &str,
    blog: &str,
    id: &str,
    page: &Url,
) -> Result<(GenericResource, Option<Url>)> {
    ensure!(html.len() <= MAX_METADATA_BYTES, "Tumblr 页面超过解析上限");
    let tags = Regex::new(r"(?is)<(?:meta|link|iframe)\b[^>]*>")?;
    let mut canonical_matches = false;
    let mut title = blog.to_owned();
    let mut video_urls = Vec::new();
    let mut iframe = None;
    for (tag_index, tag) in tags.find_iter(html).enumerate() {
        ensure!(tag_index < 1024, "Tumblr 页面标签超过解析上限");
        let tag = tag.as_str();
        let property = attribute(tag, "property")?
            .or_else(|| attribute(tag, "name").ok().flatten())
            .unwrap_or_default();
        let content = attribute(tag, "content")?.map(|value| decode_entities(&value));
        if property.eq_ignore_ascii_case("og:url")
            || attribute(tag, "rel")?.is_some_and(|rel| rel.eq_ignore_ascii_case("canonical"))
        {
            if let Some(raw) = content.as_deref().or(attribute(tag, "href")?.as_deref()) {
                if let Ok(url) = page.join(raw) {
                    ensure!(
                        tumblr_post(&url) == Some((blog.to_owned(), id.to_owned())),
                        "Tumblr 页面跳转到了其他作品或登录页面"
                    );
                    canonical_matches = true;
                }
            }
        }
        if property.eq_ignore_ascii_case("og:title") {
            if let Some(content) = &content {
                title = clean_text(content);
            }
        }
        if matches!(
            property.to_ascii_lowercase().as_str(),
            "og:video" | "og:video:url" | "og:video:secure_url"
        ) {
            if let Some(content) = content {
                video_urls.push(content);
            }
        }
        if let Some(raw) = attribute(tag, "src")? {
            let url = page.join(&decode_entities(&raw))?;
            if url.host_str() == Some("www.tumblr.com")
                && url.path().starts_with(&format!("/video/{blog}/{id}/"))
            {
                ensure!(
                    iframe.is_none() || iframe.as_ref() == Some(&url),
                    "Tumblr 页面包含多个不同播放器"
                );
                iframe = Some(url);
            }
        }
    }
    ensure!(
        canonical_matches,
        "Tumblr 页面没有可验证的目标作品标识，可能需要登录"
    );
    let mut candidates = Vec::new();
    let mut embed = None;
    let mut seen = HashSet::new();
    ensure!(
        video_urls.len() <= MAX_CANDIDATES,
        "Tumblr 视频元数据超过解析上限"
    );
    for raw in video_urls {
        let url = safe_media_url(page, &raw)?;
        if native_tumblr_host(&url) {
            let candidate =
                native_tumblr_candidate(page, &raw, None, height_from_path(&raw), false)?;
            if seen.insert(candidate.url.clone()) {
                candidates.push(candidate);
            }
        } else if crate::routing::platform(url.as_str())
            .is_some_and(|platform| platform != "Tumblr" && platform != "HLS")
        {
            ensure!(
                embed.is_none() || embed.as_ref() == Some(&url),
                "Tumblr 作品包含多个不同嵌入视频"
            );
            embed = Some(url);
        } else if url.host_str() == Some("www.tumblr.com")
            && url.path().starts_with(&format!("/video/{blog}/{id}/"))
        {
            ensure!(
                iframe.is_none() || iframe.as_ref() == Some(&url),
                "Tumblr 作品包含多个不同播放器"
            );
            iframe = Some(url);
        }
    }
    if !candidates.is_empty() {
        embed = None;
    }
    Ok((
        GenericResource {
            id: stable_id("tumblr", &format!("{blog}:{id}")),
            title,
            duration_seconds: None,
            candidates,
            embed,
        },
        iframe,
    ))
}

fn attribute(tag: &str, name: &str) -> Result<Option<String>> {
    let regex = Regex::new(&format!(
        r#"(?is)(?:\s){}\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
        regex::escape(name)
    ))?;
    Ok(regex
        .captures(tag)
        .and_then(|capture| capture.get(1).or(capture.get(2)).or(capture.get(3)))
        .map(|value| value.as_str().to_owned()))
}

fn native_tumblr_candidate(
    page: &Url,
    raw: &str,
    mime: Option<&str>,
    height: u32,
    audio_only: bool,
) -> Result<GenericCandidate> {
    let url = safe_media_url(page, raw)?;
    ensure!(
        native_tumblr_host(&url),
        "Tumblr 原始媒体地址不在公开媒体服务内"
    );
    let extension = media_extension(&url, mime).context("Tumblr 公开媒体格式暂不支持")?;
    let audio_only = audio_only
        || matches!(
            extension,
            "mp3" | "m4a" | "ogg" | "oga" | "opus" | "aac" | "flac" | "wav"
        );
    Ok(GenericCandidate {
        id: stable_id("tumblr-media", url.as_str()),
        label: if audio_only {
            format!("音频 · {}", extension.to_ascii_uppercase())
        } else if height > 0 {
            format!("{height}p · {}", extension.to_ascii_uppercase())
        } else {
            format!("原始视频 · {}", extension.to_ascii_uppercase())
        },
        height,
        url: url.to_string(),
        extension: extension.to_owned(),
        audio_only,
        headers: media_headers(page),
        size: None,
    })
}

fn native_tumblr_host(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|host| host == "media.tumblr.com" || host.ends_with(".media.tumblr.com"))
}

fn height_from_path(raw: &str) -> u32 {
    Url::parse(raw)
        .ok()
        .and_then(|url| {
            url.path()
                .rsplit('_')
                .next()
                .and_then(|suffix| suffix.split('.').next())
                .and_then(|height| height.parse::<u32>().ok())
        })
        .filter(|height| *height <= 16384)
        .unwrap_or(0)
}

fn safe_media_url(base: &Url, raw: &str) -> Result<Url> {
    ensure!(
        raw.len() <= 16 * 1024 && !raw.trim().is_empty(),
        "媒体地址无效"
    );
    let url = base.join(raw.trim()).context("媒体地址格式无效")?;
    crate::validation::parse_url(url.as_str())?;
    ensure!(
        url.scheme() == "https" || (is_loopback(base) && is_loopback(&url)),
        "公开媒体需要 HTTPS"
    );
    ensure!(url.fragment().is_none(), "媒体地址不能包含 fragment");
    Ok(url)
}

fn media_extension(url: &Url, mime: Option<&str>) -> Option<&'static str> {
    match url.path().rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => Some("mp4"),
        "webm" => Some("webm"),
        "mov" => Some("mov"),
        "mp3" => Some("mp3"),
        "m4a" => Some("m4a"),
        "ogg" => Some("ogg"),
        "oga" => Some("oga"),
        "opus" => Some("opus"),
        "aac" => Some("aac"),
        "flac" => Some("flac"),
        "wav" => Some("wav"),
        _ => match mime?
            .split(';')
            .next()?
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "video/mp4" => Some("mp4"),
            "video/webm" => Some("webm"),
            "audio/mp4" => Some("m4a"),
            "audio/mpeg" => Some("mp3"),
            "audio/ogg" => Some("ogg"),
            _ => None,
        },
    }
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

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
fn value_id(value: &Value) -> Option<String> {
    nonempty(value)
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}
fn duration_seconds(value: &Value) -> Option<u64> {
    value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0 && *number < 31_536_000.0)
        .map(|number| number.round() as u64)
}
fn stable_id(prefix: &str, value: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(value.as_bytes()));
    format!("{prefix}-{}", &digest[..20])
}
fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

fn clean_text(raw: &str) -> String {
    let text = Regex::new(r"(?s)<[^>]*>").map_or_else(
        |_| raw.to_owned(),
        |regex| regex.replace_all(raw, " ").into_owned(),
    );
    decode_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(180)
        .collect()
}

fn decode_entities(raw: &str) -> String {
    raw.replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mastodon_data() -> Value {
        json!({"id":"12345","account":{"acct":"example"},"visibility":"public","reblog":null,"content":"<p>Exact post &amp; video</p>",
            "media_attachments":[{"id":"1","type":"video","url":"https://cdn.example/media/original.mp4?sig=keep%2Bexact",
                "preview_url":"https://cdn.example/thumb.png","meta":{"original":{"height":1080,"duration":12.4}}}],
            "quote":{"quoted_status":{"id":"wrong","media_attachments":[{"type":"video","url":"https://cdn.example/wrong.mp4"}]}}})
    }

    fn tumblr_data() -> Value {
        json!({"tumblelog":{"name":"sample"},"posts":[{"id":"12345","url":"https://sample.tumblr.com/post/12345/title","state":"published",
            "tumblelog":{"name":"sample"},"video-caption":"<p>Exact Tumblr video</p>",
            "video-player":"<video data-crt-options='{\"duration\":7,\"hdUrl\":\"https://va.media.tumblr.com/tumblr_test_720.mp4?sig=keep%2Bexact\"}'><source src='https://va.media.tumblr.com/tumblr_test_480.mp4' type='video/mp4'></video>"}]})
    }

    #[test]
    fn mastodon_requires_exact_post_author_public_visibility_and_own_attachment() {
        let page = Url::parse("https://mastodon.example/@example/12345").unwrap();
        let resource = parse_mastodon(&mastodon_data(), "example", "12345", &page).unwrap();
        assert_eq!(resource.title, "Exact post & video");
        assert_eq!(resource.candidates.len(), 1);
        assert_eq!(resource.candidates[0].height, 1080);
        assert!(resource.candidates[0].url.ends_with("?sig=keep%2Bexact"));
        assert!(parse_mastodon(&mastodon_data(), "other", "12345", &page).is_err());
        assert!(parse_mastodon(&mastodon_data(), "example", "54321", &page).is_err());
        let mut data = mastodon_data();
        data["visibility"] = json!("private");
        assert!(parse_mastodon(&data, "example", "12345", &page).is_err());
        data["visibility"] = json!("public");
        data["reblog"] = json!({"id":"another-post"});
        assert!(parse_mastodon(&data, "example", "12345", &page).is_err());
        data["reblog"] = Value::Null;
        data["media_attachments"] = json!([]);
        assert!(parse_mastodon(&data, "example", "12345", &page).is_err());
    }

    #[test]
    fn mastodon_audio_and_gifv_preserve_original_no_preview_or_multiattachment_substitution() {
        let page = Url::parse("https://mastodon.example/users/example/statuses/12345").unwrap();
        let mut data = mastodon_data();
        data["media_attachments"][0]["type"] = json!("audio");
        data["media_attachments"][0]["url"] = json!("https://cdn.example/original.flac");
        let resource = parse_mastodon(&data, "example", "12345", &page).unwrap();
        assert!(resource.candidates[0].audio_only);
        assert_eq!(resource.candidates[0].extension, "flac");
        data["media_attachments"][0]["type"] = json!("gifv");
        data["media_attachments"][0]["url"] = json!("https://cdn.example/original.mp4");
        assert!(
            !parse_mastodon(&data, "example", "12345", &page)
                .unwrap()
                .candidates[0]
                .audio_only
        );
        let duplicate = data["media_attachments"][0].clone();
        data["media_attachments"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert!(parse_mastodon(&data, "example", "12345", &page).is_err());
    }

    #[test]
    fn tumblr_json_assignment_is_data_only_and_targets_exact_blog_and_post() {
        let page = Url::parse("https://sample.tumblr.com/post/12345").unwrap();
        let data = tumblr_data();
        let parsed = tumblr_json(&format!("var tumblr_api_read = {data};")).unwrap();
        let resource = parse_tumblr(&parsed, "sample", "12345", &page).unwrap();
        assert_eq!(resource.title, "Exact Tumblr video");
        assert_eq!(resource.duration_seconds, Some(7));
        assert_eq!(resource.candidates.len(), 2);
        assert_eq!(resource.candidates[0].height, 720);
        assert!(resource.candidates[0].url.ends_with("?sig=keep%2Bexact"));
        assert!(parse_tumblr(&data, "different", "12345", &page).is_err());
        assert!(parse_tumblr(&data, "sample", "54321", &page).is_err());
        assert!(tumblr_json(&format!("var tumblr_api_read = {data}; evil();")).is_err());
        let mut data = data.clone();
        data["posts"][0]["state"] = json!("private");
        assert!(parse_tumblr(&data, "sample", "12345", &page).is_err());
    }

    #[test]
    fn tumblr_player_keeps_native_formats_and_delegates_only_supported_single_embed() {
        let page = Url::parse("https://sample.tumblr.com/post/12345").unwrap();
        let (formats, embed, _) = parse_tumblr_player(
            "<iframe src='https://www.youtube.com/embed/dQw4w9WgXcQ'></iframe>",
            &page,
        )
        .unwrap();
        assert!(formats.is_empty());
        assert_eq!(embed.unwrap().host_str(), Some("www.youtube.com"));
        assert!(parse_tumblr_player("<video><source src='https://va.media.tumblr.com/one.mp4'></video><video><source src='https://va.media.tumblr.com/two.mp4'></video>", &page).is_err());
        assert!(parse_tumblr_player(
            "<video><source src='https://media.tumblr.com.evil.example/wrong.mp4'></video>",
            &page
        )
        .is_err());
        assert!(parse_tumblr_player(
            "<video><source src='file:///tmp/private.mp4'></video>",
            &page
        )
        .is_err());
        assert!(parse_tumblr_player("<iframe src='https://www.youtube.com/embed/dQw4w9WgXcQ'></iframe><iframe src='https://vimeo.com/12345'></iframe>", &page).is_err());
        let (formats, embed, _) = parse_tumblr_player("<video src='https://va.media.tumblr.com/own.mp4'></video><iframe src='https://www.youtube.com/embed/dQw4w9WgXcQ'></iframe>", &page).unwrap();
        assert_eq!(formats.len(), 1);
        assert!(embed.is_none());
    }

    #[test]
    fn tumblr_legacy_iframe_must_be_the_single_exact_target_player() {
        let page = Url::parse("https://sample.tumblr.com/post/12345").unwrap();
        let html = "<iframe src='https://www.tumblr.com/video/sample/12345/500/'></iframe>";
        assert!(tumblr_native_iframe(html, "sample", "12345", &page)
            .unwrap()
            .is_some());
        assert!(tumblr_native_iframe(
            &html.replace("sample/12345", "other/12345"),
            "sample",
            "12345",
            &page
        )
        .unwrap()
        .is_none());
        assert!(tumblr_native_iframe(
            &html.replace("sample/12345", "sample/54321"),
            "sample",
            "12345",
            &page
        )
        .unwrap()
        .is_none());
        assert!(tumblr_native_iframe(&format!("{html}{html}"), "sample", "12345", &page).is_err());
        assert!(tumblr_native_iframe(
            &format!("{html}<video src='https://va.media.tumblr.com/wrong.mp4'></video>"),
            "sample",
            "12345",
            &page
        )
        .unwrap()
        .is_none());
        let mut data = tumblr_data();
        data["posts"][0]["video-player"] = json!(html);
        assert!(tumblr_target(&data, "sample", "12345").is_ok());
        data["posts"][0]["requires_login"] = json!(true);
        assert!(tumblr_target(&data, "sample", "12345").is_err());
    }

    #[test]
    fn tumblr_npf_ignores_trail_and_drm_and_requires_one_media_block() {
        let page = Url::parse("https://sample.tumblr.com/post/12345").unwrap();
        let content = vec![
            json!({"type":"text","text":"post"}),
            json!({"type":"video","provider":"tumblr","duration":12500,"media":{"url":"https://va.media.tumblr.com/own.mp4","type":"video/mp4","height":1080}}),
        ];
        let (formats, _, duration) = parse_tumblr_npf(&content, &page).unwrap();
        assert_eq!(formats[0].height, 1080);
        assert_eq!(duration, Some(12));
        let mut protected = content.clone();
        protected[1]["drm"] = json!({"protected":true});
        assert!(parse_tumblr_npf(&protected, &page).is_err());
        let mut multi = content.clone();
        multi.push(content[1].clone());
        assert!(parse_tumblr_npf(&multi, &page).is_err());
        let external =
            vec![json!({"type":"video","provider":"youtube","url":"https://youtu.be/dQw4w9WgXcQ"})];
        assert!(parse_tumblr_npf(&external, &page).unwrap().1.is_some());
    }

    #[test]
    fn tumblr_page_requires_canonical_identity_and_reads_only_post_metadata() {
        let page = Url::parse("https://sample.tumblr.com/post/12345").unwrap();
        let html = "<meta property='og:url' content='https://sample.tumblr.com/post/12345'><meta property='og:title' content='Own post'><meta property='og:video' content='https://va.media.tumblr.com/own_720.mp4'><video src='https://va.media.tumblr.com/recommended.mp4'></video>";
        let (resource, _) = parse_tumblr_page(html, "sample", "12345", &page).unwrap();
        assert_eq!(resource.candidates.len(), 1);
        assert!(resource.candidates[0].url.contains("own_720"));
        assert!(parse_tumblr_page(
            &html.replace("post/12345", "post/99999"),
            "sample",
            "12345",
            &page
        )
        .is_err());
        assert!(parse_tumblr_page(
            "<meta property='og:video' content='https://va.media.tumblr.com/wrong.mp4'>",
            "sample",
            "12345",
            &page
        )
        .is_err());
    }

    #[test]
    fn url_shapes_and_source_ids_do_not_confuse_sites_authors_or_cdn_files() {
        for url in [
            "https://sample.tumblr.com/post/12345/title",
            "https://www.tumblr.com/sample/12345/title",
            "https://www.tumblr.com/blog/view/sample/12345",
        ] {
            assert_eq!(
                tumblr_post(&Url::parse(url).unwrap()),
                Some(("sample".to_owned(), "12345".to_owned()))
            );
        }
        for url in [
            "https://va.media.tumblr.com/tumblr_video_720.mp4",
            "https://sample.tumblr.com/post/abc",
            "https://www.tumblr.com/login",
            "https://sample.tumblr.com.evil.example/post/12345",
        ] {
            assert!(tumblr_post(&Url::parse(url).unwrap()).is_none());
        }
        assert_eq!(
            mastodon_post(&Url::parse("https://social.example/@user@example.net/12345").unwrap()),
            Some(("user@example.net".to_owned(), "12345".to_owned()))
        );
        assert_eq!(
            mastodon_post(
                &Url::parse("https://social.example/%40user%40example.net/12345").unwrap()
            ),
            Some(("user@example.net".to_owned(), "12345".to_owned()))
        );
        assert!(mastodon_post(&Url::parse("https://social.example/@user/abc").unwrap()).is_none());
        let a = parse_mastodon(
            &mastodon_data(),
            "example",
            "12345",
            &Url::parse("https://one.example/@example/12345").unwrap(),
        )
        .unwrap();
        let b = parse_mastodon(
            &mastodon_data(),
            "example",
            "12345",
            &Url::parse("https://two.example/@example/12345").unwrap(),
        )
        .unwrap();
        assert_ne!(a.id, b.id);
    }

    #[tokio::test]
    #[ignore = "requests public Tumblr and Mastodon APIs; run explicitly for live metadata validation"]
    async fn public_social_metadata() -> Result<()> {
        let resolver = SocialBatch3Resolver::new()?;
        for (platform, url) in [
            (
                "Tumblr",
                "https://maskofthedragon.tumblr.com/post/626907179849564160",
            ),
            (
                "Mastodon",
                "https://mastodon.social/@Mastodon/116296397062584045",
            ),
        ] {
            let resource = resolver.resolve(&Url::parse(url)?, platform).await?;
            ensure!(!resource.candidates.is_empty(), "{platform} 未返回原始媒体");
            println!(
                "{platform}: {} public format(s), duration {:?}",
                resource.candidates.len(),
                resource.duration_seconds
            );
        }
        Ok(())
    }
}
