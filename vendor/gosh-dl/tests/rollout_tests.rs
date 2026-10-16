#![cfg(feature = "http")]

use gosh_dl::{DownloadEngine, DownloadOptions, DownloadState, EngineConfig};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn engine(dir: &TempDir) -> std::sync::Arc<DownloadEngine> {
    DownloadEngine::new(EngineConfig {
        download_dir: dir.path().into(),
        max_connections_per_download: 1,
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn completed(engine: &DownloadEngine, id: gosh_dl::DownloadId) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = engine.status(id).unwrap();
            assert!(
                !matches!(status.state, DownloadState::Error { .. }),
                "{:?}",
                status.state
            );
            if status.state == DownloadState::Completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("download should complete");
}

#[tokio::test]
async fn nested_single_stream_download_creates_parent_directories() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("nested payload"))
        .mount(&server)
        .await;
    let engine = engine(&dir).await;
    let id = engine
        .add_http(
            &format!("{}/file", server.uri()),
            DownloadOptions {
                filename: Some("sub/dir/file.bin".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    completed(&engine, id).await;
    assert_eq!(
        tokio::fs::read(dir.path().join("sub/dir/file.bin"))
            .await
            .unwrap(),
        b"nested payload"
    );
    assert_eq!(
        engine.status(id).unwrap().metadata.filename.as_deref(),
        Some("sub/dir/file.bin")
    );
    assert!(engine.verify(id).await.unwrap().valid);
    let mut reader = engine.open_reader(id, 0).unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), reader.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"nested payload");
    engine.cancel(id, true).await.unwrap();
    assert!(!dir.path().join("sub/dir/file.bin").exists());
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsafe_filenames_are_rejected_before_enqueuing() {
    let dir = TempDir::new().unwrap();
    let engine = engine(&dir).await;
    for filename in ["../outside", "/outside", "", ".", "sub/.."] {
        assert!(
            engine
                .add_http(
                    "http://127.0.0.1:1/file",
                    DownloadOptions {
                        filename: Some(filename.into()),
                        ..Default::default()
                    }
                )
                .await
                .is_err(),
            "accepted {filename:?}"
        );
    }
    assert!(engine.list().is_empty());
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn single_stream_head_uses_download_authentication() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(header("Authorization", "Bearer example"))
        .and(header("Referer", "https://example.com/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "\"authenticated\"")
                .insert_header("Content-Length", "7"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(header("Authorization", "Bearer example"))
        .respond_with(ResponseTemplate::new(200).set_body_string("payload"))
        .mount(&server)
        .await;
    let engine = engine(&dir).await;
    let id = engine
        .add_http(
            &format!("{}/file", server.uri()),
            DownloadOptions {
                headers: vec![("Authorization".into(), "Bearer example".into())],
                referer: Some("https://example.com/".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    completed(&engine, id).await;
    assert_eq!(
        engine.status(id).unwrap().metadata.etag.as_deref(),
        Some("\"authenticated\"")
    );
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_reader_does_not_hang_on_a_missing_file() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("payload"))
        .mount(&server)
        .await;
    let engine = engine(&dir).await;
    let id = engine
        .add_http(
            &format!("{}/file", server.uri()),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    completed(&engine, id).await;
    tokio::fs::remove_file(dir.path().join("file"))
        .await
        .unwrap();
    let mut reader = engine.open_reader(id, 0).unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), reader.read_to_end(&mut bytes))
        .await
        .expect("missing completed files must terminate the stream")
        .unwrap();
    assert!(bytes.is_empty());
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_a_waiting_reader_releases_the_engine() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("payload")
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    let engine = engine(&dir).await;
    let id = engine
        .add_http(
            &format!("{}/file", server.uri()),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    engine.pause(id).await.unwrap();
    let reader = engine.open_reader(id, 0).unwrap();
    let weak = std::sync::Arc::downgrade(&engine);
    engine.shutdown().await.unwrap();
    drop(engine);
    drop(reader);
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("reader pump must release its engine after reader drop");
}

#[tokio::test]
async fn restored_segmented_reader_stops_at_the_first_hole() {
    use gosh_dl::storage::{MemoryStorage, Segment, Storage};
    let dir = TempDir::new().unwrap();
    let original = engine(&dir).await;
    let id = original
        .add_http("http://127.0.0.1:1/file", DownloadOptions::default())
        .await
        .unwrap();
    original.pause(id).await.unwrap();
    let mut status = original.status(id).unwrap();
    original.shutdown().await.unwrap();
    status.progress.total_size = Some(8);
    status.progress.completed_size = 6;
    let storage = std::sync::Arc::new(MemoryStorage::new());
    storage.save_download(&status).await.unwrap();
    let mut first = Segment::new(0, 0, 3);
    first.downloaded = 2;
    let mut second = Segment::new(1, 4, 7);
    second.downloaded = 4;
    storage.save_segments(id, &[first, second]).await.unwrap();
    tokio::fs::write(dir.path().join("file.part"), b"ab\0\0efgh")
        .await
        .unwrap();
    let engine = DownloadEngine::with_storage(
        EngineConfig {
            download_dir: dir.path().into(),
            ..Default::default()
        },
        storage,
    )
    .await
    .unwrap();
    let mut reader = engine.open_reader(id, 0).unwrap();
    let mut prefix = [0; 2];
    tokio::time::timeout(Duration::from_secs(1), reader.read_exact(&mut prefix))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&prefix, b"ab");
    let mut next = [0];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), reader.read(&mut next))
            .await
            .is_err(),
        "must not expose preallocated holes as downloaded bytes"
    );
    drop(reader);
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn start_paused_makes_no_requests_and_resumes_after_restart() {
    use std::sync::Arc;
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("queued"))
        .mount(&server)
        .await;
    let storage = Arc::new(gosh_dl::MemoryStorage::new());
    let config = EngineConfig {
        download_dir: dir.path().into(),
        ..Default::default()
    };
    let engine = DownloadEngine::with_storage(config.clone(), storage.clone())
        .await
        .unwrap();
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            engine
                .add_http(
                    &format!("{}/file-{i}", server.uri()),
                    DownloadOptions {
                        start_paused: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(engine
        .list()
        .iter()
        .all(|s| s.state == DownloadState::Paused));
    engine.shutdown().await.unwrap();
    drop(engine);
    let engine = DownloadEngine::with_storage(config, storage).await.unwrap();
    engine.resume(ids[0]).await.unwrap();
    completed(&engine, ids[0]).await;
    assert_eq!(
        tokio::fs::read(dir.path().join("file-0")).await.unwrap(),
        b"queued"
    );
    assert!(ids[1..]
        .iter()
        .all(|id| engine.status(*id).unwrap().state == DownloadState::Paused));
    engine.shutdown().await.unwrap();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn start_paused_mirror_discovers_without_transferring_children() {
    use wiremock::matchers::path;
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string("<a href=\"one.bin\">one</a><a href=\"two.bin\">two</a>"),
        )
        .mount(&server)
        .await;
    let engine = engine(&dir).await;
    let job = engine
        .add_http_recursive(
            &format!("{}/files/", server.uri()),
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(job.child_ids.len(), 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(engine
        .list()
        .iter()
        .all(|s| s.state == DownloadState::Paused));
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.url.path() == "/files/"));
    engine.shutdown().await.unwrap();
}
