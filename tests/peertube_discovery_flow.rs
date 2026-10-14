//! Arbitrary PeerTube instances are discovered only after a URL is identified
//! as HTML. A binary using the same /w/UUID path stays an ordinary direct file.
use axum::{
    body::Body,
    http::{header, HeaderMap, Method, Response, StatusCode},
    response::Html,
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use ffdownload::resolver::{Assembly, ResourceResolver};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{net::TcpListener, task::JoinHandle};

const VIDEO_ID: &str = "9c9de5e8-0a1e-484a-b099-e80766180a6d";
const BINARY_ID: &str = "cf48aadd-3917-4eae-96a8-c37a9a1933d8";

struct Fixture {
    base: String,
    api_requests: Arc<AtomicUsize>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let api_requests = Arc::new(AtomicUsize::new(0));
        let requests = api_requests.clone();
        let api_base = base.clone();
        let app = Router::new()
            .route(
                &format!("/w/{VIDEO_ID}"),
                get(|| async { Html("<!doctype html><html><title>PeerTube test instance</title><body>Public video</body></html>") }),
            )
            .route(
                &format!("/api/v1/videos/{VIDEO_ID}"),
                get(move || {
                    let requests = requests.clone();
                    let api_base = api_base.clone();
                    async move {
                        requests.fetch_add(1, Ordering::Relaxed);
                        Json(json!({
                            "uuid": VIDEO_ID,
                            "shortUUID": "kkGMgK9ZtnKfYAgnEtQxbv",
                            "name": "Public local PeerTube clip",
                            "isLive": false,
                            "privacy": {"id": 1},
                            "duration": 1,
                            "files": [{
                                "fileUrl": format!("{api_base}/media/video.mp4?signature=keep-private"),
                                "size": 32,
                                "resolution": {"id": 720}
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/media/video.mp4",
                get(|method: Method, headers: HeaderMap| async move {
                    ranged(
                        method,
                        headers,
                        Bytes::from_static(b"\0\0\0\x18ftypisom\0\0\0\0isommp42\0\0\0\x08free"),
                        "video/mp4",
                    )
                }),
            )
            .route(
                &format!("/w/{BINARY_ID}"),
                get(|method: Method, headers: HeaderMap| async move {
                    let mut response = ranged(
                        method,
                        headers,
                        Bytes::from_static(b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"),
                        "application/zip",
                    );
                    response.headers_mut().insert(
                        header::CONTENT_DISPOSITION,
                        "attachment; filename=\"research.zip\"".parse().unwrap(),
                    );
                    response
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            api_requests,
            server,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn ranged(method: Method, headers: HeaderMap, data: Bytes, mime: &str) -> Response<Body> {
    let length = data.len();
    let mut start = 0;
    let mut end = length - 1;
    let mut partial = false;
    if let Some(range) = headers.get(header::RANGE) {
        if let Some((from, to)) = range
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("bytes="))
            .and_then(|value| value.split_once('-'))
        {
            start = from.parse().unwrap();
            end = if to.is_empty() {
                length - 1
            } else {
                to.parse().unwrap()
            };
            if start > end || end >= length {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                    .body(Body::empty())
                    .unwrap();
            }
            partial = true;
        }
    }
    let mut builder = Response::builder()
        .status(if partial {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(header::ACCEPT_RANGES, "bytes");
    if partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}"),
        );
    }
    builder
        .body(if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(data.slice(start..=end))
        })
        .unwrap()
}

#[tokio::test]
async fn an_unknown_instance_html_page_is_verified_against_its_own_video_api() {
    let fixture = Fixture::start().await;
    let url = format!("{}/w/{VIDEO_ID}", fixture.base);
    let resource = ResourceResolver::with_ffmpeg(None)
        .unwrap()
        .extract(&url)
        .await
        .unwrap();
    assert_eq!(resource.platform, "PeerTube");
    assert_eq!(resource.source_url, url);
    assert_eq!(resource.formats.len(), 1);
    let format = &resource.formats[0];
    assert_eq!(format.height, 720);
    assert_eq!(format.plan.extension, "mp4");
    assert_eq!(format.plan.assembly, Assembly::Direct);
    assert!(format.plan.streams[0]
        .url
        .ends_with("?signature=keep-private"));
    assert_eq!(fixture.api_requests.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn binary_at_a_peertube_shaped_path_stays_a_direct_file_without_api_requests() {
    let fixture = Fixture::start().await;
    let url = format!("{}/w/{BINARY_ID}", fixture.base);
    let resource = ResourceResolver::with_ffmpeg(None)
        .unwrap()
        .extract(&url)
        .await
        .unwrap();
    assert_eq!(resource.platform, "Direct File");
    assert_eq!(resource.source_url, url);
    assert_eq!(resource.formats.len(), 1);
    assert_eq!(resource.formats[0].plan.extension, "zip");
    assert_eq!(resource.formats[0].plan.assembly, Assembly::Direct);
    assert_eq!(resource.formats[0].plan.streams[0].url, url);
    assert_eq!(fixture.api_requests.load(Ordering::Relaxed), 0);
}
