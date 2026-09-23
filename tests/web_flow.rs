use ffdownload::{
    service::{DownloadService, NewTask},
    test_server::{fixture_data, ServerConfig, TestServer},
    web::LocalWeb,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

async fn wait_until(
    service: &DownloadService,
    id: &str,
    predicate: impl Fn(&ffdownload::service::Task) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snapshot = service.snapshot();
            let task = snapshot.tasks.iter().find(|t| t.id == id).unwrap();
            assert_ne!(task.state, "error", "{:?}", task.error);
            if predicate(task) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn live_controls_persist_resume_and_preserve_completed_files() {
    let data = fixture_data(8 * 1024 * 1024);
    let hash = format!("{:x}", Sha256::digest(&data));
    let mut config = ServerConfig::new(data.clone());
    config.per_response_mbps = Some(12.0);
    let server = TestServer::start(config).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("files");
    let state = dir.path().join("state");
    let service = DownloadService::open(&downloads, &state).unwrap();
    assert!(
        DownloadService::open(&downloads, &state).is_err(),
        "one host per task store"
    );
    let task = service
        .create(
            NewTask {
                url: server.url.clone(),
                filename: Some("resume.bin".into()),
                connections: 4,
                sha256: Some(hash.clone()),
            },
            false,
        )
        .await
        .unwrap();
    wait_until(&service, &task.id, |t| {
        t.completed_bytes > 0 && t.state == "downloading"
    })
    .await;
    service.pause(&task.id).unwrap();
    wait_until(&service, &task.id, |t| t.state == "paused").await;
    let checkpoint = service.snapshot().tasks[0].completed_bytes;
    assert!(checkpoint > 0 && checkpoint < data.len() as u64);
    assert!(!downloads.join("resume.bin").exists());
    service.shutdown().await.unwrap();
    // All worker references have been released after checkpointing.
    tokio::time::timeout(Duration::from_secs(3), async {
        while Arc::strong_count(&service) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(service);
    let service = DownloadService::open(&downloads, &state).unwrap();
    assert_eq!(service.snapshot().tasks[0].state, "paused");
    assert_eq!(service.snapshot().tasks[0].completed_bytes, checkpoint);
    service.resume(&task.id).unwrap();
    wait_until(&service, &task.id, |t| t.state == "completed").await;
    assert_eq!(
        service.snapshot().tasks[0].sha256.as_deref(),
        Some(hash.as_str())
    );
    assert_eq!(
        std::fs::read(service.output_path(&task.id).unwrap()).unwrap(),
        data
    );
    service.remove(&task.id).unwrap();
    assert!(service.snapshot().tasks.is_empty());
    assert!(
        downloads.join("resume.bin").is_file(),
        "removing history must not delete files"
    );
    service.shutdown().await.unwrap();
    server.stop().await.unwrap();
}

#[tokio::test]
async fn queue_is_bounded_and_rejects_path_traversal() {
    let mut config = ServerConfig::new(fixture_data(8 * 1024 * 1024));
    config.per_response_mbps = Some(2.0);
    let server = TestServer::start(config).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let service =
        DownloadService::open(&dir.path().join("files"), &dir.path().join("state")).unwrap();
    for filename in [
        "../outside.bin",
        "/tmp/escape.bin",
        ".hidden",
        "nested/file.bin",
        "bad\\file",
        "bad:name",
    ] {
        assert!(service
            .create(
                NewTask {
                    url: server.url.clone(),
                    filename: Some(filename.into()),
                    connections: 4,
                    sha256: None,
                },
                false
            )
            .await
            .is_err());
    }
    for _ in 0..3 {
        service
            .create(
                NewTask {
                    url: server.url.clone(),
                    filename: Some("file.bin".into()),
                    connections: 1,
                    sha256: None,
                },
                false,
            )
            .await
            .unwrap();
    }
    let tasks = service.snapshot().tasks;
    assert_eq!(tasks.iter().filter(|t| t.state == "queued").count(), 1);
    let mut names: Vec<_> = tasks.iter().map(|t| t.filename.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 3);
    service.pause_all().unwrap();
    for task in tasks {
        wait_until(&service, &task.id, |t| t.state == "paused").await;
    }
    service.shutdown().await.unwrap();
    server.stop().await.unwrap();
}

#[tokio::test]
async fn http_ui_creates_real_download_and_blocks_foreign_mutations() {
    let source = TestServer::start(ServerConfig::new(fixture_data(3 * 1024 * 1024)))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let web = LocalWeb::bind(0, dir.path().join("files"), dir.path().join("state"))
        .await
        .unwrap();
    let origin = format!("http://{}", web.address);
    let service = web.service.clone();
    let server_task = tokio::spawn(async move {
        axum::serve(web.listener, web.router).await.unwrap();
    });
    // This test deliberately spoofs Host; never send it through the OS proxy.
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client.get(&origin).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key("content-security-policy"));
    assert!(response.text().await.unwrap().contains("新建下载"));
    assert_eq!(
        client
            .get(format!("{origin}/api/tasks"))
            .header("Host", "evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .post(format!("{origin}/api/demo"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .post(format!("{origin}/api/media/resolve"))
            .header("Content-Type", "application/json")
            .body(json!({"url":"https://youtu.be/abcdefghijk"}).to_string())
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let stale = client
        .post(format!("{origin}/api/media/tasks"))
        .header("X-FFDM-Client", "local-ui")
        .header("Content-Type", "application/json")
        .body(json!({"preview_id":"expired","format_id":"18"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 400);
    assert!(stale.text().await.unwrap().contains("过期"));
    let page = client
        .post(format!("{origin}/api/tasks"))
        .header("X-FFDM-Client", "local-ui")
        .header("Content-Type", "application/json")
        .body(json!({"url":"https://www.youtube.com/watch?v=abcdefghijk"}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(
        page.status(),
        400,
        "video pages must not become HTML file downloads"
    );
    assert_eq!(
        client
            .post(format!("{origin}/api/demo"))
            .header("X-FFDM-Client", "local-ui")
            .header("Origin", "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let response = client
        .post(format!("{origin}/api/tasks"))
        .header("X-FFDM-Client", "local-ui")
        .header("Origin", &origin)
        .header("Content-Type", "application/json")
        .body(json!({"url":source.url,"filename":"from-browser.bin","connections":4}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let task: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let id = task["id"].as_str().unwrap();
    wait_until(&service, id, |t| t.state == "completed").await;
    let body = client
        .get(format!("{origin}/api/tasks/{id}/file"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body, fixture_data(3 * 1024 * 1024));
    let response = client
        .delete(format!("{origin}/api/tasks/{id}"))
        .header("X-FFDM-Client", "local-ui")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(service.snapshot().tasks.is_empty());
    service.shutdown().await.unwrap();
    server_task.abort();
    source.stop().await.unwrap();
}
