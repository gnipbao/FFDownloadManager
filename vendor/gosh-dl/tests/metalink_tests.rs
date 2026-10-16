#![cfg(all(feature = "metalink", feature = "http"))]
//! End-to-end tests for Metalink (RFC 5854) support.
//!
//! Serves files from a wiremock server, feeds the engine a metalink document
//! pointing at them (including a dead first mirror to exercise failover), and
//! verifies both downloads complete with the right contents.

use gosh_dl::{DownloadEngine, DownloadEvent, DownloadOptions, EngineConfig};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Helper to create a test engine with a temp directory.
async fn create_test_engine(temp_dir: &TempDir) -> std::sync::Arc<DownloadEngine> {
    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_concurrent_downloads: 4,
        max_connections_per_download: 4,
        min_segment_size: 1024 * 1024, // 1MB
        ..Default::default()
    };
    DownloadEngine::new(config)
        .await
        .expect("Failed to create engine")
}

/// Helper to wait for a specific event type.
async fn wait_for_event<F>(
    rx: &mut broadcast::Receiver<DownloadEvent>,
    predicate: F,
    timeout_duration: Duration,
) -> Option<DownloadEvent>
where
    F: Fn(&DownloadEvent) -> bool,
{
    let result = timeout(timeout_duration, async {
        loop {
            match rx.recv().await {
                Ok(event) if predicate(&event) => return Some(event),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await;
    result.unwrap_or(None)
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Mount HEAD + GET mocks serving `body` at `route` (the engine probes with
/// HEAD before downloading, so both must succeed).
async fn mount_file(server: &MockServer, route: &str, body: &[u8]) {
    Mock::given(method("HEAD"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .set_body_bytes(body.to_vec()),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_metalink_end_to_end_with_dead_mirror_failover() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content_one: &[u8] = b"Metalink file one contents";
    let content_two: &[u8] = b"Metalink file two contents, served by the backup mirror";

    mount_file(&mock_server, "/file1.bin", content_one).await;
    mount_file(&mock_server, "/file2.bin", content_two).await;

    // No mock is mounted for /dead/file2.bin, so wiremock answers 404 —
    // a dead first mirror that must fail over to the working one.
    let uri = mock_server.uri();
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="file1.bin">
    <size>{size_one}</size>
    <hash type="sha-256">{sha_one}</hash>
    <url priority="1">{uri}/file1.bin</url>
  </file>
  <file name="file2.bin">
    <size>{size_two}</size>
    <hash type="sha-256">{sha_two}</hash>
    <url priority="1">{uri}/dead/file2.bin</url>
    <url priority="2">{uri}/file2.bin</url>
  </file>
</metalink>"#,
        size_one = content_one.len(),
        sha_one = sha256_hex(content_one),
        size_two = content_two.len(),
        sha_two = sha256_hex(content_two),
        uri = uri,
    );

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let ids = engine
        .add_metalink(xml.as_bytes(), DownloadOptions::default())
        .await
        .expect("Failed to add metalink downloads");
    assert_eq!(ids.len(), 2, "Both metalink files should be added");

    // Wait for both downloads to complete (failover on file2 included).
    let mut pending: HashSet<_> = ids.iter().copied().collect();
    while !pending.is_empty() {
        let event = wait_for_event(
            &mut events,
            |e| {
                matches!(e, DownloadEvent::Completed { id } if pending.contains(id))
                    || matches!(e, DownloadEvent::Failed { id, .. } if pending.contains(id))
            },
            Duration::from_secs(30),
        )
        .await
        .expect("Both metalink downloads should complete");
        match event {
            DownloadEvent::Completed { id } => {
                pending.remove(&id);
            }
            DownloadEvent::Failed { id, error, .. } => {
                panic!("Download {} failed instead of completing: {}", id, error);
            }
            _ => unreachable!(),
        }
    }

    // Verify contents (checksum verification already ran inside the engine —
    // a mismatch would have failed the download instead of completing it).
    let file_one = tokio::fs::read(temp_dir.path().join("file1.bin"))
        .await
        .expect("file1.bin should exist");
    assert_eq!(file_one, content_one, "file1.bin content should match");

    let file_two = tokio::fs::read(temp_dir.path().join("file2.bin"))
        .await
        .expect("file2.bin should exist");
    assert_eq!(file_two, content_two, "file2.bin content should match");

    engine.shutdown().await.ok();
}

#[test]
fn rejects_unclosed_or_multiple_metalink_roots() {
    for xml in [
        "<metalink>",
        "<metalink><file name=\"file\"/>",
        "<metalink/><metalink/>",
        "<metalink></metalink><file name=\"file\"/>",
    ] {
        assert!(
            gosh_dl::metalink::parse_metalink(xml.as_bytes()).is_err(),
            "accepted {xml}"
        );
    }
}

#[tokio::test]
async fn unsupported_transport_does_not_hide_an_http_mirror() {
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    mount_file(&server, "/file.bin", b"payload").await;
    let engine = create_test_engine(&dir).await;
    let mut events = engine.subscribe();
    let xml = format!("<metalink><file name=\"file.bin\"><url priority=\"1\">ftp://example.com/file.bin</url><url priority=\"2\">{}/file.bin</url></file></metalink>", server.uri());
    let ids = engine
        .add_metalink(xml.as_bytes(), DownloadOptions::default())
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id } if *id == ids[0]),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert_eq!(
        tokio::fs::read(dir.path().join("file.bin")).await.unwrap(),
        b"payload"
    );
    engine.shutdown().await.unwrap();
}
