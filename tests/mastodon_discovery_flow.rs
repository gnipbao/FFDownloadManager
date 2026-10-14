//! Discover unknown Mastodon instances only after inspecting an explicit HTML
//! page. An ordinary file at the same URL shape must not invoke the status API.
use axum::{
    body::Body,
    extract::Path,
    http::{header, HeaderMap, Method, Response, StatusCode},
    response::Html,
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use ffdownload::resolver::{Assembly, ResourceResolver};
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::{net::TcpListener, task::JoinHandle};

const VIDEO: &[u8] = b"\0\0\0\x18ftypisom\0\0\0\0isommp42\0\0\0\x08free";
const EMPTY_ZIP: &[u8] = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0";

struct Fixture {
    base: String,
    status_ids: Arc<Mutex<Vec<String>>>,
    media_ranges: Arc<Mutex<Vec<String>>>,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let status_ids = Arc::new(Mutex::new(Vec::new()));
        let media_ranges = Arc::new(Mutex::new(Vec::new()));
        let requested_ids = status_ids.clone();
        let requested_ranges = media_ranges.clone();
        let api_base = base.clone();
        let app = Router::new()
            .route(
                "/@alice/12345",
                get(|| async {
                    // No embedded player or metadata: discovery must consult
                    // this unknown instance's own exact-status endpoint.
                    Html("<!doctype html><html><title>Local social instance</title><body>Public post</body></html>")
                }),
            )
            .route(
                "/api/v1/statuses/{id}",
                get(move |Path(id): Path<String>| {
                    let requested_ids = requested_ids.clone();
                    let api_base = api_base.clone();
                    async move {
                        requested_ids.lock().unwrap().push(id);
                        Json(json!({
                            "id": "12345",
                            "url": format!("{api_base}/@alice/12345"),
                            "visibility": "public",
                            "reblog": null,
                            "account": {"id": "17", "acct": "alice"},
                            "content": "<p>Local public clip &amp; original</p>",
                            "media_attachments": [{
                                "id": "314",
                                "type": "video",
                                "url": format!("{api_base}/media/original.mp4?sig=keep%2Bexact"),
                                "preview_url": format!("{api_base}/preview.png"),
                                "meta": {"original": {"height": 720, "duration": 1.0}}
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/media/original.mp4",
                get(move |method: Method, headers: HeaderMap| {
                    let requested_ranges = requested_ranges.clone();
                    async move {
                        if method == Method::GET {
                            requested_ranges.lock().unwrap().push(
                                headers
                                    .get(header::RANGE)
                                    .and_then(|value| value.to_str().ok())
                                    .unwrap_or("full body")
                                    .to_owned(),
                            );
                        }
                        ranged(method, &headers, VIDEO, "video/mp4")
                    }
                }),
            )
            .route(
                "/@alice/67890",
                get(|method: Method, headers: HeaderMap| async move {
                    let mut response = ranged(method, &headers, EMPTY_ZIP, "application/zip");
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
            status_ids,
            media_ranges,
            server,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn ranged(method: Method, headers: &HeaderMap, data: &'static [u8], mime: &str) -> Response<Body> {
    let length = data.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("bytes="))
        .and_then(|value| value.split_once('-'));
    let (start, end) = range.map_or((0, length - 1), |(from, to)| {
        let start: usize = from.parse().unwrap();
        let end = if to.is_empty() {
            length - 1
        } else {
            to.parse::<usize>().unwrap().min(length - 1)
        };
        (start, end)
    });
    if start > end || start >= length {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{length}"))
            .body(Body::empty())
            .unwrap();
    }
    let mut builder = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(header::ACCEPT_RANGES, "bytes");
    if range.is_some() {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{length}"),
        );
    }
    builder
        .body(if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(Bytes::from_static(data).slice(start..=end))
        })
        .unwrap()
}

#[tokio::test]
async fn unknown_instance_html_discovers_exact_status_and_checks_original_media_tail() {
    let fixture = Fixture::start().await;
    let url = format!("{}/@alice/12345", fixture.base);
    let resource = ResourceResolver::with_ffmpeg(None)
        .unwrap()
        .extract(&url)
        .await
        .unwrap();

    assert_eq!(resource.platform, "Mastodon");
    assert_eq!(resource.source_url, url);
    assert_eq!(resource.title, "Local public clip & original");
    assert_eq!(resource.formats.len(), 1);
    let format = &resource.formats[0];
    assert_eq!(format.height, 720);
    assert_eq!(format.plan.extension, "mp4");
    assert_eq!(format.plan.assembly, Assembly::Direct);
    assert_eq!(format.plan.streams.len(), 1);
    assert_eq!(
        format.plan.streams[0].url,
        format!("{}/media/original.mp4?sig=keep%2Bexact", fixture.base)
    );
    assert_eq!(*fixture.status_ids.lock().unwrap(), ["12345"]);
    assert_eq!(
        *fixture.media_ranges.lock().unwrap(),
        ["bytes=0-0", "bytes=31-31", "full body"]
    );
}

#[tokio::test]
async fn binary_at_a_mastodon_shaped_path_stays_direct_without_status_api_requests() {
    let fixture = Fixture::start().await;
    let url = format!("{}/@alice/67890", fixture.base);
    let resource = ResourceResolver::with_ffmpeg(None)
        .unwrap()
        .extract(&url)
        .await
        .unwrap();

    assert_eq!(resource.platform, "Direct File");
    assert_eq!(resource.source_url, url);
    assert_eq!(resource.formats.len(), 1);
    let format = &resource.formats[0];
    assert_eq!(format.plan.extension, "zip");
    assert_eq!(format.plan.assembly, Assembly::Direct);
    assert_eq!(format.plan.streams.len(), 1);
    assert_eq!(format.plan.streams[0].url, url);
    assert!(fixture.status_ids.lock().unwrap().is_empty());
    assert!(fixture.media_ranges.lock().unwrap().is_empty());
}
