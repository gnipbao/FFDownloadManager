use axum::{
    body::Body,
    http::{header, Method, Response, StatusCode},
    routing::get,
    Router,
};
use ffdownload::{
    service::{DownloadService, NewTask, Task},
    test_server::fixture_data,
    web::LocalWeb,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct Server(tokio::task::JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(router: Router) -> (String, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = Server(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    (origin, server)
}

async fn terminal_task(service: &DownloadService, id: &str) -> Task {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let task = service
                .snapshot()
                .tasks
                .into_iter()
                .find(|task| task.id == id)
                .unwrap();
            if matches!(task.state.as_str(), "completed" | "error") {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("local file task must reach a terminal state")
}

fn file_response(method: Method, bytes: Vec<u8>, mime: &str) -> Response<Body> {
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .body(if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(bytes)
        })
        .unwrap()
}

#[tokio::test]
async fn http_file_tasks_download_zip_custom_suffix_and_extensionless_endpoints() {
    // A valid empty ZIP, and arbitrary non-media bytes with an unknown suffix.
    let zip = b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0".to_vec();
    let custom = fixture_data(64 * 1024).to_vec();
    let zip_body = zip.clone();
    let custom_body = custom.clone();
    let export_body = custom.clone();
    let rejected_heads = Arc::new(AtomicUsize::new(0));
    let head_count = rejected_heads.clone();
    let router = Router::new()
        .route(
            "/archive.zip",
            get(move |method: Method| {
                let bytes = zip_body.clone();
                async move { file_response(method, bytes, "application/zip") }
            }),
        )
        .route(
            "/project.assetpack",
            get(move |method: Method| {
                let bytes = custom_body.clone();
                async move { file_response(method, bytes, "application/octet-stream") }
            }),
        )
        .route(
            "/export",
            get(move |method: Method| {
                let bytes = export_body.clone();
                let head_count = head_count.clone();
                async move {
                    if method == Method::HEAD {
                        head_count.fetch_add(1, Ordering::Relaxed);
                        return Response::builder()
                            .status(StatusCode::METHOD_NOT_ALLOWED)
                            .body(Body::empty())
                            .unwrap();
                    }
                    let mut response = file_response(method, bytes, "application/octet-stream");
                    response.headers_mut().insert(
                        header::CONTENT_DISPOSITION,
                        "attachment; filename*=UTF-8''%E5%A4%87%E4%BB%BD.assetpack"
                            .parse()
                            .unwrap(),
                    );
                    response
                }
            }),
        );
    let (source, _source_server) = serve(router).await;
    let dir = tempfile::tempdir().unwrap();
    let web = LocalWeb::bind(0, dir.path().join("files"), dir.path().join("state"))
        .await
        .unwrap();
    let origin = format!("http://{}", web.address);
    let service = web.service.clone();
    let _web_server = Server(tokio::spawn(async move {
        axum::serve(web.listener, web.router).await.unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    for (path, filename, data, direct) in [
        ("/archive.zip", "archive.zip", &zip, None),
        (
            "/project.assetpack",
            "project.assetpack",
            &custom,
            Some(true),
        ),
        ("/export", "备份.assetpack", &custom, Some(false)),
    ] {
        let hash = format!("{:x}", Sha256::digest(data));
        let mut request = json!({
            "url": format!("{source}{path}"),
            "connections": 4,
            "sha256": hash,
        });
        if let Some(direct) = direct {
            request["direct"] = json!(direct);
        }
        let response = client
            .post(format!("{origin}/api/tasks"))
            .header("X-FFDM-Client", "local-ui")
            .header(header::ORIGIN, &origin)
            .header(header::CONTENT_TYPE, "application/json")
            .body(request.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let task: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert_eq!(task["filename"], filename);
        assert!(
            task["media"].is_null(),
            "file links must not use a media plan"
        );
        let id = task["id"].as_str().unwrap();
        let task = terminal_task(&service, id).await;
        assert_eq!(task.state, "completed", "{:?}", task.error);
        assert_eq!(task.completed_bytes, data.len() as u64);
        assert_eq!(task.sha256.as_deref(), Some(hash.as_str()));
        assert!(task.media.is_none());
        assert_eq!(
            std::fs::read(service.output_path(id).unwrap()).unwrap(),
            *data
        );
    }
    assert!(rejected_heads.load(Ordering::Relaxed) > 0);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn automatic_video_pages_are_rejected_before_io_and_direct_mode_bypasses_only_routing() {
    let dir = tempfile::tempdir().unwrap();
    let service =
        DownloadService::open(&dir.path().join("files"), &dir.path().join("state")).unwrap();
    // Invalid connection counts stop explicit file mode before any external I/O.
    // Auto mode must first identify the video page and request media parsing.
    for direct in [None, Some(false), Some(true)] {
        let mut request = json!({
            "url": "https://www.youtube.com/watch?v=abcdefghijk",
            "connections": 0,
        });
        if let Some(direct) = direct {
            request["direct"] = json!(direct);
        }
        let request: NewTask = serde_json::from_value(request).unwrap();
        assert_eq!(request.direct, direct.unwrap_or(false));
        let error = service
            .create(request, false)
            .await
            .unwrap_err()
            .to_string();
        if direct == Some(true) {
            assert!(error.contains("连接数"), "{error}");
        } else {
            assert!(error.contains("先解析"), "{error}");
        }
    }
    assert!(service.snapshot().tasks.is_empty());
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_http_sources_preserve_status_and_never_publish_an_error_page() {
    let mut router = Router::new();
    for status in [401, 403, 404] {
        router = router.route(
            &format!("/{status}.zip"),
            get(move || async move {
                Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "text/html")
                    .body(Body::from("<html>Download unavailable</html>"))
                    .unwrap()
            }),
        );
    }
    let (source, _source_server) = serve(router).await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("files");
    let service = DownloadService::open(&downloads, &dir.path().join("state")).unwrap();
    for status in [401, 403, 404] {
        let task = service
            .create(
                serde_json::from_value(json!({
                    "url": format!("{source}/{status}.zip"),
                    "direct": true,
                }))
                .unwrap(),
                false,
            )
            .await
            .unwrap();
        let task = terminal_task(&service, &task.id).await;
        assert_eq!(task.state, "error");
        let error = task.error.as_deref().unwrap();
        assert!(error.contains(&status.to_string()), "{error}");
        if status == 401 {
            assert!(error.contains("登录授权"), "{error}");
        } else if status == 403 {
            assert!(error.contains("拒绝访问"), "{error}");
        }
        assert!(task.media.is_none());
        assert!(task.sha256.is_none());
        assert!(service.output_path(&task.id).is_err());
        assert!(!downloads.join(&task.filename).exists());
    }
    service.shutdown().await.unwrap();
}
