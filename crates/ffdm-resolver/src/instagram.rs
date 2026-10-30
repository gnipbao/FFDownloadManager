//! Read the requested Instagram post's public metadata. This fallback does not
//! manufacture GraphQL session values or import a browser/account session.
use crate::generic::{GenericCandidate, GenericResource};
use anyhow::{bail, ensure, Context, Result};
use regex::Regex;
use reqwest::{header, redirect::Policy, Client, Url};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, time::Duration};

pub(crate) const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_NODES: usize = 20_000;
const MAX_DEPTH: usize = 64;
const MAX_CANDIDATES: usize = 16;
const MAX_SCRIPTS: usize = 128;

fn official(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("instagram.com" | "www.instagram.com" | "m.instagram.com")
        )
}

pub(crate) fn shortcode(url: &Url) -> Result<String> {
    ensure!(official(url), "请输入 Instagram 的 HTTPS 单条作品链接");
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let code = match parts.as_slice() {
        ["p" | "reel" | "reels" | "tv", code] => *code,
        _ => bail!("Instagram 仅支持单条帖子或 Reel 链接"),
    };
    decode_shortcode(code)?;
    Ok(code.to_owned())
}

// Instagram shortcodes use this alphabet as an unsigned base-64 integer. Do
// not round the potentially large ID through a JSON floating-point value.
fn decode_shortcode(code: &str) -> Result<u128> {
    ensure!((5..=22).contains(&code.len()), "Instagram 作品编号无效");
    code.bytes().try_fold(0_u128, |number, byte| {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => bail!("Instagram 作品编号无效"),
        };
        number
            .checked_mul(64)
            .and_then(|number| number.checked_add(u128::from(digit)))
            .context("Instagram 作品编号超出范围")
    })
}

/// Fetch only the normal public post page, with a fresh proxy configuration.
/// A shell without media returns None so the caller can try an isolated page.
pub(crate) async fn resolve_page(input: &Url) -> Result<Option<GenericResource>> {
    let code = shortcode(input)?;
    let page = Url::parse(&format!("https://www.instagram.com/p/{code}/"))?;
    let work =
        async {
            let client = Client::builder()
                .tls_backend_rustls()
                .https_only(true)
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(8))
                .user_agent(USER_AGENT)
                .redirect(Policy::custom(|attempt| {
                    if attempt.previous().len() < 3 && official(attempt.url()) {
                        attempt.follow()
                    } else {
                        attempt.error("Instagram 分享页跳转到了不受支持的地址")
                    }
                }))
                .build()?;
            let mut response = client
                .get(page)
                .header(header::ACCEPT, "text/html,application/xhtml+xml")
                .header(header::ACCEPT_ENCODING, "identity")
                .send()
                .await
                .map_err(|error| {
                    anyhow::anyhow!("Instagram 分享页连接失败：{}", error.without_url())
                })?;
            check_status(response.status().as_u16())?;
            ensure!(official(response.url()), "Instagram 分享页来源发生变化");
            let path = response.url().path();
            ensure!(
                !path.starts_with("/accounts/")
                    && !path.starts_with("/challenge/")
                    && !path.starts_with("/checkpoint/"),
                "Instagram 当前要求登录或人工验证，请先在同一网络的浏览器确认作品可播放"
            );
            ensure!(
                shortcode(response.url()).ok().as_deref() == Some(code.as_str()),
                "Instagram 返回了其他页面，无法确认这条作品"
            );
            ensure!(
                response.content_length().unwrap_or(0) <= MAX_BYTES as u64,
                "Instagram 页面超过 2 MiB 解析上限"
            );
            ensure!(
                response
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .is_none_or(|value| value.as_bytes().eq_ignore_ascii_case(b"identity")),
                "Instagram 页面返回了不支持的压缩编码"
            );
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| {
                anyhow::anyhow!("Instagram 页面读取失败：{}", error.without_url())
            })? {
                ensure!(
                    bytes.len().saturating_add(chunk.len()) <= MAX_BYTES,
                    "Instagram 页面超过 2 MiB 解析上限"
                );
                bytes.extend_from_slice(&chunk);
            }
            let html = String::from_utf8(bytes).context("Instagram 页面不是有效的 UTF-8 文本")?;
            parse_html(&html, &code)
        };
    tokio::time::timeout(Duration::from_secs(8), work)
        .await
        .context("Instagram 公开分享页读取超时")?
}

fn check_status(status: u16) -> Result<()> {
    match status {
        200..=299 => Ok(()),
        401 | 403 => {
            bail!("Instagram 当前拒绝读取作品（HTTP {status}），请在同一网络的浏览器确认可播放")
        }
        404 | 410 => bail!("Instagram 作品不存在、已删除或当前不可访问"),
        429 => bail!("Instagram 请求过于频繁，请稍后重试"),
        300..=399 => bail!("Instagram 页面发生了无法确认的跳转"),
        _ => bail!("Instagram 页面返回 HTTP {status}"),
    }
}

pub(crate) fn parse_json(data: &Value, code: &str) -> Result<Option<GenericResource>> {
    let pk = decode_shortcode(code)?;
    let mut budget = MAX_NODES;
    inspect(data, code, pk, 0, &mut budget)
}

fn inspect(
    data: &Value,
    code: &str,
    pk: u128,
    depth: usize,
    budget: &mut usize,
) -> Result<Option<GenericResource>> {
    ensure!(
        depth <= MAX_DEPTH && *budget > 0,
        "Instagram 播放信息超过解析上限"
    );
    *budget -= 1;
    match data {
        Value::Object(map) => {
            if media_shape(data) {
                match identity(data, code, pk)? {
                    Identity::Target => return resource(data, code),
                    Identity::Other => return Ok(None),
                    Identity::Absent => return Ok(None),
                }
            }
            // A denial envelope is final. Media nodes belonging to another
            // post were discarded above before inspecting their privacy flags.
            check_gate(data)?;
            for (name, child) in map {
                if matches!(
                    name.as_str(),
                    "recommendations"
                        | "recommended_media"
                        | "suggested_media"
                        | "related_media"
                        | "edge_web_feed_timeline"
                        | "user"
                        | "owner"
                        | "edge_media_to_comment"
                        | "edge_media_to_parent_comment"
                        | "edge_media_to_caption"
                        | "caption"
                        | "comments"
                        | "likers"
                ) {
                    continue;
                }
                if let Some(resource) = inspect(child, code, pk, depth + 1, budget)? {
                    return Ok(Some(resource));
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                if let Some(resource) = inspect(child, code, pk, depth + 1, budget)? {
                    return Ok(Some(resource));
                }
            }
        }
        _ => {}
    }
    Ok(None)
}

fn check_gate(data: &Value) -> Result<()> {
    for field in [
        "is_private",
        "is_gated",
        "requires_login",
        "login_required",
        "require_login",
        "is_unavailable",
    ] {
        ensure!(
            data[field] != true,
            "Instagram 作品当前需要登录或额外访问权限"
        );
    }
    for field in ["user", "owner"] {
        ensure!(
            data[field]["is_private"] != true,
            "Instagram 私密作品不能通过公开链接解析"
        );
    }
    ensure!(
        data.get("if_gated").is_none_or(Value::is_null)
            && data.get("gating_info").is_none_or(Value::is_null),
        "Instagram 作品当前受访问限制，未提供公开播放信息"
    );
    if let Some(message) = data["message"].as_str() {
        ensure!(
            !matches!(
                message,
                "login_required" | "checkpoint_required" | "challenge_required"
            ),
            "Instagram 当前要求登录或人工验证"
        );
    }
    Ok(())
}

fn media_shape(data: &Value) -> bool {
    [
        "video_versions",
        "video_url",
        "is_video",
        "media_type",
        "carousel_media",
        "edge_sidecar_to_children",
    ]
    .iter()
    .any(|key| data.get(*key).is_some())
}

enum Identity {
    Target,
    Other,
    Absent,
}

fn numeric_id(value: &Value) -> Option<u128> {
    if let Some(number) = value.as_u64() {
        return Some(u128::from(number));
    }
    let raw = value.as_str()?;
    let mut parts = raw.split('_');
    let first = parts.next()?;
    if first.is_empty() || first.len() > 39 || !first.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if let Some(owner) = parts.next() {
        if owner.is_empty() || !owner.bytes().all(|b| b.is_ascii_digit()) || parts.next().is_some()
        {
            return None;
        }
    }
    first.parse().ok()
}

// The logged-out Polaris response prefixes its media node ID; the decimal
// suffix still identifies the exact same media PK, not a separate opaque ID.
fn media_id(value: &Value) -> Option<u128> {
    if let Some(raw) = value.as_str().and_then(|raw| raw.strip_prefix("POLARIS_")) {
        return (!raw.is_empty()
            && raw.len() <= 39
            && raw.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| raw.parse().ok())
        .flatten();
    }
    numeric_id(value)
}

fn identity(data: &Value, code: &str, pk: u128) -> Result<Identity> {
    let mut matches = Vec::new();
    for field in ["code", "shortcode"] {
        if let Some(value) = data.get(field).filter(|value| !value.is_null()) {
            matches.push(value.as_str() == Some(code));
        }
    }
    for field in ["pk", "id"] {
        if let Some(value) = data.get(field).filter(|value| !value.is_null()) {
            let actual = if field == "id" {
                media_id(value)
            } else {
                numeric_id(value)
            };
            matches.push(actual == Some(pk));
        }
    }
    if matches.is_empty() {
        return Ok(Identity::Absent);
    }
    if matches.iter().all(|matches| !matches) {
        return Ok(Identity::Other);
    }
    ensure!(
        matches.iter().all(|matches| *matches),
        "Instagram 返回的作品编号相互冲突"
    );
    Ok(Identity::Target)
}

fn video_node(data: &Value) -> bool {
    data["is_video"] == true
        || data["media_type"] == 2
        || data["video_versions"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        || data["video_url"]
            .as_str()
            .is_some_and(|url| !url.is_empty())
}

fn resource(data: &Value, code: &str) -> Result<Option<GenericResource>> {
    check_gate(data)?;
    let video = if video_node(data) {
        data
    } else {
        let children = data.get("carousel_media").and_then(Value::as_array);
        let legacy = data
            .pointer("/edge_sidecar_to_children/edges")
            .and_then(Value::as_array);
        let count = children.or(legacy).map(Vec::len).unwrap_or(0);
        ensure!(count <= 32, "Instagram 轮播作品数量超过解析上限");
        let child = children
            .and_then(|items| items.iter().find(|item| video_node(item)))
            .or_else(|| {
                legacy.and_then(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("node"))
                        .find(|item| video_node(item))
                })
            });
        match child {
            Some(child) => child,
            None => bail!("这个 Instagram 帖子没有视频，当前仅支持视频作品"),
        }
    };
    check_gate(video)?;
    let mut candidates = Vec::new();
    let mut urls = HashSet::new();
    if let Some(versions) = video["video_versions"].as_array() {
        ensure!(versions.len() <= 64, "Instagram 视频格式数量超过解析上限");
        for version in versions {
            if let Some(raw) = version["url"].as_str() {
                add_candidate(
                    &mut candidates,
                    &mut urls,
                    raw,
                    version["height"].as_u64().unwrap_or(0),
                );
            }
        }
    }
    if let Some(raw) = video["video_url"].as_str() {
        add_candidate(
            &mut candidates,
            &mut urls,
            raw,
            video
                .pointer("/dimensions/height")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    let caption = data
        .pointer("/caption/text")
        .and_then(Value::as_str)
        .or_else(|| {
            data.pointer("/edge_media_to_caption/edges/0/node/text")
                .and_then(Value::as_str)
        })
        .or_else(|| data["title"].as_str())
        .unwrap_or("")
        .trim();
    let title = if caption.is_empty() {
        format!("Instagram {code}")
    } else {
        caption
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .take(4096)
            .collect()
    };
    let duration_seconds = video["video_duration"]
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && *n <= 86400.0)
        .map(|n| n.ceil() as u64);
    Ok(Some(GenericResource {
        id: code.into(),
        title,
        duration_seconds,
        candidates,
        embed: None,
    }))
}

fn media_url(raw: &str) -> Option<Url> {
    if raw.len() > 16 * 1024 {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && ["cdninstagram.com", "fbcdn.net"]
            .iter()
            .any(|base| host == *base || host.ends_with(&format!(".{base}")))
        && url.path().to_ascii_lowercase().ends_with(".mp4"))
    .then_some(url)
}

fn add_candidate(
    out: &mut Vec<GenericCandidate>,
    seen: &mut HashSet<String>,
    raw: &str,
    height: u64,
) {
    if out.len() >= MAX_CANDIDATES {
        return;
    }
    let Some(url) = media_url(raw) else {
        return;
    };
    if !seen.insert(url.to_string()) {
        return;
    }
    let height = u32::try_from(height)
        .ok()
        .filter(|n| *n <= 16_384)
        .unwrap_or(0);
    out.push(GenericCandidate {
        id: format!("{:x}", Sha256::digest(url.as_str().as_bytes()))[..16].to_owned(),
        label: if height == 0 {
            "MP4 · 视频".into()
        } else {
            format!("{height}p · MP4")
        },
        height,
        url: url.into(),
        extension: "mp4".into(),
        audio_only: false,
        headers: vec![
            ("User-Agent".into(), USER_AGENT.into()),
            ("Referer".into(), "https://www.instagram.com/".into()),
        ],
        size: None,
        transform: None,
    });
}

/// Parse JSON script data without executing scripts. A plain page shell is not
/// a login failure, and a random og:video without the exact post ID is ignored.
pub(crate) fn parse_html(html: &str, code: &str) -> Result<Option<GenericResource>> {
    ensure!(html.len() <= MAX_BYTES, "Instagram 页面超过 2 MiB 解析上限");
    decode_shortcode(code)?;
    let scripts = Regex::new(r"(?is)<script\b[^>]*>(.*?)</script\s*>")?;
    for (index, capture) in scripts.captures_iter(html).enumerate() {
        ensure!(index < MAX_SCRIPTS, "Instagram 页面脚本数量超过解析上限");
        let raw = capture[1].trim();
        let raw = raw
            .strip_prefix("window._sharedData = ")
            .or_else(|| raw.strip_prefix("window._sharedData="))
            .unwrap_or(raw)
            .trim_end_matches(';')
            .trim();
        if !raw.starts_with('{') && !raw.starts_with('[') {
            continue;
        }
        if let Ok(data) = serde_json::from_str::<Value>(raw) {
            if let Some(resource) = parse_json(&data, code)? {
                return Ok(Some(resource));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const CODE: &str = "Cxample123";
    fn modern() -> Value {
        json!({"code": CODE, "pk": decode_shortcode(CODE).unwrap().to_string(),
            "media_type":2, "video_duration":12.3, "caption":{"text":"A public reel"},
            "video_versions":[{"url":"https://scontent.cdninstagram.com/a.mp4?sig=x", "height":640},
            {"url":"https://video.xx.fbcdn.net/b.mp4", "height":1280}]})
    }
    #[test]
    fn modern_wrapper_and_numeric_pk_produce_safe_progressive_media() {
        let data = json!({"data":{"xig_polaris_media":{"if_not_gated_logged_out":modern()}}});
        let r = parse_json(&data, CODE).unwrap().unwrap();
        assert_eq!(r.id, CODE);
        assert_eq!(r.title, "A public reel");
        assert_eq!(r.duration_seconds, Some(13));
        assert_eq!(r.candidates.len(), 2);
        assert_eq!(r.candidates[1].height, 1280);
        for c in r.candidates {
            assert_eq!(c.extension, "mp4");
            assert!(!c.audio_only);
            assert!(c.transform.is_none());
            assert_eq!(c.headers.len(), 2);
            assert!(c
                .headers
                .iter()
                .all(|(k, _)| matches!(k.as_str(), "User-Agent" | "Referer")));
        }
        let mut node = modern();
        node.as_object_mut().unwrap().remove("code");
        assert!(parse_json(&node, CODE).unwrap().is_some());
    }
    #[test]
    fn polaris_media_id_must_match_the_same_exact_numeric_pk() {
        let pk = decode_shortcode(CODE).unwrap();
        let mut node = modern();
        node["id"] = json!(format!("POLARIS_{pk}"));
        assert!(parse_json(&node, CODE).unwrap().is_some());
        for raw in [
            format!("POLARIS_{}", pk + 1),
            "POLARIS_".into(),
            format!("POLARIS_{pk}_123"),
            format!("polaris_{pk}"),
            "POLARIS_not-a-number".into(),
            "opaque-other-id".into(),
        ] {
            node["id"] = json!(raw);
            assert!(parse_json(&node, CODE).is_err());
        }
        node["id"] = json!(format!("POLARIS_{pk}"));
        node["pk"] = json!(format!("POLARIS_{pk}"));
        assert!(
            parse_json(&node, CODE).is_err(),
            "The pk field is still numeric"
        );
        node.as_object_mut().unwrap().remove("pk");
        node.as_object_mut().unwrap().remove("code");
        assert!(parse_json(&node, CODE).unwrap().is_some());
        node["id"] = json!(format!("POLARIS_{}", pk + 1));
        assert!(parse_json(&node, CODE).unwrap().is_none());
    }
    #[test]
    fn legacy_and_mixed_carousel_select_only_first_video() {
        let data = json!({"data":{"xdt_shortcode_media":{"shortcode":CODE,"is_video":false,
            "edge_media_to_caption":{"edges":[{"node":{"text":"Mixed post"}}]},
            "edge_sidecar_to_children":{"edges":[{"node":{"is_video":false}},
                {"node":{"is_video":true,"shortcode":"ChildFirst","video_url":"https://cdninstagram.com/first.mp4","dimensions":{"height":720}}},
                {"node":{"is_video":true,"video_url":"https://cdninstagram.com/second.mp4"}}]}}}});
        let r = parse_json(&data, CODE).unwrap().unwrap();
        assert_eq!(r.candidates.len(), 1);
        assert_eq!(r.title, "Mixed post");
        assert!(r.candidates[0].url.ends_with("/first.mp4"));
        let carousel = json!({"code":CODE,"media_type":8,"video_versions":null,"video_url":null,
            "carousel_media":[{"media_type":1,"video_versions":null,"video_url":null},modern(),modern()]});
        assert_eq!(
            parse_json(&carousel, CODE)
                .unwrap()
                .unwrap()
                .candidates
                .len(),
            2
        );
    }
    #[test]
    fn rejects_wrong_target_conflicting_ids_and_recommendations() {
        assert!(parse_json(&modern(), "WrongCode").unwrap().is_none());
        let mut node = modern();
        node["pk"] = json!("123456");
        assert!(parse_json(&node, CODE)
            .err()
            .unwrap()
            .to_string()
            .contains("冲突"));
        let other = json!({"code":"WrongCode","is_video":true,"recommendations":[modern()]});
        assert!(parse_json(&other, CODE).unwrap().is_none());
        let unrelated_private = json!({"code":"WrongCode","media_type":2,"is_private":true});
        assert!(parse_json(&json!([unrelated_private, modern()]), CODE)
            .unwrap()
            .is_some());
        assert!(parse_json(&json!({"recommendations":[modern()]}), CODE)
            .unwrap()
            .is_none());
        let anonymous = json!({"is_video":true,"video_url":"https://cdninstagram.com/a.mp4"});
        assert!(parse_json(&anonymous, CODE).unwrap().is_none());
    }
    #[test]
    fn gates_and_private_metadata_never_become_downloads() {
        for field in ["is_private", "is_gated", "requires_login", "login_required"] {
            let mut node = modern();
            node[field] = json!(true);
            assert!(parse_json(&node, CODE).is_err());
        }
        let mut node = modern();
        node["user"] = json!({"is_private":true});
        assert!(parse_json(&node, CODE).is_err());
        let gate = json!({"data":{"xig_polaris_media":{"if_gated":{"preview":modern()}}}});
        assert!(parse_json(&gate, CODE).is_err());
        assert!(parse_json(&json!({"message":"login_required","data":modern()}), CODE).is_err());
    }
    #[test]
    fn ignores_unsafe_urls_manifests_and_non_video_posts() {
        for raw in [
            "http://cdninstagram.com/a.mp4",
            "https://cdninstagram.com.evil.test/a.mp4",
            "https://evilcdninstagram.com/a.mp4",
            "https://u:p@cdninstagram.com/a.mp4",
            "https://cdninstagram.com:8443/a.mp4",
            "https://cdninstagram.com/a.m3u8",
            "https://cdninstagram.com/a.mpd",
            "https://cdninstagram.com/a.jpg",
        ] {
            let mut node = modern();
            node["video_versions"] = json!([{"url":raw}]);
            assert!(parse_json(&node, CODE).unwrap().is_none(), "{raw}");
        }
        assert!(parse_json(&json!({"code":CODE,"media_type":1}), CODE)
            .err()
            .unwrap()
            .to_string()
            .contains("没有视频"));
    }
    #[test]
    fn reads_json_hydration_and_shared_data_but_ignores_unrelated_og_video() {
        let payload = json!({"require":[["ScheduledServerJS",null,null,[{"__bbox":{"result":{"data":{"xig_polaris_media":{"if_not_gated_logged_out":modern()}}}}}]]]});
        let html=format!("<meta property='og:video' content='https://cdninstagram.com/wrong.mp4'><script>unrelated()</script><script type='application/json' data-sjs>{payload}</script>");
        assert!(parse_html(&html, CODE).unwrap().is_some());
        let html = format!(
            "<script>window._sharedData = {};</script>",
            json!({"entry_data":{"PostPage":[{"graphql":{"shortcode_media":modern()}}]}})
        );
        assert!(parse_html(&html, CODE).unwrap().is_some());
        assert!(parse_html("<title>Instagram</title><meta property='og:video' content='https://cdninstagram.com/wrong.mp4'>",CODE).unwrap().is_none());
    }
    #[test]
    fn shortcodes_and_response_errors_are_bounded_and_specific() {
        assert_eq!(decode_shortcode("AAAA_").unwrap(), 63);
        assert_eq!(decode_shortcode("AAAA-").unwrap(), 62);
        assert!(decode_shortcode("______________________").is_err());
        assert!(decode_shortcode("abc!?").is_err());
        assert!(shortcode(
            &Url::parse("https://www.instagram.com/p/Cxample123/?igsh=public").unwrap()
        )
        .is_ok());
        assert!(
            shortcode(&Url::parse("https://www.instagram.com/accounts/login/").unwrap()).is_err()
        );
        assert!(check_status(401).unwrap_err().to_string().contains("拒绝"));
        assert!(check_status(404)
            .unwrap_err()
            .to_string()
            .contains("不存在"));
        assert!(check_status(429).unwrap_err().to_string().contains("频繁"));
        assert!(check_status(500).unwrap_err().to_string().contains("500"));
    }
    #[test]
    fn caps_duplicate_formats_and_metadata_work() {
        let mut node = modern();
        node["video_versions"] = json!((0..40)
            .map(|i| json!({"url":format!("https://cdninstagram.com/{i}.mp4"),"height":720}))
            .collect::<Vec<_>>());
        assert_eq!(
            parse_json(&node, CODE).unwrap().unwrap().candidates.len(),
            MAX_CANDIDATES
        );
        node["video_versions"] = json!([{"url":"https://cdninstagram.com/a.mp4"},{"url":"https://cdninstagram.com/a.mp4"}]);
        assert_eq!(
            parse_json(&node, CODE).unwrap().unwrap().candidates.len(),
            1
        );
        assert!(parse_html(&"x".repeat(MAX_BYTES + 1), CODE).is_err());
    }
}
