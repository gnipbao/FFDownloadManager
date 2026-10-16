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
const TEST_BEARER: &str = "Bearer ffdm-test-only-secret-header-token";
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

async fn held_source() -> (String, tokio::sync::watch::Sender<bool>, Server) {
    let (release, released) = tokio::sync::watch::channel(false);
    let router = Router::new().route(
        "/held.bin",
        get(move |method: Method| {
            let mut released = released.clone();
            async move {
                let body = if method == Method::HEAD {
                    Body::empty()
                } else {
                    Body::from_stream(futures_util::stream::once(async move {
                        while !*released.borrow_and_update() {
                            if released.changed().await.is_err() {
                                break;
                            }
                        }
                        Ok::<_, std::convert::Infallible>(PAYLOAD)
                    }))
                };
                Response::builder()
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(header::CONTENT_LENGTH, PAYLOAD.len())
                    .body(body)
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = Server(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    (origin, release, server)
}

fn saved_task(id: &str, url: &str, state: &str) -> Value {
    json!({
        "id":id,"filename":format!("{id}.zip"),"url":url,
        "connections":4,"state":state,"created_at":1,"completed_at":null,
        "total_bytes":8192,"completed_bytes":1024,"speed_bytes":0,
        "active_connections":0,"eta_seconds":null,"sha256":null,
        "expected_sha256":null,"error":if state == "error" { Some("HTTP 401") } else { None },
        "demo":false
    })
}

fn task_value(service: &DownloadService, id: &str) -> Value {
    serde_json::to_value(
        service
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.id == id)
            .unwrap(),
    )
    .unwrap()
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
async fn custom_headers_validate_at_the_web_boundary_before_any_source_request() {
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
    let mut requests = vec![
        json!({"url":format!("{source}/public.bin"),"headers":[["Authorization",TEST_BEARER]]}),
        json!({"headers":[["Authorization",TEST_BEARER],["authorization",TEST_BEARER]]}),
        json!({"headers":[["Bad Header Name",TEST_BEARER]]}),
        json!({"headers":[[":authority",TEST_BEARER]]}),
        json!({"headers":[["Authorization",format!("{TEST_BEARER}\r\nX-Injected: value")]]}),
        json!({"headers":[["Authorization",format!("{TEST_BEARER}\0value")]]}),
        json!({"headers":[["Authorization",format!("{TEST_BEARER}\tvalue")]]}),
        json!({"cookie":TEST_COOKIE,"headers":[["Cookie",TEST_COOKIE]]}),
        json!({"headers":TEST_BEARER}),
        json!({"headers":[["Authorization",TEST_BEARER,TEST_BEARER]]}),
        json!({"headers":[["Authorization",{"token":TEST_BEARER}]]}),
        json!({"headers":[["X-Token",format!("{TEST_BEARER}{}", "x".repeat(16 * 1024))]]}),
        json!({"headers":(0..49).map(|index| [format!("X-Test-{index}"),TEST_BEARER.into()]).collect::<Vec<_>>()}),
    ];
    for name in [
        "Host",
        "Range",
        "If-Range",
        "If-Match",
        "If-None-Match",
        "If-Modified-Since",
        "If-Unmodified-Since",
        "Content-Length",
        "Transfer-Encoding",
        "Connection",
        "Keep-Alive",
        "TE",
        "Trailer",
        "Upgrade",
        "Proxy-Authorization",
        "Proxy-Connection",
        "Accept-Encoding",
    ] {
        requests.push(json!({"headers":[[name,TEST_BEARER]]}));
    }
    for mut request in requests {
        if request.get("url").is_none() {
            request["url"] = json!(format!("{https_source}/public.bin"));
        }
        request["direct"] = json!(true);
        let response = tokio::time::timeout(
            Duration::from_secs(2),
            post(&client, &origin, "/api/tasks")
                .header(header::CONTENT_TYPE, "application/json")
                .body(request.to_string())
                .send(),
        )
        .await
        .expect("invalid headers must be rejected before source I/O")
        .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_no_secret(&response.text().await.unwrap());
    }
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    assert!(service.snapshot().tasks.is_empty());
    service.shutdown().await.unwrap();
    assert_files_contain_no_secret(dir.path());
}

#[tokio::test]
async fn web_header_retries_preserve_tasks_keep_secrets_private_and_expire_on_restart() {
    let (source, release, _source_server) = held_source().await;
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("files");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&downloads).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let private_url = format!("{}/private.zip", source.replacen("http://", "https://", 1));
    std::fs::write(
        state.join("tasks.json"),
        serde_json::to_vec(&json!({
            "schema":1,"directory":downloads.canonicalize().unwrap(),
            "tasks":[saved_task("failed-auth",&private_url,"error"),
                     saved_task("paused-auth",&private_url,"paused")]
        }))
        .unwrap(),
    )
    .unwrap();
    let web = LocalWeb::bind(0, downloads.clone(), state.clone())
        .await
        .unwrap();
    let origin = format!("http://{}", web.address);
    let service = web.service;
    let web_server = Server(tokio::spawn(async move {
        axum::serve(web.listener, web.router).await.unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    // Occupy every real worker with a localhost body held by this test. The
    // protected retries stay queued, so acceptance and persistence are tested
    // without making a TLS request or weakening production TLS verification.
    for index in 0..service.snapshot().max_active {
        let response = post(&client, &origin, "/api/tasks")
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                json!({
                    "url":format!("{source}/held.bin"),"filename":format!("held-{index}.bin"),
                    "connections":1
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_no_secret(&response.text().await.unwrap());
    }

    for id in ["failed-auth", "paused-auth"] {
        let path = format!("/api/tasks/{id}/resume");
        let response = post(&client, &origin, &path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                json!({"headers":[["Authorization",TEST_BEARER],["X-Api-Key",TEST_COOKIE]]})
                    .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_no_secret(&response.text().await.unwrap());
        let task = service
            .snapshot()
            .tasks
            .into_iter()
            .find(|task| task.id == id)
            .unwrap();
        assert_eq!(task.state, "queued");
        assert_eq!(task.completed_bytes, 1024);
        assert_eq!(task.total_bytes, Some(8192));
        assert!(task.authorization_required);
        assert!(!task.authorization_needed);
        assert!(task.error.is_none());
        service.pause(id).unwrap();
        let before = task_value(&service, id);
        let response = post(&client, &origin, &path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                json!({"headers":[["Authorization",TEST_BEARER],["authorization",TEST_BEARER]]})
                    .to_string(),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_no_secret(&response.text().await.unwrap());
        assert_eq!(task_value(&service, id), before);

        // Existing callers may omit the body; credentials remain reusable only
        // within this process, without a second prompt while the task is paused.
        let response = post(&client, &origin, &path).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        service.pause(id).unwrap();
    }
    let snapshot = client
        .get(format!("{origin}/api/tasks"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_no_secret(&snapshot);
    assert_files_contain_no_secret(dir.path());
    release.send_replace(true);
    service.shutdown().await.unwrap();
    drop(client);
    drop(web_server);
    tokio::time::timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&service) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(service);

    let web = LocalWeb::bind(0, downloads, state).await.unwrap();
    let origin = format!("http://{}", web.address);
    let service = web.service.clone();
    let _web_server = Server(tokio::spawn(async move {
        axum::serve(web.listener, web.router).await.unwrap();
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for id in ["failed-auth", "paused-auth"] {
        let before = serde_json::to_value(service.snapshot()).unwrap();
        let protected = before["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|task| task["id"] == id)
            .unwrap();
        assert_eq!(protected["authorization_required"], true);
        assert_eq!(protected["authorization_needed"], true);
        assert_eq!(protected["state"], "paused");
        let response = post(&client, &origin, &format!("/api/tasks/{id}/resume"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.text().await.unwrap();
        assert!(body.contains("Header") || body.contains("header"), "{body}");
        assert_no_secret(&body);
        assert_eq!(serde_json::to_value(service.snapshot()).unwrap(), before);
    }
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
