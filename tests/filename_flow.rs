use axum::{
    body::Body,
    http::{header, Method, Response, StatusCode},
    routing::get,
    Router,
};
use ffdownload::{
    filename::FilenameResolver,
    service::{DownloadService, NewTask},
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[tokio::test]
async fn redirected_header_name_is_used_for_the_published_file() {
    let hits = Arc::new(AtomicUsize::new(0));
    let count = hits.clone();
    let router = Router::new()
        .route("/redirect", get(|| async {
            Response::builder().status(StatusCode::FOUND).header(header::LOCATION, "/tunnel")
                .body(Body::empty()).unwrap()
        }))
        .route("/tunnel", get(move |method: Method| {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::Relaxed);
                Response::builder()
                    .header(header::CONTENT_DISPOSITION, "attachment; filename=report.bin; filename*=UTF-8''%E6%8A%A5%E5%91%8A.pdf")
                    .header(header::CONTENT_TYPE, "application/pdf")
                    .header(header::CONTENT_LENGTH, "17")
                    .body(if method == Method::HEAD { Body::empty() } else { Body::from("%PDF-1.7\n%%EOF\n\n\n") }).unwrap()
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/redirect", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let service =
        DownloadService::open(&dir.path().join("files"), &dir.path().join("state")).unwrap();
    let info = service.suggest_filename(&url).await.unwrap();
    assert_eq!(info.filename, "报告.pdf");
    assert_eq!(info.extension.as_deref(), Some("pdf"));
    service.suggest_filename(&url).await.unwrap();
    assert_eq!(
        hits.load(Ordering::Relaxed),
        1,
        "UI and submission share cached metadata"
    );
    let task = service
        .create(
            NewTask {
                url,
                filename: Some("我的报告".into()),
                connections: 1,
                sha256: None,
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(task.filename, "我的报告.pdf");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let task = service.snapshot().tasks.remove(0);
            assert_ne!(task.state, "error", "{:?}", task.error);
            if task.state == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(service.output_path(&task.id).unwrap()).unwrap(),
        b"%PDF-1.7\n%%EOF\n\n\n"
    );
    service.shutdown().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn head_rejection_uses_get_headers_and_error_pages_do_not_change_suffix() {
    let router = Router::new()
        .route(
            "/tunnel",
            get(|method: Method| async move {
                if method == Method::HEAD {
                    Response::builder().status(405).body(Body::empty()).unwrap()
                } else {
                    Response::builder()
                        .status(206)
                        .header(header::CONTENT_TYPE, "video/mp4")
                        .header(header::CONTENT_RANGE, "bytes 0-0/1000000")
                        .body(Body::from("x"))
                        .unwrap()
                }
            }),
        )
        .route(
            "/missing.zip",
            get(|| async {
                Response::builder()
                    .status(404)
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from("not found"))
                    .unwrap()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let resolver = FilenameResolver::new().unwrap();
    let info = resolver.resolve(&format!("{origin}/tunnel")).await.unwrap();
    assert_eq!(info.filename, "tunnel.mp4");
    assert_eq!(info.source, "content-type");
    let info = resolver
        .resolve(&format!("{origin}/missing.zip"))
        .await
        .unwrap();
    assert_eq!(info.filename, "missing.zip");
    assert_eq!(info.source, "url");
    server.abort();
}
