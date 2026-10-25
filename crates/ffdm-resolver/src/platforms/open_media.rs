//! Native public-media adapters. The metadata layouts are documented by
//! Archive.org and MediaWiki and also used by yt-dlp's Archive.org/TED/Wikimedia
//! extractors. No page JavaScript or external extractor is executed.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{bail, ensure, Context, Result};
use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    time::{Duration, SystemTime},
};

const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;
const MAX_CANDIDATES: usize = 32;
const WIKI_USER_AGENT: &str =
    "FFDownload/0.3.7 (https://github.com/gnipbao/FFDownloadManager; public media resolver)";

pub(crate) struct OpenMediaResolver {
    client: Client,
    wiki_next_request: tokio::sync::Mutex<Option<tokio::time::Instant>>,
}

impl OpenMediaResolver {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            wiki_next_request: tokio::sync::Mutex::new(None),
            client: Client::builder()
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .user_agent(crate::resource::USER_AGENT)
                .redirect(Policy::custom(|attempt| {
                    if attempt.previous().len() >= 5 || !official_metadata_url(attempt.url()) {
                        attempt.error("public metadata redirected outside official domains")
                    } else {
                        attempt.follow()
                    }
                }))
                .build()?,
        })
    }

    pub(crate) async fn resolve(&self, input: &Url, platform: &str) -> Result<GenericResource> {
        match platform {
            "Archive.org" => self.archive(input).await,
            "TED" => self.ted(input).await,
            "WikimediaCommons" => self.wikimedia(input).await,
            _ => bail!("未接入这个开放媒体平台"),
        }
    }

    async fn fetch(&self, url: Url, platform: &str, wiki: bool) -> Result<String> {
        ensure!(official_metadata_url(&url), "公开媒体信息不在官方域名内");
        // The public Action API permits one unauthenticated request at a time.
        // Keep this guard through body consumption; a slow response delays the
        // next call, and HTTP 429 honors the server's Retry-After.
        let mut wiki_guard = if wiki {
            let guard = self.wiki_next_request.lock().await;
            if let Some(next) = *guard {
                tokio::time::sleep_until(next).await;
            }
            Some(guard)
        } else {
            None
        };
        let started = tokio::time::Instant::now();
        if let Some(guard) = &mut wiki_guard {
            **guard = Some(started + Duration::from_secs(5));
        }
        let mut request = self
            .client
            .get(url)
            .header(header::ACCEPT_ENCODING, "identity");
        if wiki {
            // Wikimedia's robot policy requires an identifying user-agent;
            // pretending to be Chrome can cause otherwise-public files to 403.
            request = request.header(header::USER_AGENT, WIKI_USER_AGENT);
        }
        let mut response = request
            .send()
            .await
            .with_context(|| format!("{platform} 公开信息请求失败"))?;
        if let Some(guard) = &mut wiki_guard {
            let delay = if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| {
                        value
                            .trim()
                            .parse::<u64>()
                            .ok()
                            .map(Duration::from_secs)
                            .or_else(|| {
                                httpdate::parse_http_date(value).ok().map(|date| {
                                    date.duration_since(SystemTime::now()).unwrap_or_default()
                                })
                            })
                    })
                    .unwrap_or(Duration::from_secs(60))
            } else if started.elapsed() > Duration::from_secs(1) {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(250)
            };
            **guard = Some(
                tokio::time::Instant::now()
                    .checked_add(delay)
                    .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(60)),
            );
        }
        match response.status().as_u16() {
            401 | 403 => bail!("{platform} 要求登录、访问验证或受地区限制"),
            404 | 410 => bail!("{platform} 作品不存在或已经删除"),
            429 => bail!("{platform} 请求过于频繁，请稍后重试"),
            _ => ensure!(
                response.status().is_success(),
                "{platform} 返回 HTTP {}",
                response.status()
            ),
        }
        ensure!(
            response.content_length().unwrap_or(0) <= MAX_METADATA_BYTES as u64,
            "{platform} 公开信息超过 4 MiB 解析上限"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_METADATA_BYTES,
                "{platform} 公开信息超过 4 MiB 解析上限"
            );
            bytes.extend_from_slice(&chunk);
        }
        if let Some(guard) = &mut wiki_guard {
            let delay = if started.elapsed() > Duration::from_secs(1) {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(250)
            };
            **guard = Some(tokio::time::Instant::now() + delay);
        }
        String::from_utf8(bytes).context("公开媒体信息不是有效的 UTF-8")
    }

    async fn archive(&self, input: &Url) -> Result<GenericResource> {
        let (identifier, filename) = archive_identity(input)?;
        let mut api = Url::parse("https://archive.org/metadata/")?;
        api.path_segments_mut()
            .unwrap()
            .pop_if_empty()
            .push(&identifier);
        let text = self.fetch(api, "Archive.org", false).await?;
        let data: Value = serde_json::from_str(&text).context("Archive.org 未返回有效媒体信息")?;
        parse_archive(&data, &identifier, filename.as_deref())
    }

    async fn ted(&self, input: &Url) -> Result<GenericResource> {
        let slug = ted_slug(input)?;
        let canonical = Url::parse(&format!("https://www.ted.com/talks/{slug}"))?;
        let html = self.fetch(canonical, "TED", false).await?;
        let re = Regex::new(
            r#"(?is)<script\b[^>]*\bid\s*=\s*(?:"__NEXT_DATA__"|'__NEXT_DATA__')[^>]*>(.*?)</script\s*>"#,
        )?;
        let json = re
            .captures(&html)
            .and_then(|capture| capture.get(1))
            .context("TED 页面没有提供公开播放器信息")?;
        let data: Value =
            serde_json::from_str(json.as_str()).context("TED 播放器信息格式已变化")?;
        parse_ted(&data, &slug)
    }

    async fn wikimedia(&self, input: &Url) -> Result<GenericResource> {
        let title = wikimedia_title(input)?;
        let mut api = Url::parse("https://commons.wikimedia.org/w/api.php")?;
        api.query_pairs_mut()
            .append_pair("action", "query")
            .append_pair("format", "json")
            .append_pair("titles", &title)
            .append_pair("prop", "videoinfo")
            .append_pair("viprop", "url|size|mime|mediatype|derivatives|extmetadata");
        let text = self.fetch(api, "Wikimedia Commons", true).await?;
        let data: Value = serde_json::from_str(&text).context("Wikimedia 未返回有效媒体信息")?;
        parse_wikimedia(&data, &title)
    }
}

fn official_metadata_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(
            url.host_str(),
            Some(
                "archive.org"
                    | "www.archive.org"
                    | "www.ted.com"
                    | "ted.com"
                    | "commons.wikimedia.org"
            )
        )
}

fn archive_identity(input: &Url) -> Result<(String, Option<String>)> {
    ensure!(
        matches!(input.host_str(), Some("archive.org" | "www.archive.org")),
        "请输入 Archive.org 作品链接"
    );
    let path = percent_decode_str(input.path()).decode_utf8()?;
    let mut parts = path.trim_matches('/').splitn(3, '/');
    ensure!(
        matches!(parts.next(), Some("details" | "embed")),
        "请输入 Archive.org details 或 embed 作品链接"
    );
    let identifier = parts.next().unwrap_or("");
    ensure!(
        !identifier.is_empty()
            && identifier.len() <= 200
            && identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_- .".contains(&byte))
            && !matches!(identifier, "." | ".."),
        "Archive.org 作品标识不合法"
    );
    let filename = parts.next().filter(|name| !name.is_empty());
    if let Some(name) = filename {
        ensure!(valid_archive_filename(name), "Archive.org 文件名称不合法");
    }
    Ok((identifier.to_owned(), filename.map(str::to_owned)))
}

fn valid_archive_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 1024
        && !name.chars().any(char::is_control)
        && !name.contains('\\')
        && name
            .split('/')
            .all(|part| !part.is_empty() && !matches!(part, "." | ".."))
}

fn ted_slug(input: &Url) -> Result<String> {
    ensure!(
        matches!(
            input.host_str(),
            Some("ted.com" | "www.ted.com" | "embed.ted.com" | "embed-ssl.ted.com")
        ),
        "请输入 TED 单个演讲链接"
    );
    let parts: Vec<_> = input.path().trim_matches('/').split('/').collect();
    let slug = match parts.as_slice() {
        ["talks", slug] | ["talks", "lang", _, slug] => *slug,
        _ => bail!("请输入 TED 单个演讲链接；合集暂不支持"),
    };
    ensure!(
        !slug.is_empty()
            && slug.len() <= 300
            && slug
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
        "TED 演讲标识不合法"
    );
    Ok(slug.to_owned())
}

fn wikimedia_title(input: &Url) -> Result<String> {
    ensure!(
        input.host_str() == Some("commons.wikimedia.org"),
        "请输入 Wikimedia Commons 文件页面"
    );
    let title = if let Some(title) = input.path().strip_prefix("/wiki/") {
        percent_decode_str(title).decode_utf8()?.into_owned()
    } else if input.path() == "/w/index.php" {
        input
            .query_pairs()
            .find(|(key, _)| key == "title")
            .map(|(_, value)| value.into_owned())
            .context("Wikimedia 文件页面缺少 title")?
    } else {
        bail!("请输入 Wikimedia Commons 的 File 文件页面")
    };
    let (namespace, name) = title
        .split_once(':')
        .context("请输入 Wikimedia Commons 的 File 文件页面")?;
    ensure!(
        namespace.eq_ignore_ascii_case("File")
            && !name.is_empty()
            && title.len() <= 1024
            && !title.chars().any(char::is_control),
        "请输入 Wikimedia Commons 的 File 文件页面"
    );
    Ok(format!("File:{name}"))
}

fn parse_archive(
    data: &Value,
    identifier: &str,
    explicit: Option<&str>,
) -> Result<GenericResource> {
    ensure!(
        text(&data["metadata"]["identifier"]) == Some(identifier),
        "Archive.org 没有返回请求的作品信息"
    );
    let files = data["files"]
        .as_array()
        .context("Archive.org 作品没有文件信息")?;
    // An item can be an album or a playlist. Derivatives are alternatives only
    // when they trace to the same original file, never merely by their title.
    let origin = |file: &Value| -> Option<String> {
        let name = text(&file["name"])?;
        let mut root = text(&file["original"]).unwrap_or(name).to_owned();
        let mut seen = HashSet::new();
        for _ in 0..8 {
            if !seen.insert(root.clone()) {
                return None;
            }
            let parent = files
                .iter()
                .find(|entry| text(&entry["name"]) == Some(root.as_str()))
                .and_then(|entry| text(&entry["original"]));
            let Some(parent) = parent else {
                return Some(root);
            };
            root = parent.to_owned();
        }
        None
    };
    let supported: Vec<_> = files
        .iter()
        .filter(|file| {
            text(&file["name"]).is_some_and(|name| {
                archive_container(name).is_some() && valid_archive_filename(name)
            }) && !private_file(file)
        })
        .collect();
    let selected_origin = if let Some(filename) = explicit {
        let selected = files
            .iter()
            .find(|file| text(&file["name"]) == Some(filename))
            .context("Archive.org 作品中没有指定的文件")?;
        ensure!(
            archive_container(filename).is_some()
                || (text(&selected["source"]) == Some("original")
                    && supported
                        .iter()
                        .any(|file| { text(&file["original"]) == Some(filename) })),
            "Archive.org 指定的文件不是受支持的音视频作品"
        );
        origin(selected).context("Archive.org 文件关联信息存在循环")?
    } else {
        let origins: HashSet<_> = supported.iter().filter_map(|file| origin(file)).collect();
        ensure!(
            !origins.is_empty(),
            "Archive.org 没有可公开下载的受支持音视频文件"
        );
        ensure!(
            origins.len() == 1,
            "Archive.org 作品含多个独立音视频，请提供 /details/作品标识/文件名 链接"
        );
        origins.into_iter().next().unwrap()
    };
    let selected_original = files
        .iter()
        .find(|file| text(&file["name"]) == Some(selected_origin.as_str()));
    let page = format!("https://archive.org/details/{identifier}");
    let mut candidates = Vec::new();
    for file in supported {
        if origin(file).as_deref() != Some(selected_origin.as_str()) {
            continue;
        }
        let name = text(&file["name"]).unwrap();
        let (extension, mut audio_only) = archive_container(name).unwrap();
        if extension == "ogg"
            && text(&file["format"]).is_some_and(|kind| kind.to_ascii_lowercase().contains("video"))
        {
            audio_only = false;
        }
        let mut download = Url::parse("https://archive.org/download/")?;
        {
            let mut path = download.path_segments_mut().unwrap();
            path.pop_if_empty().push(identifier);
            for component in name.split('/') {
                path.push(component);
            }
        }
        let height = integer(&file["height"]).unwrap_or(0) as u32;
        let kind = text(&file["format"]).unwrap_or(extension);
        candidates.push(GenericCandidate {
            transform: None,
            id: stable_id("archive", name),
            label: if height > 0 {
                format!("{height}p · {kind}")
            } else {
                kind.to_owned()
            },
            height,
            url: download.to_string(),
            extension: extension.to_owned(),
            audio_only,
            headers: headers(&page, false),
            size: integer(&file["size"]),
        });
    }
    ensure!(
        !candidates.is_empty(),
        "Archive.org 指定文件没有可公开下载的受支持音视频格式"
    );
    ensure!(
        candidates.len() <= MAX_CANDIDATES,
        "Archive.org 单个文件格式超过 32 个上限"
    );
    let title = selected_original
        .and_then(|file| text(&file["title"]))
        .or_else(|| text(&data["metadata"]["title"]))
        .unwrap_or(&selected_origin)
        .to_owned();
    let duration_seconds = selected_original.and_then(|file| duration(&file["length"]));
    Ok(GenericResource {
        id: stable_id("archive", &format!("{identifier}/{selected_origin}")),
        title,
        duration_seconds,
        candidates,
        embed: None,
    })
}

fn private_file(file: &Value) -> bool {
    enabled_marker(&file["private"])
}

fn archive_container(name: &str) -> Option<(&'static str, bool)> {
    match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
        "mp4" => Some(("mp4", false)),
        "webm" => Some(("webm", false)),
        "ogv" => Some(("ogv", false)),
        "mkv" => Some(("mkv", false)),
        "mov" => Some(("mov", false)),
        "flv" => Some(("flv", false)),
        "m4a" => Some(("m4a", true)),
        "mp3" => Some(("mp3", true)),
        "ogg" => Some(("ogg", true)),
        "oga" => Some(("oga", true)),
        "flac" => Some(("flac", true)),
        "wav" => Some(("wav", true)),
        "aac" => Some(("aac", true)),
        "opus" => Some(("opus", true)),
        "weba" => Some(("weba", true)),
        "avi" => Some(("avi", false)),
        "mpg" => Some(("mpg", false)),
        "mpeg" => Some(("mpeg", false)),
        "m4v" => Some(("m4v", false)),
        "3gp" => Some(("3gp", false)),
        "ts" => Some(("ts", false)),
        "m2ts" => Some(("m2ts", false)),
        _ => None,
    }
}

fn parse_ted(data: &Value, slug: &str) -> Result<GenericResource> {
    let talk = &data["props"]["pageProps"]["videoData"];
    ensure!(talk.is_object(), "TED 页面没有单个演讲信息");
    if let Some(returned_slug) = text(&talk["slug"]) {
        ensure!(returned_slug == slug, "TED 返回了另一个演讲的信息");
    }
    let player = &talk["videoPlayerData"];
    if let Some(returned_slug) = text(&player["slug"]) {
        ensure!(returned_slug == slug, "TED 播放器与请求的演讲不一致");
    }
    ensure!(
        !enabled_marker(&talk["isPremium"])
            && !enabled_marker(&player["hasDRM"])
            && !enabled_marker(&player["drm"]),
        "TED 该演讲需要授权或使用加密媒体"
    );
    let page = format!("https://www.ted.com/talks/{slug}");
    let mut candidates = Vec::new();
    if let Some(resources) = player["resources"]["h264"].as_array() {
        for (index, resource) in resources.iter().enumerate().take(MAX_CANDIDATES) {
            let Some(raw) = text(&resource["file"]) else {
                continue;
            };
            let Some(url) = ted_media_url(raw) else {
                continue;
            };
            let extension = media_extension(&url, None);
            if extension != Some("mp4") {
                continue;
            }
            let bitrate = integer(&resource["bitrate"]);
            let height = integer(&resource["height"]).unwrap_or(0) as u32;
            candidates.push(GenericCandidate {
                transform: None,
                id: format!("ted-h264-{index}"),
                label: bitrate
                    .map_or_else(|| "MP4".to_owned(), |rate| format!("MP4 · {rate} kbps")),
                height,
                url: url.to_string(),
                extension: "mp4".to_owned(),
                audio_only: false,
                headers: headers(&page, false),
                size: integer(&resource["size"]),
            });
        }
    }
    if let Some(url) = text(&player["resources"]["hls"]["stream"]).and_then(ted_media_url) {
        candidates.push(GenericCandidate {
            transform: None,
            id: "ted-hls".to_owned(),
            label: "HLS".to_owned(),
            height: 0,
            url: url.to_string(),
            extension: "m3u8".to_owned(),
            audio_only: false,
            headers: headers(&page, false),
            size: None,
        });
    }
    if let Some(url) = text(&talk["audioDownload"]).and_then(ted_media_url) {
        if let Some(extension @ ("mp3" | "m4a" | "aac" | "ogg" | "opus")) =
            media_extension(&url, None)
        {
            candidates.push(GenericCandidate {
                transform: None,
                id: "ted-audio".to_owned(),
                label: format!("音频 · {}", extension.to_ascii_uppercase()),
                height: 0,
                url: url.to_string(),
                extension: extension.to_owned(),
                audio_only: true,
                headers: headers(&page, false),
                size: None,
            });
        }
    }
    let embed = if candidates.is_empty()
        && text(&player["external"]["service"])
            .is_some_and(|service| service.eq_ignore_ascii_case("youtube"))
    {
        let code = text(&player["external"]["code"]).context("TED 外部播放器缺少作品标识")?;
        ensure!(
            code.len() == 11
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
            "TED 外部播放器标识不合法"
        );
        Some(Url::parse(&format!(
            "https://www.youtube.com/watch?v={code}"
        ))?)
    } else {
        None
    };
    ensure!(
        !candidates.is_empty() || embed.is_some(),
        "TED 演讲没有可公开读取的受支持播放格式"
    );
    candidates.truncate(MAX_CANDIDATES);
    Ok(GenericResource {
        id: talk["id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| integer(&talk["id"]).map(|number| number.to_string()))
            .unwrap_or_else(|| slug.to_owned()),
        title: text(&talk["title"]).unwrap_or(slug).to_owned(),
        duration_seconds: duration(&talk["duration"]).or_else(|| duration(&player["duration"])),
        candidates,
        embed,
    })
}

fn ted_media_url(raw: &str) -> Option<Url> {
    let url = crate::validation::parse_url(raw).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && (host == "tedcdn.com"
            || host.ends_with(".tedcdn.com")
            || host == "ted.com"
            || host.ends_with(".ted.com")))
    .then_some(url)
}

fn parse_wikimedia(data: &Value, requested_title: &str) -> Result<GenericResource> {
    ensure!(data.get("error").is_none(), "Wikimedia 文件接口返回错误");
    let pages = data["query"]["pages"]
        .as_object()
        .context("Wikimedia 文件不存在")?;
    ensure!(pages.len() == 1, "Wikimedia 返回了多个不同文件");
    let page = pages.values().next().unwrap();
    ensure!(
        integer(&page["ns"]) == Some(6) && page.get("missing").is_none(),
        "Wikimedia 文件不存在"
    );
    let returned_title = text(&page["title"]).context("Wikimedia 文件缺少标题")?;
    let normalization_matches = data["query"]["normalized"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|entry| {
            text(&entry["from"])
                .is_some_and(|from| from.replace('_', " ") == requested_title.replace('_', " "))
                && text(&entry["to"]) == Some(returned_title)
        });
    ensure!(
        returned_title.replace('_', " ") == requested_title.replace('_', " ")
            || normalization_matches,
        "Wikimedia 返回了另一个文件的信息"
    );
    let info = page["videoinfo"]
        .as_array()
        .and_then(|items| items.first())
        .context("Wikimedia 文件没有公开音视频信息")?;
    let source_url = text(&info["url"])
        .and_then(wiki_media_url)
        .context("Wikimedia 没有公开文件地址")?;
    let source_extension = media_extension(&source_url, text(&info["mime"]))
        .context("该 Wikimedia 文件不是受支持的音视频")?;
    let source_audio = text(&info["mime"]).is_some_and(|mime| mime.starts_with("audio/"))
        || text(&info["mediatype"]) == Some("AUDIO")
        || matches!(
            source_extension,
            "mp3" | "m4a" | "flac" | "wav" | "aac" | "opus" | "oga" | "weba"
        )
        || (source_extension == "ogg" && integer(&info["height"]).unwrap_or(0) == 0);
    let referer = "https://commons.wikimedia.org/";
    let mut candidates = vec![GenericCandidate {
        transform: None,
        id: "wiki-source".to_owned(),
        label: format!("原始文件 · {}", source_extension.to_ascii_uppercase()),
        height: integer(&info["height"]).unwrap_or(0) as u32,
        url: source_url.to_string(),
        extension: source_extension.to_owned(),
        audio_only: source_audio,
        headers: headers(referer, true),
        size: integer(&info["size"]),
    }];
    let mut seen = HashSet::from([wiki_media_identity(&source_url)]);
    for (index, derivative) in info["derivatives"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
        let Some(url) = text(&derivative["src"]).and_then(wiki_media_url) else {
            continue;
        };
        let mime = text(&derivative["type"]);
        let Some(extension) = media_extension(&url, mime) else {
            continue;
        };
        if !seen.insert(wiki_media_identity(&url)) {
            continue;
        }
        let height = integer(&derivative["height"]).unwrap_or(0) as u32;
        let audio_only = mime.is_some_and(|mime| mime.starts_with("audio/"));
        candidates.push(GenericCandidate {
            transform: None,
            id: format!("wiki-derivative-{index}"),
            label: if audio_only {
                format!("音频 · {}", extension.to_ascii_uppercase())
            } else if height > 0 {
                format!("{height}p · {}", extension.to_ascii_uppercase())
            } else {
                extension.to_ascii_uppercase()
            },
            height,
            url: url.to_string(),
            extension: extension.to_owned(),
            audio_only,
            headers: headers(referer, true),
            size: integer(&derivative["size"]),
        });
    }
    let filename_title = returned_title
        .strip_prefix("File:")
        .unwrap_or(returned_title);
    let fallback_title = filename_title
        .rsplit_once('.')
        .filter(|(_, suffix)| suffix.eq_ignore_ascii_case(source_extension))
        .map_or(filename_title, |(stem, _)| stem);
    Ok(GenericResource {
        id: integer(&page["pageid"])
            .map(|id| id.to_string())
            .unwrap_or_else(|| stable_id("wiki", returned_title)),
        title: text(&info["extmetadata"]["ObjectName"]["value"])
            .unwrap_or(fallback_title)
            .to_owned(),
        duration_seconds: duration(&info["duration"]),
        candidates,
        embed: None,
    })
}

fn wiki_media_url(raw: &str) -> Option<Url> {
    let url = crate::validation::parse_url(raw).ok()?;
    (url.scheme() == "https" && url.host_str() == Some("upload.wikimedia.org")).then_some(url)
}

fn wiki_media_identity(url: &Url) -> String {
    // MediaWiki advertises its original once as a source and again as a
    // derivative, with different analytics tags. Deduplicate that alternative
    // without touching the actual download URL or any non-analytics query.
    let mut identity = url.clone();
    let pairs: Vec<_> = identity
        .query_pairs()
        .filter(|(key, _)| !key.starts_with("utm_"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    identity.set_query(None);
    if !pairs.is_empty() {
        identity.query_pairs_mut().extend_pairs(pairs);
    }
    identity.set_fragment(None);
    identity.to_string()
}

fn media_extension(url: &Url, mime: Option<&str>) -> Option<&'static str> {
    let ext = url.path().rsplit('.').next()?.to_ascii_lowercase();
    if ext == "m3u8" {
        return Some("m3u8");
    }
    if let Some((extension, _)) = archive_container(&format!("media.{ext}")) {
        return Some(extension);
    }
    let mime = mime.unwrap_or("").split(';').next().unwrap_or("").trim();
    match mime {
        "video/mp4" => Some("mp4"),
        "video/webm" | "audio/webm" => Some("webm"),
        "video/ogg" => Some("ogv"),
        "audio/ogg" => Some("ogg"),
        "video/quicktime" => Some("mov"),
        "audio/mpeg" => Some("mp3"),
        "audio/mp4" => Some("m4a"),
        "audio/opus" => Some("opus"),
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" => Some("m3u8"),
        _ => None,
    }
}

fn headers(referer: &str, wiki: bool) -> Vec<(String, String)> {
    vec![
        (
            "User-Agent".to_owned(),
            if wiki {
                WIKI_USER_AGENT
            } else {
                crate::resource::USER_AGENT
            }
            .to_owned(),
        ),
        ("Referer".to_owned(), referer.to_owned()),
    ]
}

fn text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn enabled_marker(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_u64().is_none_or(|number| number != 0),
        Value::String(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false"
        ),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn integer(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn duration(value: &Value) -> Option<u64> {
    let number = value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())?;
    (number.is_finite() && (0.0..=604800.0).contains(&number)).then_some(number.ceil() as u64)
}

fn stable_id(prefix: &str, value: &str) -> String {
    let hash = Sha256::digest(value.as_bytes());
    format!("{prefix}-{:x}", hash)[..prefix.len() + 17].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn archive_derivatives_are_grouped_by_original_and_private_files_are_omitted() {
        let data = json!({"metadata":{"identifier":"demo","title":"A film"},"files":[
            {"name":"film.avi","source":"original","length":"3.2"},
            {"name":"film_240p.mp4","original":"film.avi","height":"240","size":"123"},
            {"name":"film.webm","original":"film.avi","height":720},
            {"name":"film-private.mp4","original":"film.avi","private":"true"},
            {"name":"film.jpg","original":"film.avi"}
        ]});
        let resource = parse_archive(&data, "demo", None).unwrap();
        assert_eq!(resource.candidates.len(), 3);
        assert_eq!(resource.duration_seconds, Some(4));
        assert_eq!(resource.candidates[0].extension, "avi");
        assert_eq!(resource.candidates[1].extension, "mp4");
        assert_eq!(resource.candidates[2].extension, "webm");
        assert_eq!(resource.candidates[1].size, Some(123));
        assert!(parse_archive(&data, "another", None).is_err());
        assert!(parse_archive(&data, "demo", Some("film.jpg")).is_err());
    }

    #[test]
    fn archive_albums_need_a_specific_file_and_keep_nested_unicode_names() {
        let data = json!({"metadata":{"identifier":"album","title":"Album"},"files":[
            {"name":"disc1/一首 歌.flac","title":"Song","length":2},
            {"name":"disc1/一首 歌.mp3","original":"disc1/一首 歌.flac"},
            {"name":"second.mp3"}
        ]});
        assert!(parse_archive(&data, "album", None).is_err());
        let resource = parse_archive(&data, "album", Some("disc1/一首 歌.flac")).unwrap();
        assert_eq!(resource.title, "Song");
        assert_eq!(resource.candidates.len(), 2);
        assert!(resource
            .candidates
            .iter()
            .all(|candidate| candidate.audio_only));
        assert_eq!(resource.candidates[0].extension, "flac");
        assert!(resource.candidates[0]
            .url
            .contains("%E4%B8%80%E9%A6%96%20%E6%AD%8C.flac"));
        assert!(!valid_archive_filename("disc1/../private.mp3"));
        assert!(!valid_archive_filename("/absolute.mp3"));
        assert!(parse_archive(&data, "album", Some("missing.mp3")).is_err());
    }

    #[test]
    fn ted_uses_exact_talk_player_and_official_streams_with_external_fallback() {
        let data = json!({"props":{"pageProps":{"videoData":{"id":"123","slug":"speaker_talk","title":"A talk","duration":3.5,
            "audioDownload":"https://download.ted.com/audio.mp3",
            "videoPlayerData":{"slug":"speaker_talk","resources":{"h264":[
                {"file":"https://py.tedcdn.com/talk-1200k.mp4?token=a%2Bb","bitrate":1200},
                {"file":"https://evil.test/recommendation.mp4"}
            ],"hls":{"stream":"https://hls.ted.com/manifest.m3u8?preview"}}}
        }}}});
        let resource = parse_ted(&data, "speaker_talk").unwrap();
        assert_eq!(resource.candidates.len(), 3);
        assert_eq!(resource.duration_seconds, Some(4));
        assert_eq!(resource.candidates[0].extension, "mp4");
        assert!(resource.candidates[0].url.ends_with("?token=a%2Bb"));
        assert_eq!(resource.candidates[1].extension, "m3u8");
        assert!(resource.candidates[2].audio_only);
        assert!(parse_ted(&data, "another_talk").is_err());
        let mut drm = data.clone();
        drm["props"]["pageProps"]["videoData"]["videoPlayerData"]["drm"] =
            json!({"widevine":"licensed"});
        assert!(parse_ted(&drm, "speaker_talk").is_err());
        drm["props"]["pageProps"]["videoData"]["videoPlayerData"]["drm"] = json!({});
        assert!(parse_ted(&drm, "speaker_talk").is_err());
        drm["props"]["pageProps"]["videoData"]["videoPlayerData"]["drm"] = json!("true");
        assert!(parse_ted(&drm, "speaker_talk").is_err());
        drm["props"]["pageProps"]["videoData"]["videoPlayerData"]["drm"] = json!("false");
        assert!(parse_ted(&drm, "speaker_talk").is_ok());
        let external = json!({"props":{"pageProps":{"videoData":{"slug":"speaker_talk","videoPlayerData":{"external":{"service":"YouTube","code":"YE7VzlLtp-4"}}}}}});
        assert_eq!(
            parse_ted(&external, "speaker_talk")
                .unwrap()
                .embed
                .unwrap()
                .query(),
            Some("v=YE7VzlLtp-4")
        );
    }

    #[test]
    fn wikimedia_preserves_webm_mov_ogg_containers_and_identifying_user_agent() {
        let data = json!({"query":{"pages":{"123":{"pageid":123,"ns":6,"title":"File:Video.webm","videoinfo":[{
            "url":"https://upload.wikimedia.org/wikipedia/commons/a/ab/Video.webm?utm_content=original",
            "size":321,"height":1080,"duration":2.3,
            "extmetadata":{"ObjectName":{"value":"Public video"}},
            "derivatives":[
                {"src":"https://upload.wikimedia.org/wikipedia/commons/a/ab/Video.webm?utm_content=original&utm_campaign=api","height":1080,"type":"video/webm"},
                {"src":"https://upload.wikimedia.org/transcoded/Video.webm.240p.webm","height":240,"type":"video/webm; codecs=\"vp9, opus\""},
                {"src":"https://upload.wikimedia.org/transcoded/Video.webm.360p.mov","height":360,"type":"video/quicktime"},
                {"src":"https://upload.wikimedia.org/transcoded/Video.webm.ogg","type":"audio/ogg"},
                {"src":"https://evil.test/a.mp4","type":"video/mp4"}
            ]
        }]}}}});
        let resource = parse_wikimedia(&data, "File:Video.webm").unwrap();
        assert_eq!(resource.id, "123");
        assert_eq!(resource.candidates.len(), 4);
        assert_eq!(resource.candidates[0].extension, "webm");
        assert_eq!(resource.candidates[2].extension, "mov");
        assert_eq!(resource.candidates[3].extension, "ogg");
        assert!(resource.candidates[3].audio_only);
        assert!(resource.candidates[0].headers[0]
            .1
            .starts_with("FFDownload/"));
        assert!(parse_wikimedia(&data, "File:Another.webm").is_err());
        let mut fallback = data.clone();
        fallback["query"]["pages"]["123"]["videoinfo"][0]["extmetadata"] = Value::Null;
        assert_eq!(
            parse_wikimedia(&fallback, "File:Video.webm").unwrap().title,
            "Video"
        );
        let mut normalized = data.clone();
        normalized["query"]["normalized"] =
            json!([{"from":"File:video.webm","to":"File:Video.webm"}]);
        assert!(parse_wikimedia(&normalized, "File:video.webm").is_ok());
        assert!(parse_wikimedia(&normalized, "File:Another.webm").is_err());
    }

    #[test]
    fn url_patterns_do_not_accept_lookalike_hosts_or_playlists() {
        assert_eq!(
            archive_identity(
                &Url::parse("https://archive.org/details/demo/disc1/a%20b.flac").unwrap()
            )
            .unwrap(),
            ("demo".to_owned(), Some("disc1/a b.flac".to_owned()))
        );
        assert!(archive_identity(
            &Url::parse("https://archive.org.evil.test/details/demo").unwrap()
        )
        .is_err());
        assert_eq!(
            ted_slug(&Url::parse("https://embed.ted.com/talks/lang/zh/speaker_talk").unwrap())
                .unwrap(),
            "speaker_talk"
        );
        assert_eq!(
            ted_slug(&Url::parse("https://embed-ssl.ted.com/talks/speaker_talk").unwrap()).unwrap(),
            "speaker_talk"
        );
        assert!(ted_slug(&Url::parse("https://www.ted.com/playlists/1/list").unwrap()).is_err());
        assert_eq!(wikimedia_title(&Url::parse("https://commons.wikimedia.org/w/index.php?title=File%3A%E8%A7%86%E9%A2%91.webm").unwrap()).unwrap(), "File:视频.webm");
        assert_eq!(
            wikimedia_title(
                &Url::parse("https://commons.wikimedia.org/wiki/file:Sample_video.webm").unwrap()
            )
            .unwrap(),
            "File:Sample_video.webm"
        );
        assert_eq!(
            wikimedia_title(
                &Url::parse(
                    "https://commons.wikimedia.org/w/index.php?title=fIlE%3ASample_audio.ogg"
                )
                .unwrap()
            )
            .unwrap(),
            "File:Sample_audio.ogg"
        );
        assert!(wikimedia_title(
            &Url::parse("https://commons.wikimedia.org/wiki/Category:Videos").unwrap()
        )
        .is_err());
        assert!(!official_metadata_url(
            &Url::parse("https://user:pass@archive.org/metadata/demo").unwrap()
        ));
    }

    #[tokio::test]
    #[ignore = "reads live public Archive.org/TED/Wikimedia metadata"]
    async fn public_open_media_smoke() {
        let resolver = OpenMediaResolver::new().unwrap();
        for (platform, raw) in [
            ("Archive.org", "https://archive.org/details/BigBuckBunny_328"),
            ("TED", "https://www.ted.com/talks/candace_parker_how_to_break_down_barriers_and_not_accept_limits"),
            ("WikimediaCommons", "https://commons.wikimedia.org/wiki/File:Two-toed_sloth_rail_walking_%E4%BA%8C%E8%B6%BE%E6%A8%B9%E7%8D%BA%E7%88%AC%E8%A1%8C_(HD).webm"),
        ] {
            let resource = resolver.resolve(&Url::parse(raw).unwrap(), platform).await.unwrap();
            assert!(!resource.candidates.is_empty(), "{platform}");
            assert!(resource.candidates.iter().all(|candidate| !candidate.extension.is_empty()));
            println!("{platform}: {} public format(s)", resource.candidates.len());
        }
    }
}
