//! Read the requested video's normal public browser data when static endpoints
//! return only a page shell. This uses a fresh browser profile and never imports
//! account cookies, synthesizes signatures or selects arbitrary media traffic.
use crate::{
    browser::HeadlessPage,
    chinese_video::{parse_douyin, parse_douyin_data, parse_douyin_feed, NativeVideo},
};
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use reqwest_media::Url;
use serde_json::{json, Value};
use std::{collections::HashMap, time::Duration};

const TOTAL_TIME: Duration = Duration::from_secs(18);
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_TRACKED_REQUESTS: usize = 48;

pub(crate) async fn resolve(id: &str) -> Result<NativeVideo> {
    ensure!(
        (12..=24).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit()),
        "抖音作品编号无效"
    );
    let started = tokio::time::Instant::now();
    let mut browser = tokio::time::timeout(Duration::from_secs(8), HeadlessPage::launch())
        .await
        .context("抖音网页解析浏览器启动超时")??;
    let result = tokio::time::timeout(
        TOTAL_TIME.saturating_sub(started.elapsed()),
        capture(&mut browser, id),
    )
    .await
    .context("抖音网页尚未提供该作品的数据；请在浏览器确认分享链接能正常播放后重试")
    .and_then(|result| result);
    // HeadlessPage also owns a cancellation-safe child/profile cleanup guard.
    let _ = tokio::time::timeout(Duration::from_secs(2), browser.close()).await;
    result
}

// Reading already available page state does not run a replacement player or
// request a verification token. Ordinary login buttons are not access gates.
const INSPECT_PAGE: &str = r#"(() => {
  const visible = e => {
    if (!e) return false;
    const r = e.getBoundingClientRect(), s = getComputedStyle(e);
    return r.width > 20 && r.height > 20 && s.display !== 'none'
      && s.visibility !== 'hidden' && s.opacity !== '0';
  };
  let left = 120000;
  const states = [];
  for (const value of [globalThis._ROUTER_DATA, globalThis.__INITIAL_STATE__]) {
    try {
      const text = JSON.stringify(value);
      if (text && text.length <= left) { states.push(text); left -= text.length; }
    } catch (_) {}
  }
  const scripts = [];
  for (const e of document.querySelectorAll('script')) {
    const text = e.textContent || '';
    if (e.id === 'RENDER_DATA' || text.includes('_ROUTER_DATA')) {
      const html = e.outerHTML;
      if (html.length <= left) { scripts.push(html); left -= html.length; }
    }
  }
  const selectors = ['#captcha_container', '#captcha-verify-container',
    '.captcha_verify_container', '.captcha-verify-container',
    'iframe[src*="verifycenter"]', 'iframe[src*="captcha"]'];
  return { pageUrl: location.href, userAgent: navigator.userAgent, states, html: scripts.join('\n'),
    challengeVisible: selectors.some(s => [...document.querySelectorAll(s)].some(visible)) };
})()"#;

async fn navigate(browser: &mut HeadlessPage, id: &str, mobile: bool) -> Result<String> {
    let url = if mobile {
        format!("https://www.iesdouyin.com/share/video/{id}/")
    } else {
        format!("https://www.douyin.com/video/{id}")
    };
    let result = browser.call("Page.navigate", json!({"url": url})).await?;
    ensure!(
        result.get("errorText").is_none(),
        "浏览器无法打开抖音作品页面"
    );
    result["frameId"]
        .as_str()
        .map(str::to_owned)
        .context("无法确认抖音作品页面")
}

async fn capture(browser: &mut HeadlessPage, id: &str) -> Result<NativeVideo> {
    browser
        .call(
            "Network.enable",
            json!({"maxTotalBufferSize": 2 * 1024 * 1024,
                "maxResourceBufferSize": MAX_BODY_BYTES}),
        )
        .await?;
    let mut frame = navigate(browser, id, false).await?;
    let started = tokio::time::Instant::now();
    let mut mobile = false;
    let mut traffic = MetadataTraffic::default();
    loop {
        let inspection = browser
            .call(
                "Runtime.evaluate",
                json!({"expression": INSPECT_PAGE, "returnByValue": true}),
            )
            .await?;
        let snapshot = &inspection["result"]["value"];
        for event in browser.take_events() {
            traffic.observe(&event, &frame, id);
        }
        // Target metadata takes priority over optional login prompts and other
        // overlays; an unrelated recommended video is never a successful result.
        if let Some(video) = snapshot_video(snapshot, id)? {
            return with_browser_agent(video, snapshot);
        }
        for request_id in traffic.take_ready() {
            if let Ok(response) = browser
                .call("Network.getResponseBody", json!({"requestId": request_id}))
                .await
            {
                if let Some(data) = response_json(&response) {
                    if let Ok(video) = parse_data(&data, id) {
                        return with_browser_agent(video, snapshot);
                    }
                }
            }
        }
        check_access_gate(snapshot, id)?;
        if !mobile && started.elapsed() >= Duration::from_secs(7) {
            frame = navigate(browser, id, true).await?;
            traffic = MetadataTraffic::default();
            mobile = true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn parse_data(data: &Value, id: &str) -> Result<NativeVideo> {
    parse_douyin_data(data, id).or_else(|_| parse_douyin_feed(data, id))
}

fn with_browser_agent(mut video: NativeVideo, snapshot: &Value) -> Result<NativeVideo> {
    let agent = snapshot["userAgent"]
        .as_str()
        .filter(|agent| !agent.is_empty() && agent.len() <= 1024)
        .context("无法确认抖音网页解析的浏览器环境")?;
    ensure!(
        reqwest_media::header::HeaderValue::from_str(agent).is_ok(),
        "抖音网页解析的浏览器信息无效"
    );
    for variant in &mut video.variants {
        variant
            .headers
            .retain(|(key, _)| !key.eq_ignore_ascii_case("user-agent"));
        variant.headers.push(("User-Agent".into(), agent.into()));
    }
    Ok(video)
}

fn snapshot_video(snapshot: &Value, id: &str) -> Result<Option<NativeVideo>> {
    if !snapshot["pageUrl"]
        .as_str()
        .is_some_and(|url| target_page(url, id))
    {
        return Ok(None);
    }
    for state in snapshot["states"].as_array().into_iter().flatten() {
        if let Some(raw) = state.as_str().filter(|raw| raw.len() <= MAX_BODY_BYTES) {
            if let Ok(data) = serde_json::from_str(raw) {
                if let Ok(video) = parse_data(&data, id) {
                    return Ok(Some(video));
                }
            }
        }
    }
    if let Some(html) = snapshot["html"]
        .as_str()
        .filter(|html| html.len() <= MAX_BODY_BYTES)
    {
        if let Ok(video) = parse_douyin(html, id) {
            return Ok(Some(video));
        }
    }
    Ok(None)
}

fn check_access_gate(snapshot: &Value, id: &str) -> Result<()> {
    let raw = snapshot["pageUrl"].as_str().unwrap_or("");
    if raw.is_empty() || raw == "about:blank" {
        return Ok(());
    }
    let url = Url::parse(raw).context("抖音网页地址无效")?;
    if matches!(url.host_str(), Some("www.douyin.com" | "douyin.com"))
        && (url.path().starts_with("/login") || url.path().starts_with("/passport/"))
    {
        bail!("抖音要求在浏览器登录后查看该作品，请先在官网确认可播放");
    }
    ensure!(official_url(&url), "抖音作品页面跳转到了其他网站");
    if target_page(raw, id) && snapshot["challengeVisible"].as_bool() == Some(true) {
        bail!("抖音要求人工安全验证，请先在浏览器完成验证后重试");
    }
    Ok(())
}

fn official_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(
            url.host_str(),
            Some("douyin.com" | "www.douyin.com" | "iesdouyin.com" | "www.iesdouyin.com")
        )
}

fn target_page(raw: &str, id: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    official_url(&url)
        && (url.path().trim_end_matches('/') == format!("/video/{id}")
            || url.path().trim_end_matches('/') == format!("/share/video/{id}"))
}

fn metadata_url(raw: &str, id: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let ids: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "aweme_id")
        .map(|(_, value)| value)
        .collect();
    official_url(&url)
        && matches!(url.host_str(), Some("douyin.com" | "www.douyin.com"))
        && url.path().trim_end_matches('/') == "/aweme/v1/web/aweme/detail"
        && ids.len() == 1
        && ids[0] == id
}

#[derive(Default, Debug)]
struct MetadataTraffic {
    requests: HashMap<String, MetadataRequest>,
    ready: Vec<String>,
}

#[derive(Debug)]
struct MetadataRequest {
    url: String,
    response_ok: bool,
}

impl MetadataTraffic {
    fn observe(&mut self, event: &Value, frame: &str, id: &str) {
        let params = &event["params"];
        let Some(request_id) = params["requestId"].as_str().filter(|id| id.len() <= 256) else {
            return;
        };
        match event["method"].as_str() {
            Some("Network.requestWillBeSent") => {
                self.requests.remove(request_id);
                let Some(url) = params["request"]["url"].as_str() else {
                    return;
                };
                if self.requests.len() < MAX_TRACKED_REQUESTS
                    && params["frameId"].as_str() == Some(frame)
                    && params["documentURL"]
                        .as_str()
                        .is_some_and(|url| target_page(url, id))
                    && metadata_url(url, id)
                {
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
                    request.response_ok = params["frameId"].as_str() == Some(frame)
                        && params["response"]["url"].as_str() == Some(request.url.as_str())
                        && params["response"]["status"].as_u64() == Some(200)
                        && matches!(params["type"].as_str(), Some("XHR" | "Fetch"))
                        && matches!(
                            params["response"]["mimeType"].as_str(),
                            Some("application/json" | "text/json" | "text/plain")
                        );
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
            }
            _ => {}
        }
    }

    fn take_ready(&mut self) -> Vec<String> {
        std::mem::take(&mut self.ready)
    }
}

fn response_json(response: &Value) -> Option<Value> {
    let body = response["body"].as_str()?;
    if response["base64Encoded"].as_bool() == Some(true) {
        if body.len() > MAX_BODY_BYTES.div_ceil(3) * 4 {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(body)
            .ok()?;
        (bytes.len() <= MAX_BODY_BYTES)
            .then(|| serde_json::from_slice(&bytes).ok())
            .flatten()
    } else {
        (body.len() <= MAX_BODY_BYTES)
            .then(|| serde_json::from_str(body).ok())
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "1234567890123456789";
    const PAGE: &str = "https://www.douyin.com/video/1234567890123456789";
    const API: &str =
        "https://www.douyin.com/aweme/v1/web/aweme/detail/?aweme_id=1234567890123456789";

    #[tokio::test]
    #[ignore = "requires a public Douyin sample, network and Chrome or Edge"]
    async fn public_browser_smoke_when_requested() {
        let id = std::env::var("FFDM_TEST_DOUYIN_ID")
            .expect("set FFDM_TEST_DOUYIN_ID to a public sample's numeric ID");
        let video = resolve(&id)
            .await
            .expect("public browser resolution failed");
        assert_eq!(video.id, id);
        assert!(!video.variants.is_empty());
        eprintln!(
            "Douyin browser: {} · {} formats",
            video.title.chars().take(80).collect::<String>(),
            video.variants.len()
        );
        let variant = video
            .variants
            .iter()
            .find(|variant| variant.label.contains("H.264"))
            .unwrap_or(&video.variants[0]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let mut request = client.get(&variant.url);
        for (key, value) in &variant.headers {
            request = request.header(key, value);
        }
        let mut response = request
            .header("Range", "bytes=0-63")
            .header("Accept-Encoding", "identity")
            .send()
            .await
            .unwrap_or_else(|error| panic!("media probe failed: {}", error.without_url()));
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("unknown")
            .chars()
            .take(80)
            .collect::<String>();
        assert!(matches!(status, 200 | 206), "media probe HTTP {status}");
        let mut prefix = Vec::with_capacity(64);
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
        let has_ftyp = prefix.windows(4).any(|window| window == b"ftyp");
        eprintln!("Douyin media probe: HTTP {status}, {content_type}, ftyp={has_ftyp}");
        assert!(has_ftyp, "media probe did not return MP4 bytes");
    }

    fn request(url: &str, page: &str, frame: &str) -> Value {
        json!({"method":"Network.requestWillBeSent", "params":{
            "requestId":"r1", "frameId":frame, "documentURL":page,
            "request":{"url":url, "headers":{"Cookie":"private-cookie-marker",
                "Authorization":"private-authorization-marker"}}}})
    }
    fn response(url: &str, frame: &str) -> Value {
        json!({"method":"Network.responseReceived", "params":{
            "requestId":"r1", "frameId":frame, "type":"Fetch",
            "response":{"url":url,"status":200,"mimeType":"application/json"}}})
    }
    fn finished(size: usize) -> Value {
        json!({"method":"Network.loadingFinished", "params":{
            "requestId":"r1", "encodedDataLength":size}})
    }
    fn item(id: &str) -> Value {
        json!({"aweme_detail":{"aweme_id":id, "desc":"目标作品",
            "headers":{"Cookie":"private-cookie-marker"},
            "video":{"duration":2000,"play_addr":{"url_list":["https://cdn.example/video.mp4"]}}}})
    }

    #[test]
    fn only_complete_metadata_for_the_target_frame_can_be_read() {
        let mut traffic = MetadataTraffic::default();
        traffic.observe(&request(API, PAGE, "main"), "main", ID);
        traffic.observe(&response(API, "main"), "main", ID);
        assert!(traffic.take_ready().is_empty());
        assert!(!format!("{traffic:?}").contains("private-"));
        traffic.observe(&finished(1234), "main", ID);
        assert_eq!(traffic.take_ready(), ["r1"]);
        assert!(traffic.requests.is_empty());
        for (url, page, frame) in [
            (API, PAGE, "other-frame"),
            (API, "https://www.douyin.com/video/9999999999999999999", "main"),
            ("https://www.douyin.com.evil.test/aweme/v1/web/aweme/detail/?aweme_id=1234567890123456789", PAGE, "main"),
            ("https://www.douyin.com/aweme/v1/web/aweme/detail/?aweme_id=9999999999999999999", PAGE, "main"),
            ("https://www.douyin.com/aweme/v1/web/aweme/detail/?aweme_id=1234567890123456789&aweme_id=9999999999999999999", PAGE, "main"),
        ] {
            traffic.observe(&request(url, page, frame), "main", ID);
            traffic.observe(&response(url, frame), "main", ID);
            traffic.observe(&finished(1234), "main", ID);
            assert!(traffic.take_ready().is_empty());
        }
        traffic.observe(&request(API, PAGE, "main"), "main", ID);
        traffic.observe(
            &response("https://other.example/private", "main"),
            "main",
            ID,
        );
        traffic.observe(&finished(1234), "main", ID);
        assert!(traffic.take_ready().is_empty());
        traffic.observe(&request(API, PAGE, "main"), "main", ID);
        traffic.observe(&response(API, "main"), "main", ID);
        traffic.observe(&finished(MAX_BODY_BYTES + 1), "main", ID);
        assert!(traffic.take_ready().is_empty());
    }

    #[test]
    fn target_data_precedes_overlays_and_never_imports_account_headers() {
        let mut snapshot = json!({"pageUrl":PAGE, "userAgent":"Browser/1.0 Windows",
            "states":[item(ID).to_string()],
            "html":"", "challengeVisible":true});
        let video =
            with_browser_agent(snapshot_video(&snapshot, ID).unwrap().unwrap(), &snapshot).unwrap();
        assert_eq!(video.id, ID);
        assert_eq!(video.variants.len(), 1);
        assert_eq!(
            video.variants[0]
                .headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case("user-agent"))
                .count(),
            1
        );
        assert!(video.variants[0]
            .headers
            .contains(&("User-Agent".into(), "Browser/1.0 Windows".into())));
        assert!(video.variants[0]
            .headers
            .iter()
            .all(|(key, value)| !key.eq_ignore_ascii_case("cookie")
                && !key.eq_ignore_ascii_case("authorization")
                && !value.contains("private-")));
        snapshot["states"] = json!([item("9999999999999999999").to_string()]);
        assert!(snapshot_video(&snapshot, ID).unwrap().is_none());
        assert!(check_access_gate(&snapshot, ID).is_err());
        snapshot["challengeVisible"] = json!(false);
        snapshot["title"] = json!("登录抖音，记录美好生活");
        assert!(check_access_gate(&snapshot, ID).is_ok());
        snapshot["pageUrl"] = json!("https://www.douyin.com.evil.test/video/1234567890123456789");
        snapshot["states"] = json!([item(ID).to_string()]);
        assert!(snapshot_video(&snapshot, ID).unwrap().is_none());
        assert!(check_access_gate(&snapshot, ID).is_err());
    }

    #[test]
    fn metadata_body_is_bounded_and_never_accepted_as_arbitrary_html() {
        let raw = item(ID).to_string();
        assert!(parse_data(
            &response_json(&json!({"body":raw,"base64Encoded":false})).unwrap(),
            ID
        )
        .is_ok());
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        assert!(response_json(&json!({"body":encoded,"base64Encoded":true})).is_some());
        assert!(
            response_json(&json!({"body":"<html>login</html>","base64Encoded":false})).is_none()
        );
        assert!(response_json(
            &json!({"body":"x".repeat(MAX_BODY_BYTES + 1),"base64Encoded":false})
        )
        .is_none());
    }
}
