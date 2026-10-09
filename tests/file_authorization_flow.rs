use axum::{
    body::Body,
    http::{header, HeaderMap, Method, Response, StatusCode},
    routing::get,
    Router,
};
use ffdownload::{
    service::{DownloadService, Task},
    web::LocalWeb,
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

const TEST_COOKIE: &str = "session=ffdm-test-only-secret-never-a-real-cookie";
const PAYLOAD: &[u8] = b"ordinary file without browser authorization\0\x80\xff";

struct Server(tokio::task::JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn source() -> (String, Arc<AtomicUsize>, Server) {
    let hits = Arc::new(AtomicUsize::new(0));
    let public_hits = hits.clone();
    let private_hits = hits.clone();
    let router = Router::new()
        .route(
            "/public.bin",
            get(move |method: Method, headers: HeaderMap| {
                let hits = public_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::Relaxed);
                    assert!(!headers.contains_key(header::COOKIE));
                    Response::builder()
                        .header(header::CONTENT_TYPE, "application/octet-stream")
                        .header(header::CONTENT_LENGTH, PAYLOAD.len().to_string())
                        .body(if method == Method::HEAD {
                            Body::empty()
                        } else {
                            Body::from(PAYLOAD)
                        })
                        .unwrap()
                }
            }),
        )
        .route(
            "/private.zip",
            get(move |headers: HeaderMap| {
                let hits = private_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::Relaxed);
                    assert!(!headers.contains_key(header::COOKIE));
                    Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .body(Body::from("login required"))
                        .unwrap()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = Server(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    (origin, hits, server)
}

async fn terminal_task(service: &DownloadService, id: &str) -> Task {
    tokio::time::timeout(Duration::from_secs(15), async {
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
    .expect("local fixture should finish promptly")
}

fn assert_no_secret(value: &str) {
    assert!(!value.contains("ffdm-test-only-secret"));
}

fn assert_files_contain_no_secret(path: &Path) {
    if !path.exists() {
        return;
    }
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            assert_files_contain_no_secret(&entry.path());
        } else {
            // Windows enforces the service's exclusive byte-range lock even
            // for reads. These lock files contain no application data.
            if matches!(
                entry.file_name().to_str(),
                Some("manager.lock" | "session.lock")
            ) {
                assert_eq!(entry.metadata().unwrap().len(), 0);
                continue;
            }
            let bytes = std::fs::read(entry.path()).unwrap();
            assert_no_secret(&String::from_utf8_lossy(&bytes));
        }
    }
}

fn post(client: &reqwest::Client, origin: &str, path: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{origin}{path}"))
        .header("X-FFDM-Client", "local-ui")
        .header(header::ORIGIN, origin)
}

#[tokio::test]
async fn cookies_on_http_and_header_injection_are_rejected_before_source_requests() {
    let (source, hits, _source_server) = source().await;
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
    let https_source = source.replacen("http://", "https://", 1);
    for (url, cookie, hint) in [
        (
            format!("{source}/public.bin"),
            TEST_COOKIE.to_owned(),
            "HTTPS",
        ),
        (
            format!("{https_source}/public.bin"),
            format!("{TEST_COOKIE}\r\nX-Injected: value"),
            "Cookie",
        ),
        (
            format!("{https_source}/public.bin"),
            format!("{TEST_COOKIE}\0invalid"),
            "Cookie",
        ),
    ] {
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            post(&client, &origin, "/api/tasks")
                .header(header::CONTENT_TYPE, "application/json")
                .body(json!({"url":url,"direct":true,"cookie":cookie}).to_string())
                .send(),
        )
        .await
        .expect("invalid authorization must be rejected without network I/O")
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.text().await.unwrap();
        assert!(body.contains(hint), "{body}");
        assert_no_secret(&body);
    }
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert!(service.snapshot().tasks.is_empty());
    service.shutdown().await.unwrap();
    assert_files_contain_no_secret(dir.path());
}

#[tokio::test]
async fn old_clients_still_download_and_plaintext_authorization_retry_preserves_failed_task() {
    let (source, hits, _source_server) = source().await;
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
    let mut private_id = String::new();
    for path in ["public.bin", "private.zip"] {
        let response = post(&client, &origin, "/api/tasks")
            .header(header::CONTENT_TYPE, "application/json")
            .body(json!({"url":format!("{source}/{path}")}).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let task: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        let id = task["id"].as_str().unwrap();
        let task = terminal_task(&service, id).await;
        if path == "public.bin" {
            assert_eq!(task.state, "completed", "{:?}", task.error);
            assert_eq!(
                std::fs::read(service.output_path(id).unwrap()).unwrap(),
                PAYLOAD
            );
        } else {
            assert_eq!(task.state, "error");
            assert!(task.error.as_deref().unwrap().contains("401"));
            private_id = id.to_owned();
        }
    }
    let previous = serde_json::to_value(service.snapshot()).unwrap();
    let hit_count = hits.load(Ordering::Relaxed);
    let resume_path = format!("/api/tasks/{private_id}/resume");
    let response = post(&client, &origin, &resume_path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"cookie":TEST_COOKIE}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = response.text().await.unwrap();
    assert!(error.contains("HTTPS"), "{error}");
    assert_no_secret(&error);
    assert_eq!(serde_json::to_value(service.snapshot()).unwrap(), previous);
    assert_eq!(hits.load(Ordering::Relaxed), hit_count);

    // Older web/desktop clients send an empty body to the resume endpoint.
    let response = post(&client, &origin, &resume_path).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let failed = terminal_task(&service, &private_id).await;
    assert_eq!(failed.state, "error");
    assert!(failed.error.as_deref().unwrap().contains("401"));
    let snapshot = client
        .get(format!("{origin}/api/tasks"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_no_secret(&snapshot);
    assert!(!dir.path().join("files/private.zip").exists());
    service.shutdown().await.unwrap();
    assert_files_contain_no_secret(dir.path());
}

#[tokio::test]
async fn reopened_authorized_tasks_require_a_new_cookie_and_legacy_records_still_load() {
    let (source, hits, _source_server) = source().await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("files");
    let state = dir.path().join("state");
    let service = DownloadService::open(&downloads, &state).unwrap();
    let task = service
        .create(
            serde_json::from_value(json!({"url":format!("{source}/private.zip")})).unwrap(),
            false,
        )
        .await
        .unwrap();
    terminal_task(&service, &task.id).await;
    service.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&service) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(service);

    let state_path = state.join("tasks.json");
    let mut saved: Value = serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    let mut protected = saved["tasks"][0].clone();
    protected["id"] = json!("needs-new-authorization");
    protected["url"] = json!(format!(
        "{}/private.zip",
        source.replacen("http://", "https://", 1)
    ));
    protected["filename"] = json!("protected.zip");
    protected["state"] = json!("paused");
    protected["error"] = Value::Null;
    protected["authorization_required"] = json!(true);
    saved["tasks"][0]
        .as_object_mut()
        .unwrap()
        .remove("authorization_required");
    saved["tasks"].as_array_mut().unwrap().push(protected);
    std::fs::write(&state_path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let service = DownloadService::open(&downloads, &state).unwrap();
    let snapshot = serde_json::to_value(service.snapshot()).unwrap();
    let tasks = snapshot["tasks"].as_array().unwrap();
    let legacy = tasks.iter().find(|saved| saved["id"] == task.id).unwrap();
    assert_eq!(legacy["authorization_required"], false);
    let protected = tasks
        .iter()
        .find(|task| task["id"] == "needs-new-authorization")
        .unwrap();
    assert_eq!(protected["authorization_required"], true);
    assert_eq!(protected["authorization_needed"], true);
    let hit_count = hits.load(Ordering::Relaxed);
    let error = service
        .resume("needs-new-authorization")
        .unwrap_err()
        .to_string();
    assert!(error.contains("Cookie"), "{error}");
    assert_eq!(serde_json::to_value(service.snapshot()).unwrap(), snapshot);
    assert_eq!(hits.load(Ordering::Relaxed), hit_count);
    service.shutdown().await.unwrap();
    assert_files_contain_no_secret(dir.path());
}
