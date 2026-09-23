//! Maintain YouTube client compatibility and verify media before offering formats.
use crate::media::MediaStream;
use anyhow::{ensure, Context, Result};
use futures_util::{stream, StreamExt};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use ytdown::{Extractor, ExtractorContext, HttpClient, HttpRequest, HttpResponse, MediaInfo};

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

#[derive(Clone)]
pub(crate) struct MediaProbe {
    client: reqwest::Client,
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
        crate::filename::parse_url(&media.url)?;
        crate::download::validate_headers(&media.headers)?;
        let request = || {
            let mut request = self.client.get(&media.url);
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
            ensure!(
                response.status().is_success(),
                "媒体服务器拒绝读取（HTTP {}）",
                response.status().as_u16()
            );
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
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "媒体服务器拒绝完整下载（HTTP {}）",
            response.status().as_u16()
        );
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

struct CheckedTransport {
    inner: Arc<dyn HttpClient>,
    probe: MediaProbe,
    best: Mutex<Option<(u64, HttpResponse)>>,
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
        if player {
            update_player_client(&mut request, &mut request_body);
        }
        // ANDROID is the final client in ytdown 0.8.0. Preserve working lower-quality
        // responses while later clients are checked for a better readable format.
        let last_client = player && request_body["context"]["client"]["clientName"] == "ANDROID";
        let mut response = match self.inner.execute(request).await {
            Ok(response) => response,
            Err(error) => {
                return if last_client {
                    self.fallback().ok_or(error)
                } else {
                    Err(error)
                }
            }
        };
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
                        url: url.into(),
                        headers: vec![],
                        size,
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
        if rejected > 0 {
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
    context: ExtractorContext,
    transport: Arc<CheckedTransport>,
    resolve_lock: tokio::sync::Mutex<()>,
}

impl YoutubeResolver {
    pub fn new(client: reqwest_media::Client, probe: MediaProbe) -> Self {
        let transport = Arc::new(CheckedTransport {
            inner: Arc::new(ytdown::ReqwestClient::new(client)),
            probe,
            best: Mutex::new(None),
        });
        Self {
            extractor: ytdown::extractor::youtube::YoutubeExtractor::new(),
            context: ExtractorContext::new(transport.clone()),
            transport,
            resolve_lock: tokio::sync::Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(client: reqwest_media::Client, probe: MediaProbe, base: &str) -> Self {
        let mut resolver = Self::new(client, probe);
        resolver.extractor = ytdown::extractor::youtube::YoutubeExtractor::with_base_url(base);
        resolver
    }

    pub async fn resolve(&self, url: &reqwest::Url) -> ytdown::Result<MediaInfo> {
        let _guard = self.resolve_lock.lock().await;
        self.transport.best.lock().unwrap().take();
        let result = self.extractor.extract(&self.context, url).await;
        self.transport.best.lock().unwrap().take();
        result
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
                    {"itag":140,"url":format!("{base}/cdn/complete"),"mimeType":"audio/mp4; codecs=\"mp4a.40.2\"","audioQuality":"AUDIO_QUALITY_MEDIUM","contentLength":"16"}
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
            url: format!("{base}/cdn/range-only"),
            headers: vec![],
            size: Some(16),
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
