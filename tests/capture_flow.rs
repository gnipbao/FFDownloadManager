#![cfg(debug_assertions)]

use ffdownload::{capture::CaptureService, service::DownloadService, web::LocalWeb};
use reqwest::{Client, Proxy};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn loopback_proxy_capture_download_and_stop() {
    let dir = tempfile::tempdir().unwrap();
    let web = LocalWeb::bind(0, dir.path().join("files"), dir.path().join("state"))
        .await
        .unwrap();
    let origin = format!("http://{}", web.address);
    let capture = web.capture.clone();
    let service = web.service.clone();
    let server = tokio::spawn(async move {
        axum::serve(web.listener, web.router).await.unwrap();
    });
    let ui = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    assert_eq!(
        ui.post(format!("{origin}/api/capture/start"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert!(ui
        .post(format!("{origin}/api/capture/start"))
        .header("x-ffdm-client", "local-ui")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let snapshot = capture.snapshot().await;
    assert!(snapshot.running);
    let address = snapshot.proxy.unwrap();
    let proxy = Client::builder()
        .no_proxy()
        .proxy(Proxy::http(format!("http://{address}")).unwrap())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let video = proxy
        .get(format!("{origin}/capture-demo/sample.mp4?token=private"))
        .header("Cookie", "capture=private")
        .header("Range", "bytes=0-100")
        .send()
        .await
        .unwrap();
    assert_eq!(video.status(), 206);
    assert_eq!(video.bytes().await.unwrap().len(), 101);
    let snapshot = capture.snapshot().await;
    assert_eq!(snapshot.resources.len(), 1);
    let rendered = serde_json::to_string(&snapshot).unwrap();
    assert!(!rendered.contains("private"));
    let plan = capture.select(&snapshot.resources[0].id, None).unwrap();
    assert!(plan.streams[0]
        .headers
        .iter()
        .any(|(k, v)| k == "cookie" && v == "capture=private"));
    assert!(!plan.streams[0].headers.iter().any(|(k, _)| k == "range"));
    let response = ui
        .post(format!(
            "{origin}/api/capture/{}/download",
            snapshot.resources[0].id
        ))
        .header("x-ffdm-client", "local-ui")
        .header("content-type", "application/json")
        .body(json!({"connections":4}).to_string())
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{}",
        response.text().await.unwrap()
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let task = service.snapshot().tasks.remove(0);
            assert_ne!(task.state, "error", "{:?}", task.error);
            if task.state == "completed" {
                assert_eq!(
                    std::fs::read(service.output_path(&task.id).unwrap()).unwrap(),
                    include_bytes!("fixtures/sample-av.mp4")
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        proxy
            .get(format!("{origin}/api/tasks"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    capture.stop().await.unwrap();
    assert!(!capture.snapshot().await.running);
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    service.shutdown().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn captured_download_keeps_exact_origin_headers() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::get,
        Router,
    };
    let expected = vec![7u8; 2 * 1024 * 1024];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let data = expected.clone();
    let app = Router::new().route(
        "/capture-demo/video",
        get(move |req: Request<Body>| {
            let data = data.clone();
            async move {
                if req
                    .headers()
                    .get("cookie")
                    .is_none_or(|v| v != "authorized=1")
                    || req
                        .headers()
                        .get("referer")
                        .is_none_or(|v| v != "https://example.com/watch")
                {
                    return axum::http::Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(Body::empty())
                        .unwrap();
                }
                axum::http::Response::builder()
                    .header("content-type", "video/mp4")
                    .header("content-length", data.len())
                    .body(if req.method() == "HEAD" {
                        Body::empty()
                    } else {
                        Body::from(data)
                    })
                    .unwrap()
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    let capture = CaptureService::new(dir.path().join("browser"), origin.clone());
    capture.start().await.unwrap();
    let proxy = Client::builder()
        .no_proxy()
        .proxy(
            Proxy::http(format!(
                "http://{}",
                capture.snapshot().await.proxy.unwrap()
            ))
            .unwrap(),
        )
        .build()
        .unwrap();
    proxy
        .get(format!("{origin}/capture-demo/video"))
        .header("cookie", "authorized=1")
        .header("referer", "https://example.com/watch")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let id = capture.snapshot().await.resources[0].id.clone();
    let plan = capture.select(&id, None).unwrap();
    let service =
        DownloadService::open(&dir.path().join("files"), &dir.path().join("state")).unwrap();
    let task = service.create_captured(plan, 4).unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let t = service.snapshot().tasks.remove(0);
            assert_ne!(t.state, "error", "{:?}", t.error);
            if t.state == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(service.output_path(&task.id).unwrap()).unwrap(),
        expected
    );
    capture.stop().await.unwrap();
    service.shutdown().await.unwrap();
    server.abort();
}
