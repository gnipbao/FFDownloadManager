//! Public Douyin extraction with per-request networking and bounded fallbacks.
use crate::chinese_video::{parse_douyin, parse_douyin_data, parse_douyin_feed, NativeVideo};
use anyhow::{anyhow, bail, ensure, Context, Result};
use futures_util::{stream::FuturesUnordered, StreamExt};
use reqwest::Url;
use std::time::Duration;

const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MOBILE_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
const FEED_UA: &str = "com.ss.android.ugc.aweme/370000 (Linux; U; Android 14; zh_CN)";
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct DouyinResolver {
    network: Box<dyn Fn() -> Result<Network> + Send + Sync>,
    #[cfg(test)]
    allow_browser: bool,
}

impl DouyinResolver {
    pub fn new() -> Self {
        Self {
            network: Box::new(Network::new),
            #[cfg(test)]
            allow_browser: true,
        }
    }

    pub async fn resolve(&self, input: &Url) -> Result<NativeVideo> {
        crate::validation::parse_url(input.as_str())?;
        ensure!(page_host(input), "请输入抖音的单个视频或分享链接");
        // reqwest snapshots system proxy settings when the client is built.
        // Neither a previous extraction nor application startup owns this session.
        let network = (self.network)()?;
        let id = tokio::time::timeout(Duration::from_secs(11), network.video_id(input))
            .await
            .context("抖音短链接解析超时，请检查网络或重新复制分享链接")??;
        let http_error =
            match tokio::time::timeout(Duration::from_secs(16), network.resolve_http(&id)).await {
                Ok(Ok(video)) => return Ok(video),
                Ok(Err(error)) => error.to_string(),
                Err(_) => "公开接口与分享页请求超时".into(),
            };
        #[cfg(test)]
        if !self.allow_browser {
            bail!("{http_error}");
        }
        match crate::douyin_browser::resolve(&id).await {
            Ok(video) => Ok(video),
            Err(error) => bail!(
                "未能读取这条抖音视频。{http_error}；网页备用解析：{error:#}。请确认原链接在同一网络下可以正常播放后重试"
            ),
        }
    }
}

#[derive(Clone, Copy)]
enum Backend {
    Native,
    Portable,
}

#[derive(Clone, Copy)]
enum Kind {
    Feed,
    Share,
    Detail,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Feed => "公开接口",
            Self::Share => "分享页",
            Self::Detail => "网页详情",
        }
    }

    fn agent(self) -> &'static str {
        match self {
            Self::Feed => FEED_UA,
            Self::Share => MOBILE_UA,
            Self::Detail => WEB_UA,
        }
    }
}

struct Endpoints {
    feeds: [Url; 2],
    share: Url,
    detail: Url,
}

impl Endpoints {
    fn public() -> Result<Self> {
        Ok(Self {
            feeds: [
                Url::parse("https://api5-normal-c-hl.amemv.com/aweme/v1/feed/")?,
                Url::parse("https://aweme.snssdk.com/aweme/v1/feed/")?,
            ],
            share: Url::parse("https://www.iesdouyin.com/share/video/")?,
            detail: Url::parse("https://www.douyin.com/aweme/v1/web/aweme/detail/")?,
        })
    }

    fn source(&self, kind: Kind, id: &str, feed: usize) -> Result<Url> {
        let mut url = match kind {
            Kind::Feed => self.feeds[feed].clone(),
            Kind::Share => return Ok(self.share.join(&format!("{id}/"))?),
            Kind::Detail => self.detail.clone(),
        };
        url.query_pairs_mut().append_pair("aweme_id", id);
        if matches!(kind, Kind::Feed) {
            url.query_pairs_mut().append_pair("aid", "1128");
        }
        Ok(url)
    }
}

struct Network {
    native: reqwest_media::Client,
    portable: reqwest::Client,
    endpoints: Endpoints,
}

impl Network {
    fn new() -> Result<Self> {
        let native = reqwest_media::Client::builder()
            .use_native_tls()
            .https_only(true)
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest_media::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() < 5 && allowed_redirect(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.error("抖音链接跳转到了不受支持的地址")
                }
            }))
            .build()?;
        // Keep the established native TLS path, then try the same Rust TLS
        // implementation on macOS and Windows when its public responses fail.
        let portable = reqwest::Client::builder()
            .tls_backend_rustls()
            .https_only(true)
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() < 5 && allowed_redirect(attempt.url()) {
                    attempt.follow()
                } else {
                    attempt.error("抖音链接跳转到了不受支持的地址")
                }
            }))
            .build()?;
        Ok(Self {
            native,
            portable,
            endpoints: Endpoints::public()?,
        })
    }

    async fn video_id(&self, input: &Url) -> Result<String> {
        if let Some(id) = video_id(input) {
            return Ok(id);
        }
        ensure!(
            input.host_str() == Some("v.douyin.com"),
            "抖音链接缺少视频作品编号"
        );
        let mut url = input.clone();
        url.set_scheme("https")
            .map_err(|_| anyhow!("抖音短链接无效"))?;
        let mut reasons = Vec::new();
        for backend in [Backend::Native, Backend::Portable] {
            match self.fetch(backend, &url, MOBILE_UA, "text/html").await {
                Ok(response) => {
                    if let Some(id) = video_id(&response.url) {
                        return Ok(id);
                    }
                    reasons.push("分享链接没有返回视频作品编号".into());
                }
                Err(error) => reasons.push(error.to_string()),
            }
        }
        reasons.dedup();
        bail!("抖音短链接解析失败：{}", reasons.join("；"))
    }

    async fn resolve_http(&self, id: &str) -> Result<NativeVideo> {
        let primary = [
            (Backend::Native, Kind::Feed, 0),
            (Backend::Native, Kind::Feed, 1),
        ];
        let fallback = [
            (Backend::Native, Kind::Share, 0),
            (Backend::Portable, Kind::Feed, 0),
            (Backend::Portable, Kind::Share, 0),
            (Backend::Portable, Kind::Detail, 0),
        ];
        let mut failures = Vec::new();
        for attempts in [&primary[..], &fallback[..]] {
            let mut pending = FuturesUnordered::new();
            for &(backend, kind, feed) in attempts {
                pending.push(async move { (kind, self.try_source(backend, kind, feed, id).await) });
            }
            while let Some((kind, result)) = pending.next().await {
                match result {
                    Ok(video) => return Ok(video),
                    Err(error) => {
                        let reason = format!("{}：{error}", kind.label());
                        if !failures.contains(&reason) {
                            failures.push(reason);
                        }
                    }
                }
            }
        }
        // No raw response bodies, signed URLs or session values reach the UI.
        bail!(
            "{}",
            failures.into_iter().take(4).collect::<Vec<_>>().join("；")
        )
    }

    async fn try_source(
        &self,
        backend: Backend,
        kind: Kind,
        feed: usize,
        id: &str,
    ) -> Result<NativeVideo> {
        let url = self.endpoints.source(kind, id, feed)?;
        let accept = if matches!(kind, Kind::Share) {
            "text/html"
        } else {
            "application/json"
        };
        let response = self.fetch(backend, &url, kind.agent(), accept).await?;
        match kind {
            Kind::Share => parse_douyin(&String::from_utf8_lossy(&response.body), id)
                .map_err(|_| anyhow!("页面未提供目标作品数据")),
            Kind::Feed | Kind::Detail => {
                let data: serde_json::Value = serde_json::from_slice(&response.body)
                    .map_err(|_| anyhow!("未返回有效的作品数据"))?;
                match kind {
                    Kind::Feed => parse_douyin_feed(&data, id),
                    _ => parse_douyin_data(&data, id),
                }
                .map_err(|_| anyhow!("接口未返回目标作品"))
            }
        }
    }

    async fn fetch(&self, backend: Backend, url: &Url, agent: &str, accept: &str) -> Result<Page> {
        let mut response = match backend {
            Backend::Native => Response::Native(
                self.native
                    .get(url.clone())
                    .header("User-Agent", agent)
                    .header("Accept", accept)
                    .header("Accept-Encoding", "identity")
                    .header("Referer", "https://www.douyin.com/")
                    .send()
                    .await
                    .map_err(|error| network_error(error.is_timeout(), error.is_connect()))?,
            ),
            Backend::Portable => Response::Portable(
                self.portable
                    .get(url.clone())
                    .header("User-Agent", agent)
                    .header("Accept", accept)
                    .header("Accept-Encoding", "identity")
                    .header("Referer", "https://www.douyin.com/")
                    .send()
                    .await
                    .map_err(|error| network_error(error.is_timeout(), error.is_connect()))?,
            ),
        };
        let (url, status, size) = match &response {
            Response::Native(r) => (r.url().clone(), r.status().as_u16(), r.content_length()),
            Response::Portable(r) => (r.url().clone(), r.status().as_u16(), r.content_length()),
        };
        ensure!((200..300).contains(&status), "HTTP {status}");
        ensure!(
            size.unwrap_or(0) <= MAX_BODY_BYTES as u64,
            "作品数据超过读取上限"
        );
        let mut body = Vec::new();
        loop {
            let chunk = match &mut response {
                Response::Native(r) => r
                    .chunk()
                    .await
                    .map(|chunk| chunk.map(|b| b.to_vec()))
                    .map_err(|error| network_error(error.is_timeout(), error.is_connect()))?,
                Response::Portable(r) => r
                    .chunk()
                    .await
                    .map(|chunk| chunk.map(|b| b.to_vec()))
                    .map_err(|error| network_error(error.is_timeout(), error.is_connect()))?,
            };
            let Some(chunk) = chunk else {
                break;
            };
            ensure!(
                body.len().saturating_add(chunk.len()) <= MAX_BODY_BYTES,
                "作品数据超过读取上限"
            );
            body.extend_from_slice(&chunk);
        }
        Ok(Page { url, body })
    }
}

enum Response {
    Native(reqwest_media::Response),
    Portable(reqwest::Response),
}
struct Page {
    url: Url,
    body: Vec<u8>,
}

fn network_error(timeout: bool, connect: bool) -> anyhow::Error {
    anyhow!(if timeout {
        "请求超时"
    } else if connect {
        "连接失败，请检查网络和系统代理"
    } else {
        "连接或响应读取失败"
    })
}

fn host_matches(url: &Url, domain: &str) -> bool {
    url.host_str()
        .is_some_and(|host| host == domain || host.ends_with(&format!(".{domain}")))
}

fn page_host(url: &Url) -> bool {
    host_matches(url, "douyin.com") || host_matches(url, "iesdouyin.com")
}

fn allowed_redirect(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && (page_host(url) || host_matches(url, "amemv.com") || host_matches(url, "snssdk.com"))
}

fn video_id(url: &Url) -> Option<String> {
    if !page_host(url) {
        return None;
    }
    let valid =
        |id: &str| (12..=24).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit());
    if let Some((_, id)) = url
        .query_pairs()
        .find(|(key, value)| key == "modal_id" && valid(value))
    {
        return Some(id.into_owned());
    }
    let parts: Vec<_> = url.path_segments()?.collect();
    let id = parts
        .windows(2)
        .find(|parts| matches!(parts[0], "video" | "note"))?[1];
    valid(id).then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::{Request, State},
        http::StatusCode,
        response::{IntoResponse, Response},
        Json, Router,
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

    const VIDEO_ID: &str = "1234567890123456789";

    #[derive(Clone, Copy)]
    enum FixtureMode {
        Detail,
        Failure,
    }

    struct MockState {
        mode: FixtureMode,
        title: String,
        requests: Mutex<Vec<String>>,
    }

    struct MockSite {
        base: Url,
        state: Arc<MockState>,
        shutdown: Option<oneshot::Sender<()>>,
        server: Option<JoinHandle<()>>,
    }

    impl MockSite {
        async fn start(mode: FixtureMode, title: &str) -> Self {
            let state = Arc::new(MockState {
                mode,
                title: title.into(),
                requests: Mutex::new(Vec::new()),
            });
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let (shutdown, stopped) = oneshot::channel();
            let app = Router::new()
                .fallback(mock_response)
                .with_state(state.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });
            Self {
                base,
                state,
                shutdown: Some(shutdown),
                server: Some(server),
            }
        }

        async fn stop(mut self) {
            let _ = self.shutdown.take().unwrap().send(());
            self.server.take().unwrap().await.unwrap();
        }
    }

    impl Drop for MockSite {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(server) = self.server.take() {
                server.abort();
            }
        }
    }

    async fn mock_response(State(state): State<Arc<MockState>>, request: Request) -> Response {
        let path = request.uri().path().to_owned();
        state.requests.lock().unwrap().push(path.clone());
        assert!(!request.headers().contains_key("cookie"));
        assert!(!request.headers().contains_key("authorization"));
        assert_eq!(request.headers()["accept-encoding"], "identity");
        if path.starts_with("/share/") {
            return "<script>window._ROUTER_DATA={\"loaderData\":{}};</script>".into_response();
        }
        let query = request.uri().query().unwrap_or_default();
        assert!(query.contains(&format!("aweme_id={VIDEO_ID}")));
        if path.starts_with("/feed-") {
            assert_eq!(request.headers()["user-agent"], FEED_UA);
            return match state.mode {
                FixtureMode::Detail => Json(json!({"aweme_list":[
                    {"aweme_id":"9876543210987654321","video":{"play_addr":{
                        "url_list":["https://cdn.example/wrong-feed.mp4"]}}}
                ]}))
                .into_response(),
                FixtureMode::Failure => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "private-response-marker Authorization=not-a-real-secret",
                )
                    .into_response(),
            };
        }
        assert_eq!(path, "/detail/");
        assert_eq!(request.headers()["user-agent"], WEB_UA);
        match state.mode {
            FixtureMode::Detail => Json(json!({"items":[
                {"video":{"playAddr":{"urlList":["https://cdn.example/anonymous.mp4"]}}},
                {"awemeId":"9876543210987654321","video":{"playAddr":{
                    "urlList":["https://cdn.example/recommendation.mp4"]}}},
                {"awemeId":VIDEO_ID,"desc":state.title,"video":{
                    "duration":9000,"width":1920,"height":1080,
                    "playAddr":{"urlList":["https://cdn.example/target.mp4"]}}}
            ]}))
            .into_response(),
            FixtureMode::Failure => (
                StatusCode::BAD_GATEWAY,
                "private-detail-marker https://cdn.example/file?signature=private",
            )
                .into_response(),
        }
    }

    fn mock_network(base: &Url) -> Result<Network> {
        Ok(Network {
            native: reqwest_media::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .redirect(reqwest_media::redirect::Policy::none())
                .build()?,
            portable: reqwest::Client::builder()
                .tls_backend_rustls()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            endpoints: Endpoints {
                feeds: [base.join("feed-a/")?, base.join("feed-b/")?],
                share: base.join("share/")?,
                detail: base.join("detail/")?,
            },
        })
    }

    #[tokio::test]
    async fn detail_recovers_after_feed_misses_and_empty_share_data_without_picking_recommendations(
    ) {
        let site = MockSite::start(FixtureMode::Detail, "目标作品").await;
        let network = mock_network(&site.base).unwrap();
        let video = network.resolve_http(VIDEO_ID).await.unwrap();
        assert_eq!(video.id, VIDEO_ID);
        assert_eq!(video.title, "目标作品");
        assert_eq!(video.duration_seconds, Some(9));
        assert_eq!(video.variants.len(), 1);
        assert_eq!(video.variants[0].url, "https://cdn.example/target.mp4");
        assert_eq!(video.variants[0].height, 1080);
        let paths = site.state.requests.lock().unwrap().clone();
        assert!(paths.iter().any(|path| path == "/feed-a/"));
        assert!(paths.iter().any(|path| path == "/feed-b/"));
        assert!(paths.iter().any(|path| path == "/detail/"));
        site.stop().await;
    }

    #[tokio::test]
    async fn failed_sources_preserve_http_and_connection_reasons_without_claiming_login() {
        let site = MockSite::start(FixtureMode::Failure, "unused").await;
        let mut network = mock_network(&site.base).unwrap();
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_address = closed.local_addr().unwrap();
        drop(closed);
        network.endpoints.feeds[1] =
            Url::parse(&format!("http://{closed_address}/feed-b/")).unwrap();
        let error = network
            .resolve_http(VIDEO_ID)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 503"), "{error}");
        assert!(error.contains("连接失败"), "{error}");
        assert!(error.contains("页面未提供目标作品数据"), "{error}");
        assert!(!error.contains("登录"), "{error}");
        for forbidden in [
            "private-response-marker",
            "private-detail-marker",
            "signature=",
            "Authorization",
            "https://cdn.example",
            VIDEO_ID,
        ] {
            assert!(
                !error.contains(forbidden),
                "response or source data escaped into error"
            );
        }
        site.stop().await;
    }

    #[tokio::test]
    async fn next_extraction_refreshes_clients_after_the_previous_network_is_gone() {
        let first = MockSite::start(FixtureMode::Detail, "第一条网络").await;
        let second = MockSite::start(FixtureMode::Detail, "更新后的网络").await;
        let first_base = first.base.clone();
        let second_base = second.base.clone();
        let builds = Arc::new(AtomicUsize::new(0));
        let observed_builds = builds.clone();
        let resolver = DouyinResolver {
            network: Box::new(move || {
                let base = if observed_builds.fetch_add(1, Ordering::SeqCst) == 0 {
                    &first_base
                } else {
                    &second_base
                };
                mock_network(base)
            }),
            allow_browser: false,
        };
        let input = Url::parse(&format!("https://www.douyin.com/video/{VIDEO_ID}")).unwrap();
        assert_eq!(resolver.resolve(&input).await.unwrap().title, "第一条网络");
        first.stop().await;
        assert_eq!(
            resolver.resolve(&input).await.unwrap().title,
            "更新后的网络"
        );
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        assert!(!second.state.requests.lock().unwrap().is_empty());
        second.stop().await;
    }

    #[tokio::test]
    #[ignore = "requires network and FFDM_TEST_DOUYIN_URL"]
    async fn portable_rust_tls_resolves_a_public_sample() {
        let raw = std::env::var("FFDM_TEST_DOUYIN_URL").expect("set FFDM_TEST_DOUYIN_URL");
        let input = crate::validation::parse_url(&raw).unwrap();
        assert!(allowed_redirect(&input));
        let network = Network::new().unwrap();
        let id = if let Some(id) = video_id(&input) {
            id
        } else {
            assert_eq!(input.host_str(), Some("v.douyin.com"));
            let response = network
                .fetch(Backend::Portable, &input, MOBILE_UA, "text/html")
                .await
                .unwrap();
            video_id(&response.url).expect("share link did not resolve to a video")
        };
        let mut failures = Vec::new();
        for (kind, feed) in [
            (Kind::Feed, 0),
            (Kind::Feed, 1),
            (Kind::Share, 0),
            (Kind::Detail, 0),
        ] {
            match network.try_source(Backend::Portable, kind, feed, &id).await {
                Ok(video) => {
                    println!(
                        "Douyin portable Rust TLS: title={:?}, formats={}",
                        video.title,
                        video.variants.len()
                    );
                    assert!(!video.variants.is_empty());
                    return;
                }
                Err(error) => failures.push(format!("{}: {error}", kind.label())),
            }
        }
        panic!("portable extraction failed: {}", failures.join("; "));
    }
}
