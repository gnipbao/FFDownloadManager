//! Maintain YouTube client compatibility and verify media before offering formats.
use crate::media::MediaStream;
use anyhow::{bail, ensure, Context, Result};
use futures_util::{stream, StreamExt};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use ytdown::{Extractor, ExtractorContext, HttpClient, HttpRequest, HttpResponse, MediaInfo};

mod auth;
use auth::YoutubeSession;

// ytdown 0.8.0 still starts with Android VR, whose media URLs are now rejected.
// Use the current non-JS player profile in that slot while keeping ytdown's
// parsing, deciphering, and remaining client fallbacks. Reference:
// https://github.com/yt-dlp/yt-dlp/blob/2026.08.19/yt_dlp/extractor/youtube/_base.py
const VISIONOS_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

fn update_player_client(request: &mut HttpRequest, body: &mut Value) {
    if body["context"]["client"]["clientName"] != "ANDROID_VR" {
        return;
    }
    let previous = &body["context"]["client"];
    let mut client = serde_json::json!({
        "clientName": "VISIONOS",
        "clientVersion": "1.02",
        "deviceMake": "Apple",
        "deviceModel": "RealityDevice17,1",
        "userAgent": VISIONOS_UA,
        "osName": "visionOS",
        "osVersion": "26.5.23O471",
        "hl": "en",
        "gl": "US"
    });
    // Keep the visitor session and locale, but do not leak Android device fields
    // into the new client identity. Headers and JSON must describe the same client.
    for field in ["visitorData", "hl", "gl"] {
        if let Some(value) = previous.get(field) {
            client[field] = value.clone();
        }
    }
    body["context"]["client"] = client;
    for (name, value) in [
        ("User-Agent", VISIONOS_UA),
        ("X-YouTube-Client-Name", "101"),
        ("X-YouTube-Client-Version", "1.02"),
    ] {
        request
            .headers
            .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        request.headers.push((name.into(), value.into()));
    }
    request.body = Some(serde_json::to_vec(body).expect("JSON value is serializable"));
}

fn update_authorized_client(request: &mut HttpRequest, body: &mut Value) {
    // Mobile/VR clients do not support browser Cookie authentication. Keep the
    // existing extractor/deciphering, but use clients that accept user sessions.
    let tv = matches!(
        body["context"]["client"]["clientName"].as_str(),
        Some("ANDROID_VR" | "TVHTML5")
    );
    let (name, id, version, ua) = if tv {
        ("TVHTML5", "7", "7.20260707.07.00", auth::TV_UA)
    } else {
        ("WEB", "1", "2.20260708.00.00", auth::WEB_UA)
    };
    let mut client = serde_json::json!({"clientName":name,"clientVersion":version,"userAgent":ua,"hl":"en","gl":"US"});
    for field in ["visitorData", "hl", "gl"] {
        if let Some(value) = body["context"]["client"].get(field) {
            client[field] = value.clone();
        }
    }
    body["context"]["client"] = client;
    for (name, value) in [
        ("User-Agent", ua),
        ("X-YouTube-Client-Name", id),
        ("X-YouTube-Client-Version", version),
    ] {
        request
            .headers
            .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        request.headers.push((name.into(), value.into()));
    }
    request.body = Some(serde_json::to_vec(body).expect("JSON value is serializable"));
}

fn has_readable_hd_with_audio(body: &Value) -> bool {
    let formats: Vec<_> = ["formats", "adaptiveFormats"]
        .into_iter()
        .filter_map(|key| body["streamingData"][key].as_array())
        .flatten()
        .filter(|f| f["url"].is_string())
        .collect();
    formats
        .iter()
        .any(|f| f["height"].as_u64().is_some_and(|h| h >= 720))
        && formats.iter().any(|f| f["audioQuality"].is_string())
}

#[derive(Clone)]
pub(crate) struct MediaProbe {
    client: reqwest::Client,
    local_client: reqwest::Client,
    passed: Arc<Mutex<HashMap<String, Instant>>>,
}

impl MediaProbe {
    pub fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .user_agent("gosh-dl/0.6.3")
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(6))
                .build()?,
            local_client: reqwest::Client::builder()
                .no_proxy()
                .user_agent("gosh-dl/0.6.3")
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(6))
                .build()?,
            passed: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn check(&self, media: &MediaStream) -> Result<()> {
        let key = serde_json::to_string(media)?;
        if self
            .passed
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(45))
        {
            return Ok(());
        }
        let url = crate::validation::parse_url(&media.url)?;
        crate::validation::validate_headers(&media.headers)?;
        // Share the source's request budget with downloads, including any
        // Retry-After cooldown recorded by another probe or running job.
        let _source_permit = crate::source_policy::acquire(&media.url).await?;
        let client = if url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }) {
            &self.local_client
        } else {
            &self.client
        };
        let request = || {
            let mut request = client.get(&media.url);
            for (name, value) in &media.headers {
                if !name.eq_ignore_ascii_case("accept-encoding") {
                    request = request.header(name.as_str(), value);
                }
            }
            request.header("Accept-Encoding", "identity")
        };
        // A first-byte check misses preview-only URLs. Check the final byte.
        let last = media.size.filter(|s| *s > 0).map(|s| s - 1);
        let mut offset = last.unwrap_or(0);
        loop {
            let mut response = request()
                .header("Range", format!("bytes={offset}-{offset}"))
                .send()
                .await
                .context("媒体可读性检查请求失败")?;
            check_source_rate_limit(&media.url, &response)?;
            ensure!(
                response.status().is_success(),
                "媒体服务器拒绝读取（HTTP {}）",
                response.status().as_u16()
            );
            ensure_media_response(response.headers())?;
            if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
                let range = response
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .context("媒体分段响应缺少 Content-Range")?;
                let (bounds, total) = range
                    .strip_prefix("bytes ")
                    .and_then(|v| v.split_once('/'))
                    .context("媒体分段响应格式错误")?;
                let total: u64 = total.parse().context("媒体大小不明确")?;
                ensure!(
                    bounds == format!("{offset}-{offset}") && total > offset,
                    "媒体服务器返回错误的分段范围"
                );
                ensure!(
                    media.size.is_none_or(|size| size == total),
                    "媒体大小与解析结果不一致"
                );
                let chunk = response.chunk().await?.context("媒体分段响应为空")?;
                ensure!(chunk.len() == 1, "媒体服务器返回错误的分段长度");
                if last.is_none() && offset == 0 && total > 1 {
                    offset = total - 1;
                    continue;
                }
            } else {
                // Servers without Range support can still download as one stream.
                ensure!(
                    response.status() == reqwest::StatusCode::OK,
                    "媒体服务器未返回文件内容"
                );
                ensure!(
                    media
                        .size
                        .zip(response.content_length())
                        .is_none_or(|(a, b)| a == b),
                    "媒体大小与解析结果不一致"
                );
            }
            break;
        }
        // The engine falls back to a plain GET if HEAD is rejected. Verify that path too.
        let response = request().send().await?;
        check_source_rate_limit(&media.url, &response)?;
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "媒体服务器拒绝完整下载（HTTP {}）",
            response.status().as_u16()
        );
        ensure_media_response(response.headers())?;
        ensure!(
            media
                .size
                .zip(response.content_length())
                .is_none_or(|(a, b)| a == b),
            "完整媒体大小与解析结果不一致"
        );
        drop(response); // Inspect headers only; do not download the file during extraction.
        let mut passed = self.passed.lock().unwrap();
        passed.retain(|_, t| t.elapsed() < Duration::from_secs(45));
        if passed.len() >= 256 {
            passed.clear();
        }
        passed.insert(key, Instant::now());
        Ok(())
    }
}

fn check_source_rate_limit(url: &str, response: &reqwest::Response) -> Result<()> {
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        && crate::source_policy::limits(url).is_some()
    {
        let _ = crate::source_policy::record_retry_after(
            url,
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
        );
        bail!("Wikimedia 限制了请求（HTTP 429），请等待服务器冷却后重新解析");
    }
    Ok(())
}

fn ensure_media_response(headers: &reqwest::header::HeaderMap) -> Result<()> {
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    ensure!(
        !content_type.starts_with("text/")
            && !content_type.contains("json")
            && !content_type.contains("xml")
            && !content_type.contains("mpegurl"),
        "媒体地址返回了网页、错误响应或播放清单，请重新解析可用格式"
    );
    Ok(())
}

struct CheckedTransport {
    inner: Arc<dyn HttpClient>,
    probe: MediaProbe,
    best: Mutex<Option<(u64, HttpResponse)>>,
    authorization: Option<YoutubeSession>,
}

impl CheckedTransport {
    fn better_response(&self, height: u64, response: HttpResponse) -> HttpResponse {
        let mut best = self.best.lock().unwrap();
        if best.as_ref().is_none_or(|(old, _)| height > *old) {
            *best = Some((height, response));
        }
        best.as_ref().unwrap().1.clone()
    }

    fn fallback(&self) -> Option<HttpResponse> {
        self.best.lock().unwrap().as_ref().map(|(_, r)| r.clone())
    }
}

#[async_trait::async_trait]
impl HttpClient for CheckedTransport {
    async fn execute(&self, mut request: HttpRequest) -> ytdown::Result<HttpResponse> {
        let player =
            reqwest::Url::parse(&request.url).is_ok_and(|u| u.path() == "/youtubei/v1/player");
        let mut request_body: Value = request
            .body
            .as_deref()
            .and_then(|v| serde_json::from_slice(v).ok())
            .unwrap_or_default();
        // Determine the final extractor slot before replacing a client identity.
        let last_client = player && request_body["context"]["client"]["clientName"] == "ANDROID";
        if player {
            if self.authorization.is_some() {
                update_authorized_client(&mut request, &mut request_body);
            } else {
                update_player_client(&mut request, &mut request_body);
            }
        } else if self.authorization.is_some() && request_body["context"]["client"].is_object() {
            update_authorized_client(&mut request, &mut request_body);
        }
        if player {
            if let Some(visitor) = request_body["context"]["client"]["visitorData"].as_str() {
                request
                    .headers
                    .retain(|(key, _)| !key.eq_ignore_ascii_case("X-Goog-Visitor-Id"));
                request
                    .headers
                    .push(("X-Goog-Visitor-Id".into(), visitor.into()));
            }
        }
        if let Some(session) = &self.authorization {
            session.apply(&mut request);
        }
        // ANDROID is the final client in ytdown 0.8.0. Preserve working lower-quality
        // responses while later clients are checked for a better readable format.
        let mut response = match self.inner.execute(request).await {
            Ok(response) => response,
            Err(error) => {
                let error = if let Some(session) = &self.authorization {
                    ytdown::Error::Network {
                        stage: "youtube-session",
                        message: session.redact(&error.to_string()),
                    }
                } else {
                    error
                };
                return if last_client {
                    self.fallback().ok_or(error)
                } else {
                    Err(error)
                };
            }
        };
        if self.authorization.is_some() && !response.is_success() {
            if last_client {
                if let Some(response) = self.fallback() {
                    return Ok(response);
                }
            }
            return Err(ytdown::Error::Network {
                stage: "youtube-session",
                message: format!(
                    "YouTube 授权请求被拒绝（HTTP {}）；授权请求不跟随重定向",
                    response.status
                ),
            });
        }
        if !player || !response.is_success() {
            return Ok(if last_client {
                self.fallback().unwrap_or(response)
            } else {
                response
            });
        }
        let Ok(mut body) = serde_json::from_slice::<Value>(&response.body) else {
            return Ok(response);
        };
        if body["playabilityStatus"]["status"] != "OK"
            || body["videoDetails"]["isLiveContent"] == true
        {
            if body["playabilityStatus"]["status"] != "OK" {
                // Some rejected clients return a partial videoDetails object.
                // Remove it so ytdown can classify the actual playability error
                // instead of failing JSON deserialization on a missing title.
                if let Some(object) = body.as_object_mut() {
                    object.remove("videoDetails");
                }
                if let Some(session) = &self.authorization {
                    if let Some(reason) = body["playabilityStatus"]["reason"].as_str() {
                        body["playabilityStatus"]["reason"] = session.redact(reason).into();
                    }
                    if let Some(message) = body["error"]["message"].as_str() {
                        body["error"]["message"] = session.redact(message).into();
                    }
                }
                response.body = serde_json::to_vec(&body).expect("JSON value is serializable");
            }
            return Ok(if last_client {
                self.fallback().unwrap_or(response)
            } else {
                response
            });
        }
        let mut usable = 0;
        let mut rejected = 0;
        let mut best_height = 0;
        for name in ["formats", "adaptiveFormats"] {
            let Some(formats) = body
                .get_mut("streamingData")
                .and_then(|data| data.get_mut(name))
                .and_then(Value::as_array_mut)
            else {
                continue;
            };
            let candidates = std::mem::take(formats);
            let offered = candidates.len();
            let results: Vec<_> =
                stream::iter(candidates.into_iter().take(96).map(|format| async move {
                    let Some(url) = format["url"].as_str() else {
                        // Cipher URLs must be deciphered by ytdown first; selected plans are checked again.
                        return format.get("signatureCipher").is_some().then_some(format);
                    };
                    let size = format["contentLength"]
                        .as_str()
                        .and_then(|v| v.parse().ok());
                    let media = MediaStream {
                        transform: None,
                        url: url.into(),
                        headers: vec![],
                        size,
                        decrypt_key: None,
                    };
                    self.probe.check(&media).await.is_ok().then_some(format)
                }))
                .buffered(4)
                .collect()
                .await;
            *formats = results.into_iter().flatten().collect();
            usable += formats.len();
            rejected += offered - formats.len();
            best_height = best_height.max(
                formats
                    .iter()
                    .filter_map(|v| v["height"].as_u64())
                    .max()
                    .unwrap_or(0),
            );
        }
        if usable == 0 {
            if last_client {
                if let Some(response) = self.fallback() {
                    return Ok(response);
                }
            }
            // ytdown tries its next built-in client on transport errors as well as API errors.
            return Err(ytdown::Error::Extraction {
                stage: "media-readability",
                message: "此客户端只返回了无法完整下载的媒体地址".into(),
            });
        }
        response.body = serde_json::to_vec(&body).map_err(|e| ytdown::Error::Extraction {
            stage: "media-readability",
            message: e.to_string(),
        })?;
        if last_client {
            return Ok(self.better_response(best_height, response));
        }
        if rejected > 0 && !has_readable_hd_with_audio(&body) {
            self.better_response(best_height, response);
            return Err(ytdown::Error::Extraction {
                stage: "media-readability",
                message: "部分清晰度不可下载，继续检查其他客户端".into(),
            });
        }
        Ok(self.better_response(best_height, response))
    }
}

pub(crate) struct YoutubeResolver {
    extractor: ytdown::extractor::youtube::YoutubeExtractor,
    network: Box<dyn Fn(bool) -> Result<YoutubeNetwork> + Send + Sync>,
    resolve_lock: tokio::sync::Mutex<()>,
}

struct YoutubeNetwork {
    client: reqwest_media::Client,
    probe: MediaProbe,
}

impl YoutubeNetwork {
    fn new(authorized: bool) -> Result<Self> {
        // reqwest snapshots the system proxy when a Client is built. Refresh
        // both API and CDN clients per extraction, including anonymous retries
        // after v2rayN (or another system proxy) has been enabled or changed.
        let mut client = reqwest_media::Client::builder()
            .use_native_tls()
            .https_only(true)
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15))
            .user_agent(auth::WEB_UA);
        if authorized {
            client = client.redirect(reqwest_media::redirect::Policy::none());
        }
        Ok(Self {
            client: client.build()?,
            probe: MediaProbe::new()?,
        })
    }
}

pub(crate) struct YoutubeResolution {
    pub info: MediaInfo,
    // Keep this extraction's network and readability cache for the normalized
    // format checks too; the application's startup probe may use an old proxy.
    pub probe: MediaProbe,
}

impl YoutubeResolver {
    pub fn new() -> Self {
        Self {
            extractor: ytdown::extractor::youtube::YoutubeExtractor::new(),
            network: Box::new(YoutubeNetwork::new),
            resolve_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(client: reqwest_media::Client, probe: MediaProbe, base: &str) -> Self {
        let mut resolver = Self::new();
        resolver.extractor = ytdown::extractor::youtube::YoutubeExtractor::with_base_url(base);
        resolver.network = Box::new(move |_| {
            Ok(YoutubeNetwork {
                client: client.clone(),
                probe: probe.clone(),
            })
        });
        resolver
    }

    #[cfg(test)]
    pub async fn resolve(&self, url: &reqwest::Url) -> Result<MediaInfo> {
        Ok(self.resolve_with_cookie(url, None).await?.info)
    }

    pub async fn resolve_with_cookie(
        &self,
        url: &reqwest::Url,
        cookie: Option<&str>,
    ) -> Result<YoutubeResolution> {
        let session = cookie
            .filter(|value| !value.trim().is_empty())
            .map(YoutubeSession::new)
            .transpose()?;
        let _guard = self.resolve_lock.lock().await;
        let network = (self.network)(session.is_some())?;
        let transport = Arc::new(CheckedTransport {
            inner: Arc::new(ytdown::ReqwestClient::new(network.client)),
            probe: network.probe.clone(),
            best: Mutex::new(None),
            authorization: session,
        });
        let context = ExtractorContext::new(transport);
        // The context, checked responses and authorization all drop on completion
        // or cancellation. Only normalized media URLs leave this request.
        let info = self.extractor.extract(&context, url).await?;
        Ok(YoutubeResolution {
            info,
            probe: network.probe,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::{Request, State},
        response::{IntoResponse, Response},
        Json, Router,
    };
    use serde_json::json;

    #[derive(Clone, Default)]
    struct Fixture {
        clients: Arc<Mutex<Vec<String>>>,
        hd_available: bool,
    }

    async fn fixture(State(state): State<Fixture>, request: Request) -> Response {
        let base = format!("http://{}", request.headers()["host"].to_str().unwrap());
        let path = request.uri().path().to_owned();
        if path.starts_with("/cdn/") {
            assert!(!request.headers().contains_key("cookie"));
            assert!(!request.headers().contains_key("authorization"));
            let range = request.headers().get("range").and_then(|v| v.to_str().ok());
            if let Some(range) = range {
                if path == "/cdn/preview-only" && range != "bytes=0-0" {
                    return axum::http::StatusCode::FORBIDDEN.into_response();
                }
                let bounds = range.strip_prefix("bytes=").unwrap();
                return axum::http::Response::builder()
                    .status(206)
                    .header("content-range", format!("bytes {bounds}/16"))
                    .body(Body::from("x"))
                    .unwrap();
            }
            if path == "/cdn/range-only" {
                return axum::http::StatusCode::FORBIDDEN.into_response();
            }
            return Body::from("0123456789abcdef").into_response();
        }
        if path != "/youtubei/v1/player" {
            return axum::http::StatusCode::NOT_FOUND.into_response();
        }
        let headers = request.headers().clone();
        let body: Value =
            serde_json::from_slice(&to_bytes(request.into_body(), 16384).await.unwrap()).unwrap();
        let client = body["context"]["client"]["clientName"].as_str().unwrap();
        state.clients.lock().unwrap().push(client.into());
        if client == "VISIONOS" && state.hd_available {
            // The platform rejects mixed JSON/header identities. This exercises
            // the actual ytdown extractor through our compatibility transport.
            assert_eq!(headers["x-youtube-client-name"], "101");
            assert_eq!(headers["x-youtube-client-version"], "1.02");
            assert_eq!(headers["user-agent"], VISIONOS_UA);
            assert_eq!(body["context"]["client"]["userAgent"], VISIONOS_UA);
            assert_eq!(
                body["context"]["client"]["deviceModel"],
                "RealityDevice17,1"
            );
            assert!(body["context"]["client"].get("androidSdkVersion").is_none());
            return Json(json!({
                "playabilityStatus":{"status":"OK"},
                "videoDetails":{"videoId":"abcdefghijk","title":"HD regression","lengthSeconds":"3","isLiveContent":false},
                "streamingData":{"adaptiveFormats":[
                    {"itag":400,"url":format!("{base}/cdn/complete"),"mimeType":"video/mp4; codecs=\"av01.0.12M.08\"","width":2560,"height":1440,"contentLength":"16"},
                    {"itag":140,"url":format!("{base}/cdn/complete"),"mimeType":"audio/mp4; codecs=\"mp4a.40.2\"","audioQuality":"AUDIO_QUALITY_MEDIUM","contentLength":"16"},
                    {"itag":401,"url":format!("{base}/cdn/preview-only"),"mimeType":"video/mp4; codecs=\"av01.0.12M.08\"","width":3840,"height":2160,"contentLength":"16"}
                ]}
            })).into_response();
        }
        if matches!(client, "VISIONOS" | "ANDROID_VR" | "TVHTML5") {
            return Json(
                json!({"playabilityStatus":{"status":"UNPLAYABLE","reason":"client unavailable"}}),
            )
            .into_response();
        }
        let bad = json!({"itag":137,"url":format!("{base}/cdn/preview-only"),"mimeType":"video/mp4; codecs=\"avc1.640028\"","width":1920,"height":1080,"contentLength":"16"});
        let mut formats = vec![];
        if client == "IOS" {
            formats.push(json!({"itag":17,"url":format!("{base}/cdn/complete"),"mimeType":"video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"","width":256,"height":144,"audioQuality":"AUDIO_QUALITY_MEDIUM","contentLength":"16"}));
        }
        if client == "ANDROID" {
            formats.push(json!({"itag":18,"url":format!("{base}/cdn/complete"),"mimeType":"video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"","width":640,"height":360,"audioQuality":"AUDIO_QUALITY_MEDIUM","contentLength":"16"}));
        }
        Json(json!({
            "playabilityStatus":{"status":"OK"},
            "videoDetails":{"videoId":"abcdefghijk","title":"Fallback regression","lengthSeconds":"3","isLiveContent":false},
            "streamingData":{"formats":formats,"adaptiveFormats":[bad]}
        })).into_response()
    }

    async fn start(
        hd_available: bool,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let clients = Arc::new(Mutex::new(vec![]));
        let app = Router::new().fallback(fixture).with_state(Fixture {
            clients: clients.clone(),
            hd_available,
        });
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, clients, server)
    }

    #[tokio::test]
    async fn playable_metadata_with_preview_only_bytes_falls_back_to_a_working_client() {
        let (base, clients, server) = start(false).await;
        let resolver = YoutubeResolver::with_base_url(
            reqwest_media::Client::new(),
            MediaProbe::new().unwrap(),
            &base,
        );
        let result = resolver
            .resolve(&reqwest::Url::parse("https://youtube.com/watch?v=abcdefghijk").unwrap())
            .await
            .unwrap();
        let MediaInfo::Single(video) = result else {
            panic!("expected single video")
        };
        assert_eq!(video.formats.len(), 1);
        assert_eq!(video.formats[0].itag, Some(18));
        assert!(video.formats[0].video.is_some() && video.formats[0].audio.is_some());
        assert_eq!(
            *clients.lock().unwrap(),
            ["VISIONOS", "IOS", "TVHTML5", "ANDROID"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn probe_rejects_urls_that_only_allow_ranges_and_handles_unknown_lengths() {
        let (base, _, server) = start(false).await;
        let probe = MediaProbe::new().unwrap();
        let mut media = MediaStream {
            transform: None,
            url: format!("{base}/cdn/range-only"),
            headers: vec![],
            size: Some(16),
            decrypt_key: None,
        };
        assert!(probe
            .check(&media)
            .await
            .unwrap_err()
            .to_string()
            .contains("403"));
        media.url = format!("{base}/cdn/preview-only");
        media.size = None;
        assert!(probe.check(&media).await.is_err());
        media.url = format!("{base}/cdn/complete");
        probe.check(&media).await.unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn current_client_recovers_hd_and_audio_without_downgrading() {
        let (base, clients, server) = start(true).await;
        let resolver = YoutubeResolver::with_base_url(
            reqwest_media::Client::new(),
            MediaProbe::new().unwrap(),
            &base,
        );
        let result = resolver
            .resolve(&reqwest::Url::parse("https://youtube.com/watch?v=abcdefghijk").unwrap())
            .await
            .unwrap();
        let MediaInfo::Single(video) = result else {
            panic!("expected single video")
        };
        let hd = video.formats.iter().find(|f| f.itag == Some(400)).unwrap();
        assert_eq!(hd.video.as_ref().unwrap().height, Some(1440));
        assert!(video
            .formats
            .iter()
            .any(|f| f.itag == Some(140) && f.audio.is_some()));
        assert_eq!(*clients.lock().unwrap(), ["VISIONOS"]);
        assert!(!video.formats.iter().any(|f| f.itag == Some(401)), "an unreadable extra format must not discard working HD or force another login-gated client");
        server.abort();
    }

    #[tokio::test]
    async fn subsequent_extraction_refreshes_both_api_and_cdn_proxy_clients() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Both proxies serve the same public origin and media URLs. Changing
        // route must refresh the API client, CDN probe and readability cache.
        let (proxy_a, clients_a, server_a) = start(true).await;
        let (proxy_b, clients_b, server_b) = start(true).await;
        let builds = Arc::new(AtomicUsize::new(0));
        let factory_builds = builds.clone();
        let mut resolver = YoutubeResolver::new();
        resolver.extractor = ytdown::extractor::youtube::YoutubeExtractor::with_base_url(
            "http://youtube.example.test",
        );
        resolver.network = Box::new(move |authorized| {
            assert!(!authorized);
            let proxy = if factory_builds.fetch_add(1, Ordering::SeqCst) == 0 {
                &proxy_a
            } else {
                &proxy_b
            };
            let client = reqwest_media::Client::builder()
                .no_proxy()
                .proxy(reqwest_media::Proxy::all(proxy)?)
                .build()?;
            let mut probe = MediaProbe::new()?;
            probe.client = reqwest::Client::builder()
                .no_proxy()
                .proxy(reqwest::Proxy::all(proxy)?)
                .build()?;
            Ok(YoutubeNetwork { client, probe })
        });
        let url = reqwest::Url::parse("https://www.youtube.com/watch?v=abcdefghijk").unwrap();
        for attempt in 0..2 {
            let result = resolver.resolve_with_cookie(&url, None).await.unwrap();
            let MediaInfo::Single(video) = result.info else {
                panic!("expected single video")
            };
            let hd = video.formats.iter().find(|f| f.itag == Some(400)).unwrap();
            if attempt == 0 {
                // The next extraction must work after the previous proxy has
                // gone away, including checks made after normalization.
                server_a.abort();
            } else {
                result.probe.passed.lock().unwrap().clear();
                result
                    .probe
                    .check(&MediaStream {
                        transform: None,
                        url: hd.url.clone(),
                        headers: vec![],
                        size: hd.filesize,
                        decrypt_key: None,
                    })
                    .await
                    .unwrap();
            }
        }
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert_eq!(*clients_a.lock().unwrap(), ["VISIONOS"]);
        assert_eq!(*clients_b.lock().unwrap(), ["VISIONOS"]);
        server_b.abort();
    }

    struct SessionApi {
        cdn: String,
        clients: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl HttpClient for SessionApi {
        async fn execute(&self, request: HttpRequest) -> ytdown::Result<HttpResponse> {
            let path = reqwest::Url::parse(&request.url).unwrap().path().to_owned();
            let value = if path == "/youtubei/v1/visitor_id" {
                json!({"responseContext":{"visitorData":"test-visitor"}})
            } else if path == "/youtubei/v1/player" {
                let body: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
                let client = body["context"]["client"]["clientName"].as_str().unwrap();
                self.clients.lock().unwrap().push(client.into());
                if request
                    .headers
                    .iter()
                    .any(|(k, v)| k == "Cookie" && v.contains("test-session-secret"))
                {
                    assert!(matches!(client, "TVHTML5" | "WEB"));
                    assert!(request
                        .headers
                        .iter()
                        .any(|(k, v)| k == "Authorization" && v.starts_with("SAPISIDHASH ")));
                    assert_eq!(body["context"]["client"]["visitorData"], "test-visitor");
                    assert!(body["context"]["client"].get("androidSdkVersion").is_none());
                    json!({"playabilityStatus":{"status":"OK"},
                        "videoDetails":{"videoId":"abcdefghijk","title":"Authorized video","lengthSeconds":"3"},
                        "streamingData":{"formats":[{"itag":18,"url":format!("{}/cdn/complete", self.cdn),
                            "mimeType":"video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"","width":1280,"height":720,
                            "audioQuality":"AUDIO_QUALITY_MEDIUM","contentLength":"16"}]}})
                } else {
                    // A missing title must not hide the real login requirement.
                    json!({"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in to confirm you're not a bot"},
                        "videoDetails":{"videoId":"abcdefghijk"}})
                }
            } else {
                assert!(!request
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("cookie")
                        || k.eq_ignore_ascii_case("authorization")));
                return Ok(HttpResponse {
                    status: 404,
                    body: vec![],
                    headers: vec![],
                    final_url: None,
                });
            };
            Ok(HttpResponse {
                status: 200,
                body: serde_json::to_vec(&value).unwrap(),
                headers: vec![],
                final_url: None,
            })
        }
    }

    #[tokio::test]
    async fn authenticated_extraction_uses_session_clients_and_keeps_credentials_out_of_media() {
        let (base, _, server) = start(false).await;
        let api = Arc::new(SessionApi {
            cdn: base,
            clients: Mutex::new(vec![]),
        });
        let extractor = ytdown::extractor::youtube::YoutubeExtractor::new();
        let url = reqwest::Url::parse("https://www.youtube.com/watch?v=abcdefghijk").unwrap();
        for authenticated in [false, true, false] {
            let transport = Arc::new(CheckedTransport {
                inner: api.clone(),
                probe: MediaProbe::new().unwrap(),
                best: Mutex::new(None),
                authorization: authenticated.then(|| {
                    YoutubeSession::new("SAPISID=test-session-secret; LOGIN_INFO=test-login-secret")
                        .unwrap()
                }),
            });
            let weak = Arc::downgrade(&transport);
            let context = ExtractorContext::new(transport);
            let result = extractor.extract(&context, &url).await;
            if authenticated {
                let MediaInfo::Single(video) = result.unwrap() else {
                    panic!("single video expected")
                };
                assert_eq!(video.formats.len(), 1);
                let description = format!("{video:?}");
                assert!(!description.contains("test-session-secret"));
                assert!(!description.contains("test-login-secret"));
                assert_eq!(api.clients.lock().unwrap().last().unwrap(), "TVHTML5");
            } else {
                assert!(matches!(
                    result,
                    Err(ytdown::Error::Unavailable {
                        reason: ytdown::error::UnavailableReason::BotCheck,
                        ..
                    })
                ));
            }
            drop(context);
            assert!(
                weak.upgrade().is_none(),
                "no session or checked response survives extraction"
            );
        }
        server.abort();
    }

    #[test]
    fn client_update_preserves_session_and_does_not_touch_other_clients() {
        let mut body = json!({"videoId":"test", "context":{"client":{
            "clientName":"ANDROID_VR", "visitorData":"test-session", "hl":"zh", "gl":"TW",
            "androidSdkVersion":32
        }}});
        let mut request = HttpRequest::post("innertube", "https://youtube.com/youtubei/v1/player")
            .header("user-agent", "old")
            .header("x-youtube-client-name", "28");
        update_player_client(&mut request, &mut body);
        assert_eq!(body["context"]["client"]["visitorData"], "test-session");
        assert_eq!(body["context"]["client"]["hl"], "zh");
        assert_eq!(body["context"]["client"]["gl"], "TW");
        assert_eq!(body["videoId"], "test");
        assert_eq!(
            request
                .headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
                .count(),
            1
        );
        let before = request.clone();
        body["context"]["client"]["clientName"] = "IOS".into();
        update_player_client(&mut request, &mut body);
        assert_eq!(request.headers, before.headers);
        assert_eq!(request.body, before.body);
    }
}
