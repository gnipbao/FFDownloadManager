//! Read an Instagram post's normal public playback metadata in a disposable
//! browser. The browser uses its own anonymous session; account profiles,
//! account cookies and unrelated media requests are never imported.
use crate::{browser::HeadlessPage, generic::GenericResource, instagram};
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use reqwest::Url;
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};

const TOTAL_TIME: Duration = Duration::from_secs(20);
// CDP wraps response bodies in JSON. Leave space for escaping below its 1 MiB
// message limit, including when a small compressed response expands in memory.
const MAX_BODY_BYTES: usize = 384 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 192 * 1024;
const MAX_TRACKED_REQUESTS: usize = 48;

pub(crate) async fn resolve(code: &str) -> Result<GenericResource> {
    ensure!(valid_code(code), "Instagram 作品编号无效");
    let started = tokio::time::Instant::now();
    let mut browser = tokio::time::timeout(Duration::from_secs(8), HeadlessPage::launch())
        .await
        .context("Instagram 网页解析浏览器启动超时")??;
    let result = tokio::time::timeout(
        TOTAL_TIME.saturating_sub(started.elapsed()),
        capture(&mut browser, code),
    )
    .await
    .context("Instagram 网页没有提供该作品的公开播放数据；请确认该链接能在未登录的浏览器中播放")
    .and_then(|result| result);
    // The owned HeadlessPage also cleans up its child and temporary profile
    // when this future is cancelled by the host's outer extraction timeout.
    let _ = tokio::time::timeout(Duration::from_secs(2), browser.close()).await;
    result
}

const INSPECT_PAGE: &str = r#"(() => {
  const visible = e => {
    const r = e.getBoundingClientRect(), s = getComputedStyle(e);
    return r.width > 20 && r.height > 20 && s.display !== 'none'
      && s.visibility !== 'hidden' && s.opacity !== '0';
  };
  const encoder = new TextEncoder();
  let left = 196608;
  const scripts = [];
  for (const element of document.querySelectorAll('script[type="application/json"],script[data-sjs]')) {
    if (scripts.length >= 96) break;
    const text = element.textContent || '';
    if (!text.includes('xig_polaris_media') && !text.includes('shortcode_media')
        && !text.includes('video_versions')) continue;
    const bytes = encoder.encode(text).byteLength;
    if (bytes <= left) { scripts.push(text); left -= bytes; }
  }
  return { pageUrl: location.href, userAgent: navigator.userAgent, scripts,
    challengeVisible: [...document.querySelectorAll(
      'iframe[src*="recaptcha"],iframe[src*="hcaptcha"],form[action*="/challenge/"]'
    )].some(visible) };
})()"#;

async fn capture(browser: &mut HeadlessPage, code: &str) -> Result<GenericResource> {
    browser
        .call(
            "Network.enable",
            json!({"maxTotalBufferSize": 4 * 1024 * 1024,
                "maxResourceBufferSize": MAX_BODY_BYTES}),
        )
        .await
        .context("无法开始读取 Instagram 网页播放信息")?;
    let navigation = browser
        .call_with_timeout(
            "Page.navigate",
            json!({"url": format!("https://www.instagram.com/p/{code}/")}),
            Duration::from_secs(8),
        )
        .await
        .context("打开 Instagram 作品页面失败")?;
    ensure!(
        navigation.get("errorText").is_none(),
        "浏览器无法打开 Instagram 作品页面"
    );
    let frame = navigation["frameId"]
        .as_str()
        .context("无法确认 Instagram 作品页面")?
        .to_owned();
    let mut traffic = MetadataTraffic::default();
    loop {
        let inspection = browser
            .call_with_timeout(
                "Runtime.evaluate",
                json!({"expression": INSPECT_PAGE, "returnByValue": true}),
                Duration::from_secs(6),
            )
            .await
            .context("读取 Instagram 作品页面状态失败")?;
        let snapshot = &inspection["result"]["value"];
        for event in browser.take_events() {
            traffic.observe(&event, &frame, code);
        }
        check_access_gate(snapshot)?;
        // A normal signup prompt is not a gate. Only metadata belonging to the
        // requested post can produce a result, even if recommendations load.
        if let Some(resource) = snapshot_resource(snapshot, code)? {
            return with_browser_agent(resource, snapshot);
        }
        if snapshot["pageUrl"]
            .as_str()
            .is_some_and(|url| target_page(url, code))
        {
            for request_id in traffic.take_ready() {
                if let Ok(response) = browser
                    .call("Network.getResponseBody", json!({"requestId": request_id}))
                    .await
                {
                    if let Some(resource) = response_resource(&response, code)? {
                        return with_browser_agent(resource, snapshot);
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn with_browser_agent(mut resource: GenericResource, snapshot: &Value) -> Result<GenericResource> {
    let agent = snapshot["userAgent"]
        .as_str()
        .filter(|agent| !agent.is_empty() && agent.len() <= 1024)
        .context("无法确认 Instagram 网页解析的浏览器环境")?;
    ensure!(
        reqwest::header::HeaderValue::from_str(agent).is_ok(),
        "Instagram 网页解析的浏览器信息无效"
    );
    for candidate in &mut resource.candidates {
        candidate.headers = vec![
            ("User-Agent".into(), agent.into()),
            ("Referer".into(), "https://www.instagram.com/".into()),
        ];
    }
    Ok(resource)
}

fn snapshot_resource(snapshot: &Value, code: &str) -> Result<Option<GenericResource>> {
    if !snapshot["pageUrl"]
        .as_str()
        .is_some_and(|url| target_page(url, code))
    {
        return Ok(None);
    }
    let mut bytes = 0usize;
    for script in snapshot["scripts"]
        .as_array()
        .into_iter()
        .flatten()
        .take(96)
    {
        let Some(raw) = script.as_str() else {
            continue;
        };
        bytes = bytes.saturating_add(raw.len());
        ensure!(bytes <= MAX_SNAPSHOT_BYTES, "Instagram 页面播放信息过大");
        if let Ok(data) = serde_json::from_str(raw) {
            if let Some(resource) = instagram::parse_json(&data, code)? {
                return Ok(Some(resource));
            }
        }
    }
    Ok(None)
}

fn check_access_gate(snapshot: &Value) -> Result<()> {
    let raw = snapshot["pageUrl"].as_str().unwrap_or("");
    if raw.is_empty() || raw == "about:blank" {
        return Ok(());
    }
    let url = Url::parse(raw).context("Instagram 网页地址无效")?;
    ensure!(official_url(&url), "Instagram 作品页面跳转到了其他网站");
    if url.path().starts_with("/accounts/login") {
        bail!("Instagram 要求登录后查看该作品；当前网页备用解析仅支持公开内容");
    }
    if url.path().starts_with("/challenge/")
        || url.path().starts_with("/checkpoint/")
        || snapshot["challengeVisible"].as_bool() == Some(true)
    {
        bail!("Instagram 要求人工安全验证，暂时无法读取该作品的公开播放数据");
    }
    Ok(())
}

fn valid_code(code: &str) -> bool {
    (5..=24).contains(&code.len())
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn official_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.host_str(), Some("www.instagram.com" | "instagram.com"))
}

fn target_page(raw: &str, code: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    if !official_url(&url) {
        return false;
    }
    let segments: Vec<_> = url.path().trim_matches('/').split('/').collect();
    match segments.as_slice() {
        [kind, actual] | [_, kind, actual] => {
            matches!(*kind, "p" | "reel" | "reels" | "tv") && *actual == code
        }
        _ => false,
    }
}

fn metadata_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    if !official_url(&url) || url.host_str() != Some("www.instagram.com") {
        return false;
    }
    let path = url.path().trim_end_matches('/');
    if matches!(path, "/api/graphql" | "/graphql/query") {
        return true;
    }
    // The payload still has to match the requested shortcode or numeric ID.
    path.strip_prefix("/api/v1/media/")
        .and_then(|rest| rest.strip_suffix("/info"))
        .is_some_and(|id| {
            !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_digit() || b == b'_')
        })
}

#[derive(Default, Debug)]
struct MetadataTraffic {
    requests: HashMap<String, MetadataRequest>,
    ready: Vec<String>,
    accepted: usize,
}

#[derive(Debug)]
struct MetadataRequest {
    url: String,
    response_ok: bool,
}

impl MetadataTraffic {
    fn observe(&mut self, event: &Value, frame: &str, code: &str) {
        let params = &event["params"];
        let Some(request_id) = params["requestId"].as_str().filter(|id| id.len() <= 256) else {
            return;
        };
        match event["method"].as_str() {
            Some("Network.requestWillBeSent") => {
                self.requests.remove(request_id);
                self.ready.retain(|id| id != request_id);
                let Some(url) = params["request"]["url"].as_str() else {
                    return;
                };
                if self.accepted < MAX_TRACKED_REQUESTS
                    && params["frameId"].as_str() == Some(frame)
                    && params["documentURL"]
                        .as_str()
                        .is_some_and(|url| target_page(url, code))
                    && metadata_url(url)
                {
                    self.accepted += 1;
                    self.requests.insert(
                        request_id.into(),
                        MetadataRequest {
                            url: url.into(),
                            response_ok: false,
                        },
                    );
                }
            }
            Some("Network.responseReceived") => {
                if let Some(request) = self.requests.get_mut(request_id) {
                    let response = &params["response"];
                    request.response_ok = params["frameId"].as_str() == Some(frame)
                        && response["url"].as_str() == Some(request.url.as_str())
                        && response["status"].as_u64() == Some(200)
                        && matches!(params["type"].as_str(), Some("XHR" | "Fetch"))
                        && response["mimeType"].as_str().is_some_and(|mime| {
                            matches!(
                                mime.split(';').next().unwrap_or("").trim(),
                                "application/json"
                                    | "text/json"
                                    | "text/plain"
                                    | "application/x-javascript"
                            )
                        });
                }
            }
            Some("Network.loadingFinished") => {
                if let Some(request) = self.requests.remove(request_id) {
                    if request.response_ok
                        && self.ready.len() < 8
                        && params["encodedDataLength"]
                            .as_f64()
                            .is_some_and(|len| len >= 0.0 && len <= MAX_BODY_BYTES as f64)
                    {
                        self.ready.push(request_id.into());
                    }
                }
            }
            Some("Network.loadingFailed") => {
                self.requests.remove(request_id);
                self.ready.retain(|id| id != request_id);
            }
            _ => {}
        }
    }

    fn take_ready(&mut self) -> Vec<String> {
        std::mem::take(&mut self.ready)
    }
}

fn response_text(response: &Value) -> Option<String> {
    let body = response["body"].as_str()?;
    if response["base64Encoded"].as_bool() == Some(true) {
        if body.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(body)
            .ok()?;
        (bytes.len() <= MAX_BODY_BYTES)
            .then(|| String::from_utf8(bytes).ok())
            .flatten()
    } else {
        (body.len() <= MAX_BODY_BYTES).then(|| body.to_owned())
    }
}

fn response_resource(response: &Value, code: &str) -> Result<Option<GenericResource>> {
    let Some(body) = response_text(response) else {
        return Ok(None);
    };
    let text = body.trim().trim_start_matches("for (;;);").trim();
    if let Ok(data) = serde_json::from_str(text) {
        return instagram::parse_json(&data, code);
    }
    // Relay can deliver several complete JSON envelopes separated by newlines.
    for line in text.lines().take(16) {
        if let Ok(data) = serde_json::from_str(line) {
            if let Some(resource) = instagram::parse_json(&data, code)? {
                return Ok(Some(resource));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    const CODE: &str = "Cxample123";
    const PAGE: &str = "https://www.instagram.com/p/Cxample123/";
    const API: &str = "https://www.instagram.com/api/graphql";

    fn request(url: &str, page: &str, frame: &str) -> Value {
        json!({"method":"Network.requestWillBeSent","params":{
            "requestId":"r1","frameId":frame,"documentURL":page,
            "request":{"url":url,"headers":{"Cookie":"private-cookie-marker"}}}})
    }
    fn response(url: &str, frame: &str) -> Value {
        json!({"method":"Network.responseReceived","params":{
            "requestId":"r1","frameId":frame,"type":"Fetch",
            "response":{"url":url,"status":200,"mimeType":"application/json"}}})
    }
    fn finished(size: usize) -> Value {
        json!({"method":"Network.loadingFinished","params":{
            "requestId":"r1","encodedDataLength":size}})
    }
    fn video(code: &str) -> Value {
        json!({"data":{"xdt_shortcode_media":{"shortcode":code,"is_video":true,
            "video_url":"https://scontent.cdninstagram.com/video.mp4",
            "dimensions":{"width":360,"height":640}}}})
    }

    #[test]
    fn only_completed_official_metadata_from_requested_document_is_read() {
        let mut traffic = MetadataTraffic::default();
        traffic.observe(&request(API, PAGE, "main"), "main", CODE);
        traffic.observe(&response(API, "main"), "main", CODE);
        assert!(traffic.take_ready().is_empty());
        traffic.observe(&finished(2048), "main", CODE);
        assert_eq!(traffic.take_ready(), ["r1"]);
        assert!(traffic.take_ready().is_empty());
        for (url, page, frame) in [
            (
                "https://www.instagram.com.attacker.test/api/graphql",
                PAGE,
                "main",
            ),
            ("https://www.instagram.com:444/api/graphql", PAGE, "main"),
            ("https://user@www.instagram.com/api/graphql", PAGE, "main"),
            ("https://scontent.cdninstagram.com/video.mp4", PAGE, "main"),
            (API, "https://www.instagram.com/p/OtherPost/", "main"),
            (API, PAGE, "other-frame"),
        ] {
            let mut traffic = MetadataTraffic::default();
            traffic.observe(&request(url, page, frame), "main", CODE);
            traffic.observe(&response(url, frame), "main", CODE);
            traffic.observe(&finished(2048), "main", CODE);
            assert!(traffic.take_ready().is_empty(), "{url} {page} {frame}");
        }
    }

    #[test]
    fn redirects_errors_large_bodies_and_request_floods_are_bounded() {
        let mut traffic = MetadataTraffic::default();
        traffic.observe(&request(API, PAGE, "main"), "main", CODE);
        traffic.observe(
            &request("https://other.test/api/graphql", PAGE, "main"),
            "main",
            CODE,
        );
        traffic.observe(&response(API, "main"), "main", CODE);
        traffic.observe(&finished(2048), "main", CODE);
        assert!(traffic.take_ready().is_empty());
        traffic.observe(&request(API, PAGE, "main"), "main", CODE);
        traffic.observe(&response(API, "main"), "main", CODE);
        traffic.observe(&finished(MAX_BODY_BYTES + 1), "main", CODE);
        assert!(traffic.take_ready().is_empty());
        for n in 0..100 {
            let mut event = request(API, PAGE, "main");
            event["params"]["requestId"] = json!(format!("r{n}"));
            traffic.observe(&event, "main", CODE);
        }
        assert!(traffic.requests.len() <= MAX_TRACKED_REQUESTS);
        assert_eq!(traffic.accepted, MAX_TRACKED_REQUESTS);
        assert!(!format!("{traffic:?}").contains("private-cookie-marker"));
    }

    #[test]
    fn target_path_and_real_gates_are_distinguished_from_signup_prompt() {
        assert!(target_page(PAGE, CODE));
        assert!(target_page(
            "https://www.instagram.com/author/reel/Cxample123/",
            CODE
        ));
        assert!(!target_page(
            "https://www.instagram.com/p/Cxample123/c/comment",
            CODE
        ));
        assert!(!target_page("http://www.instagram.com/p/Cxample123/", CODE));
        assert!(check_access_gate(&json!({"pageUrl":PAGE,"signupVisible":true})).is_ok());
        for path in ["/accounts/login/", "/challenge/123", "/checkpoint/123"] {
            assert!(check_access_gate(
                &json!({"pageUrl":format!("https://www.instagram.com{path}")})
            )
            .is_err());
        }
        assert!(check_access_gate(&json!({"pageUrl":PAGE,"challengeVisible":true})).is_err());
        assert!(check_access_gate(&json!({"pageUrl":"https://other.test/"})).is_err());
    }

    #[test]
    fn bounded_json_and_target_identity_are_required() {
        let raw = video(CODE).to_string();
        let wrapped = json!({"body":format!("for (;;);{raw}"),"base64Encoded":false});
        let resource = response_resource(&wrapped, CODE).unwrap().unwrap();
        assert_eq!(resource.id, CODE);
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        assert!(
            response_resource(&json!({"body":encoded,"base64Encoded":true}), CODE)
                .unwrap()
                .is_some()
        );
        assert!(
            response_resource(&json!({"body":video("OtherPost").to_string()}), CODE)
                .unwrap()
                .is_none()
        );
        assert!(response_text(&json!({"body":"x".repeat(MAX_BODY_BYTES + 1)})).is_none());
        let large =
            base64::engine::general_purpose::STANDARD.encode(vec![b'x'; MAX_BODY_BYTES + 1]);
        assert!(response_text(&json!({"body":large,"base64Encoded":true})).is_none());
        assert!(response_text(&json!({"body":"!bad-base64!","base64Encoded":true})).is_none());
    }

    #[test]
    fn snapshot_metadata_never_uses_another_page_or_transfers_cookies() {
        let mut snapshot = json!({"pageUrl":PAGE,"userAgent":"Browser Test/1", "scripts":[video(CODE).to_string()]});
        let mut resource = snapshot_resource(&snapshot, CODE).unwrap().unwrap();
        resource.candidates[0]
            .headers
            .push(("Cookie".into(), "private-cookie-marker".into()));
        let resource = with_browser_agent(resource, &snapshot).unwrap();
        assert_eq!(
            resource.candidates[0].headers,
            vec![
                ("User-Agent".to_owned(), "Browser Test/1".to_owned()),
                (
                    "Referer".to_owned(),
                    "https://www.instagram.com/".to_owned()
                )
            ]
        );
        snapshot["pageUrl"] = json!("https://www.instagram.com/p/OtherPost/");
        assert!(snapshot_resource(&snapshot, CODE).unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires a public Instagram post, network and Chrome or Edge"]
    async fn public_browser_smoke_when_requested() {
        let code = std::env::var("FFDM_TEST_INSTAGRAM_CODE").expect("set FFDM_TEST_INSTAGRAM_CODE");
        let resource = resolve(&code)
            .await
            .expect("public Instagram browser resolution failed");
        assert_eq!(resource.id, code);
        assert!(!resource.candidates.is_empty());
        eprintln!(
            "Instagram browser: {} · {} formats",
            resource.title.chars().take(80).collect::<String>(),
            resource.candidates.len()
        );
        let candidate = &resource.candidates[0];
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let mut request = client.get(&candidate.url);
        for (name, value) in &candidate.headers {
            request = request.header(name, value);
        }
        let mut response = request
            .header("Range", "bytes=0-63")
            .header("Accept-Encoding", "identity")
            .send()
            .await
            .unwrap_or_else(|error| panic!("media probe failed: {}", error.without_url()));
        let status = response.status().as_u16();
        assert!(matches!(status, 200 | 206), "media probe HTTP {status}");
        let mut prefix = Vec::new();
        while prefix.len() < 64 {
            let Some(chunk) = response
                .chunk()
                .await
                .unwrap_or_else(|error| panic!("media probe body failed: {}", error.without_url()))
            else {
                break;
            };
            prefix.extend_from_slice(&chunk[..chunk.len().min(64 - prefix.len())]);
        }
        assert!(
            prefix.windows(4).any(|bytes| bytes == b"ftyp"),
            "media probe did not return MP4 bytes"
        );
        eprintln!("Instagram media probe: HTTP {status}, MP4 bytes verified");
    }
}
