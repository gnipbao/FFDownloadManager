//! WeChat page adaptation, independently implemented in Rust. Only the small
//! page bridge runs in the site's JS runtime; no external parser is launched.
use futures_util::StreamExt;
use http_body_util::BodyExt;
use hudsucker::{
    hyper::{header, Response},
    Body,
};
use regex::Regex;
use reqwest::Url;
use serde_json::Value;
use std::io::Read;
use std::sync::LazyLock;

// .invalid cannot resolve outside the capture proxy. The bridge must never
// forward a playback key to a real Internet endpoint after capture stops.
pub(crate) const BRIDGE_HOST: &str = "ffdm-capture.invalid";
pub(crate) const MAX_BRIDGE_BODY: usize = 128 * 1024;
pub(crate) const MAX_SCRIPT: usize = 8 * 1024 * 1024;

/// Counts only; no media URL, token, key or page content is exposed in diagnostics.
#[derive(Clone, Default, serde::Serialize)]
pub struct Diagnostics {
    pub connections: u64,
    pub requests: u64,
    pub rejected: u64,
    pub session_requests: u64,
    pub sessions: u64,
    pub control_tunnels: u64,
    pub loaded: u64,
    pub getter: u64,
    pub detail: u64,
    pub invalid_media: u64,
    pub last_issue: Option<String>,
    pub assets: Vec<String>,
}
impl Diagnostics {
    pub(crate) fn status(&mut self, body: &[u8]) -> bool {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return false;
        };
        if value["source"] != "status" {
            return false;
        }
        match value["event"].as_str() {
            Some("loaded") => self.loaded += 1,
            Some("getter") => self.getter += 1,
            Some("detail") => self.detail += 1,
            _ => return false,
        }
        true
    }
    pub(crate) fn asset(&mut self, url: &str) {
        let Ok(mut url) = Url::parse(url) else { return };
        // Only public static CDN asset paths are useful here, never playback-page URLs.
        if url.host_str() != Some("res.wx.qq.com") || !script_target(&url) {
            return;
        }
        url.set_query(None);
        url.set_fragment(None);
        let url = url.to_string();
        if self.assets.len() < 8 && !self.assets.contains(&url) {
            self.assets.push(url);
        }
    }
}

pub(crate) fn media_host(host: &str) -> bool {
    ["finder.video.qq.com", "wxapp.tc.qq.com", "stodownload.com"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}
pub(crate) fn script_target(url: &Url) -> bool {
    let path = url.path();
    path.ends_with(".js")
        && ((url.host_str() == Some("res.wx.qq.com") && path.contains("/web-finder/"))
            || (url.host_str() == Some("channels.weixin.qq.com") && path.starts_with("/web/")))
}
pub(crate) fn page_target(url: &Url) -> bool {
    url.host_str() == Some("channels.weixin.qq.com")
        && ["/web/pages/feed", "/web/pages/home"]
            .iter()
            .any(|p| url.path().starts_with(p))
}
pub(crate) fn trusted_origin(origin: &str) -> bool {
    Url::parse(origin).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str() == Some("channels.weixin.qq.com")
            && u.port_or_known_default() == Some(443)
            && u.username().is_empty()
            && u.password().is_none()
            && u.path() == "/"
            && u.query().is_none()
            && u.fragment().is_none()
    })
}
/// WeChat login, account and messaging connections retain their original TLS.
/// Playback pages and public assets are separate hosts and can still be adapted.
pub(crate) fn control_host(host: &str) -> bool {
    (host == "weixin.qq.com" || host.ends_with(".weixin.qq.com"))
        && !matches!(host, "channels.weixin.qq.com" | "mp.weixin.qq.com")
        || host == "wechat.com"
        || host.ends_with(".wechat.com")
        || host == "weixin.com"
        || host.ends_with(".weixin.com")
}

pub(crate) fn bridge_response(
    status: hudsucker::hyper::StatusCode,
    trusted: bool,
    body: Body,
) -> Response<Body> {
    let mut builder = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::VARY, "Origin")
        .header(header::CONTENT_TYPE, "application/json");
    if trusted {
        builder = builder.header(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            "https://channels.weixin.qq.com",
        );
    }
    builder.body(body).unwrap()
}
pub(crate) fn bridge_rejection(
    method: &str,
    url: &Url,
    origin: &str,
    token: &str,
) -> Option<&'static str> {
    if method != "POST" {
        Some("回传方法不匹配")
    } else if url.scheme() != "https" {
        Some("回传未使用 HTTPS")
    } else if url.path() != format!("/{token}/media") {
        Some("微信播放页仍使用旧抓包会话，请重新打开视频号播放页")
    } else if origin.is_empty() {
        Some("微信回传缺少 Origin 来源信息")
    } else if !trusted_origin(origin) {
        Some("微信回传的 Origin 不是视频号页面")
    } else {
        None
    }
}
/// Strong file identifiers survive expiring tokens, Range requests and CDN
/// aliases. Unknown URLs keep all query data: never merge by size or host alone.
pub(crate) fn file_identity(url: &Url) -> Option<String> {
    if !media_host(url.host_str().unwrap_or("")) {
        return None;
    }
    for name in ["encfilekey", "filekey", "vid"] {
        if let Some((_, value)) = url.query_pairs().find(|(k, v)| k == name && !v.is_empty()) {
            return Some(format!("wechat:{name}:{value}"));
        }
    }
    None
}
pub(crate) fn resource_identity(url: &Url) -> String {
    let mut result = file_identity(url).unwrap_or_else(|| {
        let mut u = url.clone();
        u.set_fragment(None);
        let mut pairs: Vec<_> = u
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        pairs.sort();
        u.set_query(None);
        if !pairs.is_empty() {
            u.query_pairs_mut().extend_pairs(pairs);
        }
        u.into()
    });
    if file_identity(url).is_some() {
        // A format change can change the bytes even for the same source video.
        let mut format: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| {
                matches!(
                    k.as_ref(),
                    "fileFormat" | "fileformat" | "format_id" | "quality" | "definition"
                )
            })
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        format.sort();
        result.push_str(&serde_json::to_string(&format).unwrap());
    }
    result
}

static GET_MEDIA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bget\s+media\s*\(\s*\)\s*\{").unwrap());
static DETAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\basync\s+finderGetCommentDetail\s*\(").unwrap());

pub(crate) fn adapt_script(source: &str, _token: &str) -> Option<String> {
    if source.contains("function __ffdmCaptureMedia033(") || source.len() > MAX_SCRIPT {
        return None;
    }
    let getter = GET_MEDIA.is_match(source);
    let detail = DETAIL.is_match(source);
    if !getter && !detail {
        return None;
    }
    // Keep the original method body, receiver, arguments, result and rejection.
    // Only add a wrapper around the known playback-detail method, not arbitrary
    // fetch/XHR or native bridge calls (which may contain messages or credentials).
    let wrapped = DETAIL.replace_all(source, "async finderGetCommentDetail(...ffdmArgs){const ffdmResult=await this.__ffdmOriginalDetail033(...ffdmArgs);try{__ffdmCaptureMedia033(ffdmResult?.data?.object?.objectDesc,'detail')}catch(_){}return ffdmResult}async __ffdmOriginalDetail033(");
    let wrapped = GET_MEDIA.replace_all(
        &wrapped,
        "${0}try{__ffdmCaptureMedia033(this.objectDesc,'metadata')}catch(_){}",
    );
    let origin = serde_json::to_string(&format!("https://{BRIDGE_HOST}")).unwrap();
    let helper = include_str!("capture_wechat_bridge.js").replace("__FFDM_BRIDGE__", &origin);
    // Appending a hoisted declaration preserves the original strict directive
    // and ES-module import semantics. The newline ends any source-map comment.
    Some(format!("{wrapped}\n{helper}\n"))
}

static JS_ASSET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(["'])([^"'<>\s]+\.js)(["'])"#).unwrap());
fn fresh_assets(source: &str, token: &str) -> String {
    use sha2::{Digest, Sha256};
    // Cache identifiers go to a public CDN; never put the bridge credential there.
    let cache_tag = format!("{:x}", Sha256::digest(format!("ffdm-assets:{token}")));
    JS_ASSET
        .replace_all(source, |c: &regex::Captures<'_>| {
            format!("{}{}?__ffdm={cache_tag}{}", &c[1], &c[2], &c[3])
        })
        .into_owned()
}
fn decoded(raw: &[u8], encoding: &str) -> Option<String> {
    let reader: Box<dyn Read + '_> = match encoding {
        "" | "identity" => Box::new(raw),
        "gzip" => Box::new(flate2::read::GzDecoder::new(raw)),
        "deflate" => Box::new(flate2::read::ZlibDecoder::new(raw)),
        "br" => Box::new(brotli::Decompressor::new(raw, 4096)),
        _ => return None,
    };
    let mut output = Vec::new();
    reader
        .take((MAX_SCRIPT + 1) as u64)
        .read_to_end(&mut output)
        .ok()?;
    if output.len() > MAX_SCRIPT {
        return None;
    }
    String::from_utf8(output).ok()
}
/// Only selected playback HTML/JS is buffered. On a limit, timeout or decoding
/// failure, reconstruct the exact original response and keep forwarding it.
pub(crate) async fn adapt_response(
    response: Response<Body>,
    token: &str,
    page: bool,
) -> (Response<Body>, bool) {
    if response.status() != 200
        || response
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok())
            .is_some_and(|n| n > MAX_SCRIPT)
    {
        return (response, false);
    }
    let encoding = response
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    if !matches!(
        encoding.as_str(),
        "" | "identity" | "gzip" | "deflate" | "br"
    ) {
        return (response, false);
    }
    let (mut parts, body) = response.into_parts();
    let mut stream = body.into_data_stream();
    let mut raw = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(None) => break,
            Ok(Some(Ok(chunk))) if raw.len() + chunk.len() <= MAX_SCRIPT => {
                raw.extend_from_slice(&chunk)
            }
            other => {
                let next = other.ok().flatten();
                let output = futures_util::stream::iter([Ok(bytes::Bytes::from(raw))])
                    .chain(futures_util::stream::iter(next))
                    .chain(stream);
                return (
                    Response::from_parts(parts, Body::from_stream(output)),
                    false,
                );
            }
        }
    }
    let original = |parts| (Response::from_parts(parts, Body::from(raw.clone())), false);
    let Some(source) = decoded(&raw, &encoding) else {
        return original(parts);
    };
    let patched = if page {
        None
    } else {
        adapt_script(&source, token)
    };
    let hooked = patched.is_some();
    // Refresh relative module imports as well as the entry script on each
    // capture session. Otherwise cached player JS never reaches the adapter.
    let output = fresh_assets(patched.as_deref().unwrap_or(&source), token);
    if !hooked && output == source {
        return original(parts);
    }
    for h in [
        header::CONTENT_ENCODING,
        header::ETAG,
        header::LAST_MODIFIED,
        header::TRANSFER_ENCODING,
    ] {
        parts.headers.remove(h);
    }
    for h in ["content-md5", "digest", "content-digest"] {
        parts.headers.remove(h);
    }
    parts.headers.insert(
        header::CONTENT_LENGTH,
        output.len().to_string().parse().unwrap(),
    );
    parts
        .headers
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    if page {
        // Preserve every existing policy, adding only the proxy-owned origin
        // to connect-src. No inline-script or general network exception.
        let policies: Vec<_> = parts
            .headers
            .get_all("content-security-policy")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(str::to_owned)
            .collect();
        if !policies.is_empty() {
            parts.headers.remove("content-security-policy");
            for policy in policies {
                let mut directives: Vec<String> = policy
                    .split(';')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                let bridge = format!("https://{BRIDGE_HOST}");
                if let Some(d) = directives
                    .iter_mut()
                    .find(|s| s.split_whitespace().next() == Some("connect-src"))
                {
                    *d = d
                        .split_whitespace()
                        .filter(|s| *s != "'none'")
                        .chain(std::iter::once(bridge.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ");
                } else {
                    let base = directives
                        .iter()
                        .find(|s| s.split_whitespace().next() == Some("default-src"))
                        .map(|s| {
                            s.split_whitespace()
                                .skip(1)
                                .filter(|s| *s != "'none'")
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                        .unwrap_or_else(|| "*".into());
                    directives.push(format!("connect-src {base} {bridge}"));
                }
                if let Ok(value) = directives.join("; ").parse() {
                    parts.headers.append("content-security-policy", value);
                }
            }
        }
    }
    (Response::from_parts(parts, Body::from(output)), hooked)
}

pub(crate) struct Media {
    pub url: String,
    pub title: String,
    pub key: Option<u64>,
    pub bytes: Option<u64>,
    pub detail: bool,
}
pub(crate) fn parse_media(payload: &[u8]) -> Vec<Media> {
    if payload.len() > MAX_BRIDGE_BODY {
        return vec![];
    }
    let Ok(v) = serde_json::from_slice::<Value>(payload) else {
        return vec![];
    };
    let detail = v["source"] == "detail";
    if !detail && v["source"] != "metadata" {
        return vec![];
    }
    let title: String = v["description"]
        .as_str()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect();
    v["media"]
        .as_array()
        .into_iter()
        .flatten()
        .take(12)
        .filter_map(|m| {
            if m["mediaType"].as_u64() == Some(9) {
                return None;
            }
            let raw = m["url"].as_str()?;
            let token = m["urlToken"].as_str().unwrap_or("");
            if raw.len() + token.len() > 8192
                || (!token.is_empty() && !token.starts_with(['?', '&']))
            {
                return None;
            }
            let full = if raw.contains('?') && token.starts_with('?') {
                format!("{raw}&{}", &token[1..])
            } else {
                format!("{raw}{token}")
            };
            let u = Url::parse(&full).ok()?;
            if !matches!(u.scheme(), "http" | "https")
                || !media_host(u.host_str().unwrap_or(""))
                || u.port_or_known_default() != Some(if u.scheme() == "http" { 80 } else { 443 })
                || !u.username().is_empty()
                || u.password().is_some()
            {
                return None;
            }
            let key = m["decodeKey"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| {
                    m["decodeKey"]
                        .as_u64()
                        .filter(|n| *n <= 9_007_199_254_740_991)
                })
                .filter(|n| *n != 0);
            let bytes = m["fileSize"]
                .as_u64()
                .or_else(|| m["fileSize"].as_str()?.parse().ok())
                .filter(|n| *n > 0);
            Some(Media {
                url: full,
                title: title.clone(),
                key,
                bytes,
                detail,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_page_renews_session_and_retries_media_once() {
        use boa_engine::{Context, Source};
        let mut ctx = Context::default();
        ctx.eval(Source::from_bytes(
            r#"
            var location={hostname:'channels.weixin.qq.com'}, sessions=0, posts=[];
            var fetch=async(url,options)=>{
                if(url.endsWith('/session')) {
                    sessions++;
                    return {ok:true,json:async()=>({token:(sessions===1?'a':'b').repeat(64)})};
                }
                posts.push({url,payload:JSON.parse(options.body),mode:options.mode});
                return {status:url.includes('/aaa')?403:204};
            };
        "#,
        ))
        .unwrap();
        ctx.eval(Source::from_bytes(
            &include_str!("capture_wechat_bridge.js")
                .replace("__FFDM_BRIDGE__", "'https://ffdm-capture.invalid'"),
        ))
        .unwrap();
        ctx.run_jobs();
        assert_eq!(ctx.eval(Source::from_bytes("sessions===2 && posts.length===2 && posts[0].payload.event==='loaded' && posts[1].url.includes('/bbb') && posts.every(p=>p.mode==='cors')")).unwrap().as_boolean(), Some(true));
        // The same loaded JS handles the next active session without reloading.
        ctx.eval(Source::from_bytes("globalThis.__ffdmBridge033.refreshed=0;__ffdmCapturePost033({source:'status',event:'getter'})")).unwrap();
        ctx.run_jobs();
        assert_eq!(
            ctx.eval(Source::from_bytes(
                "sessions===3 && posts.length===3 && posts[2].payload.event==='getter'"
            ))
            .unwrap()
            .as_boolean(),
            Some(true)
        );
    }

    #[test]
    fn bridge_failures_are_bounded_and_other_origins_cannot_reconnect() {
        use boa_engine::{Context, Source};
        let helper = include_str!("capture_wechat_bridge.js")
            .replace("__FFDM_BRIDGE__", "'https://ffdm-capture.invalid'");
        let mut ctx = Context::default();
        ctx.eval(Source::from_bytes("var location={hostname:'unrelated.test'},calls=0;var fetch=async()=>{calls++;throw Error('offline')};")).unwrap();
        ctx.eval(Source::from_bytes(&helper)).unwrap();
        ctx.run_jobs();
        assert_eq!(
            ctx.eval(Source::from_bytes("calls")).unwrap().as_number(),
            Some(0.0)
        );
        ctx.eval(Source::from_bytes("location.hostname='channels.weixin.qq.com';__ffdmCapturePost033({});__ffdmCapturePost033({})")).unwrap();
        ctx.run_jobs();
        assert_eq!(
            ctx.eval(Source::from_bytes(
                "calls===1 && __ffdmBridge033.pending===null"
            ))
            .unwrap()
            .as_boolean(),
            Some(true)
        );
        assert!(!trusted_origin("https://channels.weixin.qq.com/other"));
        assert!(!trusted_origin("https://channels.weixin.qq.com?x=1"));
        assert!(!trusted_origin("null"));
        assert!(control_host("szextshort.weixin.qq.com"));
        assert!(control_host("login.weixin.qq.com"));
        assert!(!control_host("channels.weixin.qq.com"));
        assert!(!control_host("mp.weixin.qq.com"));
        assert!(!control_host("weixin.qq.com.attacker.test"));
        let cached = fresh_assets("import './player.js'", "private-session-credential");
        assert!(!cached.contains("private-session-credential"));
        assert!(cached.contains("player.js?__ffdm="));
    }

    #[test]
    #[ignore = "requires a locally downloaded public WeChat player fixture"]
    fn current_public_player_remains_a_valid_module() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".ffdm-web/wechat-adapter-research");
        let source = std::fs::read_to_string(directory.join("player-original.js")).unwrap();
        let patched = fresh_assets(&adapt_script(&source, "fixture").unwrap(), "fixture");
        let mut context = boa_engine::Context::default();
        boa_engine::Module::parse(boa_engine::Source::from_bytes(&patched), None, &mut context)
            .expect("rewritten public player must parse as a complete ES module");
        std::fs::write(directory.join("player-adapted.mjs"), patched).unwrap();
    }

    #[test]
    fn adapted_player_executes_and_preserves_getter_async_result_and_rejection() {
        use boa_engine::{Context, Source};
        let player = r#"'use strict';
            class Player {
                constructor(){this.objectDesc={description:'test',media:[{url:'https://finder.video.qq.com/v?encfilekey=one',decodeKey:'18446744073709551614'}]};}
                get media(){this.reads=(this.reads||0)+1;return this.objectDesc.media}
                async finderGetCommentDetail(arg){if(arg==='fail')throw new Error('original-error');return {arg,data:{object:{objectDesc:this.objectDesc}}}}
            }
            globalThis.player=new Player();
        "#;
        let mut ctx = Context::default();
        ctx.eval(Source::from_bytes(r#"var reports=[];var location={hostname:'channels.weixin.qq.com'};var fetch=(url,options)=>{if(url.endsWith('/session'))return Promise.resolve({ok:true,json:async()=>({token:'a'.repeat(64)})});reports.push(JSON.parse(options.body));return Promise.resolve({status:204})};"#)).unwrap();
        ctx.eval(Source::from_bytes(
            &adapt_script(player, "session").unwrap(),
        ))
        .unwrap();
        ctx.eval(Source::from_bytes(r#"var media=player.media;player.media;var resolved=false,rejected=false;player.finderGetCommentDetail('ok').then(r=>resolved=r.arg==='ok'&&r.data.object.objectDesc===player.objectDesc);player.finderGetCommentDetail('fail').catch(e=>rejected=e.message==='original-error');"#)).unwrap();
        ctx.run_jobs();
        assert_eq!(ctx.eval(Source::from_bytes("resolved && rejected && player.reads===2 && media===player.objectDesc.media && reports.filter(r=>r.source!=='status').length===2 && reports.filter(r=>r.source!=='status')[0].source==='metadata' && reports.filter(r=>r.source!=='status')[1].source==='detail' && reports.filter(r=>r.source!=='status')[1].media[0].decodeKey==='18446744073709551614'")).unwrap().as_boolean(),Some(true));
    }
    #[tokio::test]
    async fn gzip_scripts_decode_but_oversize_and_unsupported_bodies_pass_through() {
        use std::io::Write;
        let script = "class F {get media(){return this.objectDesc.media}}";
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(script.as_bytes()).unwrap();
        let body = gzip.finish().unwrap();
        let response = Response::builder()
            .header("content-type", "application/javascript")
            .header("content-encoding", "gzip")
            .header("etag", "old")
            .body(Body::from(body))
            .unwrap();
        let (response, hooked) = adapt_response(response, "token", false).await;
        assert!(hooked);
        assert!(!response.headers().contains_key("content-encoding"));
        assert!(!response.headers().contains_key("etag"));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("function __ffdmCaptureMedia033("));
        for (encoding, body) in [
            ("zstd", vec![1, 2, 3]),
            ("identity", vec![b'x'; MAX_SCRIPT + 1]),
        ] {
            let response = Response::builder()
                .header("content-encoding", encoding)
                .body(Body::from(body.clone()))
                .unwrap();
            let (response, hooked) = adapt_response(response, "token", false).await;
            assert!(!hooked);
            assert_eq!(response.headers()["content-encoding"], encoding);
            assert_eq!(
                response
                    .into_body()
                    .collect()
                    .await
                    .unwrap()
                    .to_bytes()
                    .as_ref(),
                body
            );
        }
    }
    #[tokio::test]
    async fn playback_page_refreshes_scripts_and_limits_csp_exception_to_local_bridge() {
        let response = Response::builder()
            .header("content-type", "text/html")
            .header(
                "content-security-policy",
                "default-src 'self'; script-src 'self' https://res.wx.qq.com; connect-src 'self'",
            )
            .body(Body::from(
                "<script src=\"https://res.wx.qq.com/a/web-finder/player.js\"></script>",
            ))
            .unwrap();
        let (response, hooked) = adapt_response(response, "session", true).await;
        assert!(!hooked);
        let policy = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        assert!(policy.contains("connect-src 'self' https://ffdm-capture.invalid"));
        assert!(policy.contains("script-src 'self' https://res.wx.qq.com"));
        assert!(!policy.contains("unsafe-inline"));
        assert!(
            String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes())
                .contains("player.js?__ffdm=")
        );
    }
    #[test]
    fn stable_identity_keeps_files_and_quality_separate() {
        let id = |s| resource_identity(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://finder.video.qq.com/251/stodownload?encfilekey=a&token=one&idx=1"),
            id("https://wxapp.tc.qq.com/other?idx=8&token=two&encfilekey=a")
        );
        assert_ne!(
            id("https://finder.video.qq.com/v?encfilekey=a"),
            id("https://finder.video.qq.com/v?encfilekey=b")
        );
        assert_ne!(
            id("https://finder.video.qq.com/v?encfilekey=a&fileFormat=hd"),
            id("https://finder.video.qq.com/v?encfilekey=a&fileFormat=sd")
        );
        assert_ne!(
            id("https://finder.video.qq.com/v?id=a"),
            id("https://finder.video.qq.com/v?id=b")
        );
    }
    #[test]
    fn scripts_are_scoped_and_original_methods_are_preserved() {
        assert!(script_target(&Url::parse("https://res.wx.qq.com/t/wx_fed/finder/web/web-finder/res/js/virtual_svg-icons-register.publish.123.js?v=1").unwrap()));
        assert!(!script_target(
            &Url::parse("https://res.wx.qq.com/other/login.js").unwrap()
        ));
        let script = "'use strict';class Feed {get media(){return this.objectDesc.media}async finderGetCommentDetail(arg){return await this.get(arg)}}";
        let patched = adapt_script(script, "session").unwrap();
        assert!(patched.starts_with("'use strict';"));
        assert!(patched.contains("async __ffdmOriginalDetail033(arg){return await this.get(arg)}"));
        assert!(patched.contains("https://ffdm-capture.invalid"));
        assert!(!patched.contains("/session/media"));
        assert!(adapt_script(&patched, "session").is_none());
        assert!(adapt_script("const nothing=1", "session").is_none());
    }
    #[test]
    fn payload_is_narrow_and_does_not_round_large_keys() {
        let payload = serde_json::json!({"source":"detail","description":"标题","media":[
            {"url":"https://finder.video.qq.com/v?encfilekey=a","urlToken":"&token=secret","decodeKey":"18446744073709551614","fileSize":"123"},
            {"url":"http://127.0.0.1/secret","decodeKey":"9"},
            {"url":"https://finder.video.qq.com/cover","mediaType":9},
            {"url":"https://finder.video.qq.com/v?encfilekey=b","decodeKey":18446744073709551614u64}]});
        let media = parse_media(&serde_json::to_vec(&payload).unwrap());
        assert_eq!(media.len(), 2);
        assert_eq!(media[0].key, Some(18446744073709551614));
        assert_eq!(media[0].bytes, Some(123));
        assert!(media[0].detail);
        assert!(media[1].key.is_none());
        assert!(trusted_origin("https://channels.weixin.qq.com"));
        assert!(!trusted_origin(
            "https://channels.weixin.qq.com.attacker.test"
        ));
    }

    #[test]
    fn http_playback_urls_work_without_accepting_arbitrary_hosts_or_ports() {
        let payload = serde_json::json!({"source":"detail","description":"HTTP 视频","media":[
            {"url":"http://finder.video.qq.com/v?encfilekey=one","urlToken":"&token=private","decodeKey":"18446744073709551614"},
            {"url":"http://finder.video.qq.com:8080/v"},
            {"url":"https://finder.video.qq.com.attacker.test/v"},
            {"url":"http://127.0.0.1/v"},
            {"url":"file:///tmp/v"}
        ]});
        let media = parse_media(&serde_json::to_vec(&payload).unwrap());
        assert_eq!(media.len(), 1);
        assert!(media[0].url.starts_with("http://finder.video.qq.com/"));
        assert_eq!(media[0].key, Some(18446744073709551614));
        assert!(media[0].detail);
        let mut diagnostics = Diagnostics::default();
        assert!(diagnostics.status(br#"{"source":"status","event":"loaded"}"#));
        assert!(diagnostics.status(br#"{"source":"status","event":"getter"}"#));
        assert_eq!((diagnostics.loaded, diagnostics.getter), (1, 1));
        diagnostics.asset("https://res.wx.qq.com/a/web-finder/player.js?token=private");
        assert!(!serde_json::to_string(&diagnostics)
            .unwrap()
            .contains("private"));
    }
}
