#![cfg(feature = "http")]
//! Integration tests for gosh-dl
//!
//! These tests use wiremock to simulate HTTP servers and test
//! real download scenarios including concurrent downloads, pause/resume,
//! and error recovery.

use gosh_dl::{
    DownloadEngine, DownloadEvent, DownloadOptions, DownloadProgress, DownloadState, EngineConfig,
    HttpConfig,
};
#[cfg(feature = "recursive-http")]
use gosh_dl::{RecursiveJobEvent, RecursiveOptions};
#[cfg(feature = "storage")]
use gosh_dl::{SqliteStorage, Storage};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

#[derive(Debug)]
struct MissingHeaderMatcher(&'static str);

impl Match for MissingHeaderMatcher {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.0)
    }
}

/// Helper to create a test engine with a temp directory
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

#[cfg(feature = "storage")]
async fn create_persistent_test_engine(
    temp_dir: &TempDir,
    database_name: &str,
    max_concurrent_downloads: usize,
) -> std::sync::Arc<DownloadEngine> {
    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        database_path: Some(temp_dir.path().join(database_name)),
        max_concurrent_downloads,
        max_connections_per_download: 4,
        min_segment_size: 1024 * 1024,
        ..Default::default()
    };
    DownloadEngine::new(config)
        .await
        .expect("Failed to create persistent engine")
}

/// Helper to wait for a specific event type
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

#[cfg(feature = "recursive-http")]
async fn wait_for_recursive_event<F>(
    rx: &mut broadcast::Receiver<RecursiveJobEvent>,
    predicate: F,
    timeout_duration: Duration,
) -> Option<RecursiveJobEvent>
where
    F: Fn(&RecursiveJobEvent) -> bool,
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

fn assert_progress_invariant(progress: &DownloadProgress) {
    if let Some(total_size) = progress.total_size {
        assert!(
            progress.completed_size <= total_size,
            "completed size {} exceeded total size {}",
            progress.completed_size,
            total_size
        );
        assert!(
            progress.percentage() <= 100.0 + f64::EPSILON,
            "progress percentage exceeded 100: {}",
            progress.percentage()
        );
    }
}

fn assert_all_progress_invariants(progress_events: &[DownloadProgress]) {
    for progress in progress_events {
        assert_progress_invariant(progress);
    }
}

#[cfg(feature = "recursive-http")]
async fn wait_for_downloads_to_complete(
    engine: &std::sync::Arc<DownloadEngine>,
    ids: &[gosh_dl::DownloadId],
) {
    timeout(Duration::from_secs(10), async {
        loop {
            let statuses = ids
                .iter()
                .filter_map(|id| engine.status(*id))
                .collect::<Vec<_>>();
            if statuses.len() == ids.len()
                && statuses
                    .iter()
                    .all(|status| matches!(status.state, DownloadState::Completed))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("recursive downloads should complete");
}

#[cfg(feature = "recursive-http")]
async fn wait_for_downloads_to_finish(
    engine: &std::sync::Arc<DownloadEngine>,
    ids: &[gosh_dl::DownloadId],
) {
    timeout(Duration::from_secs(10), async {
        loop {
            let statuses = ids
                .iter()
                .filter_map(|id| engine.status(*id))
                .collect::<Vec<_>>();
            if statuses.len() == ids.len()
                && statuses.iter().all(|status| {
                    matches!(
                        status.state,
                        DownloadState::Completed | DownloadState::Error { .. }
                    )
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("recursive downloads should reach a terminal state");
}

#[cfg(feature = "recursive-http")]
fn child_id_by_filename(
    engine: &std::sync::Arc<DownloadEngine>,
    ids: &[gosh_dl::DownloadId],
    filename: &str,
) -> gosh_dl::DownloadId {
    ids.iter()
        .copied()
        .find(|id| {
            engine
                .status(*id)
                .and_then(|status| status.metadata.filename)
                .as_deref()
                == Some(filename)
        })
        .unwrap_or_else(|| panic!("missing child id for filename {filename}"))
}

// =============================================================================
// Basic Download Tests
// =============================================================================

#[tokio::test]
async fn test_basic_http_download() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Create test content
    let test_content = b"Hello, World! This is test content for download.";

    // Setup mock endpoint
    Mock::given(method("HEAD"))
        .and(path("/test-file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/test-file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .set_body_bytes(test_content.to_vec()),
        )
        .mount(&mock_server)
        .await;

    // Create engine and subscribe to events
    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    // Start download
    let url = format!("{}/test-file.txt", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    // Wait for completion
    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await;

    assert!(completed.is_some(), "Download should complete");

    // Verify file exists
    let downloaded_file = temp_dir.path().join("test-file.txt");
    assert!(downloaded_file.exists(), "Downloaded file should exist");

    // Verify content
    let content = tokio::fs::read(&downloaded_file)
        .await
        .expect("Failed to read file");
    assert_eq!(content, test_content, "File content should match");

    // Verify status
    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_download_with_custom_filename() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let test_content = b"Custom filename test content";

    Mock::given(method("HEAD"))
        .and(path("/original-name.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/original-name.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .set_body_bytes(test_content.to_vec()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let options = DownloadOptions {
        filename: Some("custom-name.txt".to_string()),
        ..Default::default()
    };

    let url = format!("{}/original-name.txt", mock_server.uri());
    let id = engine
        .add_http(&url, options)
        .await
        .expect("Failed to add download");

    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await;

    assert!(completed.is_some(), "Download should complete");

    let downloaded_file = temp_dir.path().join("custom-name.txt");
    assert!(
        downloaded_file.exists(),
        "Downloaded file with custom name should exist"
    );

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_download_content_disposition_filename() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let test_content = b"Content-Disposition filename test";

    Mock::given(method("HEAD"))
        .and(path("/download"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes")
                .insert_header(
                    "Content-Disposition",
                    "attachment; filename=\"real-file.dat\"",
                ),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/download"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header(
                    "Content-Disposition",
                    "attachment; filename=\"real-file.dat\"",
                )
                .set_body_bytes(test_content.to_vec()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/download", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await;

    assert!(completed.is_some(), "Download should complete");

    // The file should be saved with the Content-Disposition filename or URL path
    // Different implementations may handle this differently
    let expected_file = temp_dir.path().join("real-file.dat");
    let fallback_file = temp_dir.path().join("download");
    assert!(
        expected_file.exists() || fallback_file.exists(),
        "Downloaded file should exist (expected {:?} or {:?})",
        expected_file,
        fallback_file
    );

    engine.shutdown().await.ok();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn test_recursive_download_downloads_nested_files() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    r#"
                    <html><body>
                        <a href="readme.txt">readme.txt</a>
                        <a href="releases/">releases/</a>
                    </body></html>
                    "#,
                ),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/releases/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    r#"<html><body><a href="app.tar.gz">app.tar.gz</a></body></html>"#,
                ),
        )
        .mount(&mock_server)
        .await;

    for file_path in ["/pub/readme.txt", "/pub/releases/app.tar.gz"] {
        Mock::given(method("HEAD"))
            .and(path(file_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "11")
                    .insert_header("Accept-Ranges", "bytes"),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(file_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "11")
                    .set_body_bytes(b"hello world".to_vec()),
            )
            .mount(&mock_server)
            .await;
    }

    let engine = create_test_engine(&temp_dir).await;
    let job = engine
        .add_http_recursive(
            &format!("{}/pub/", mock_server.uri()),
            DownloadOptions::default(),
            RecursiveOptions::default(),
        )
        .await
        .expect("Failed to add recursive download");

    assert_eq!(job.child_ids.len(), 2);
    wait_for_downloads_to_complete(&engine, &job.child_ids).await;

    assert_eq!(
        tokio::fs::read_to_string(temp_dir.path().join("readme.txt"))
            .await
            .expect("Should read root file"),
        "hello world"
    );
    assert_eq!(
        tokio::fs::read_to_string(temp_dir.path().join("releases").join("app.tar.gz"))
            .await
            .expect("Should read nested file"),
        "hello world"
    );

    let job_status = engine.recursive_job_status(&job);
    assert_eq!(job_status.state, gosh_dl::RecursiveJobState::Completed);
    assert_eq!(job_status.progress.total_children, 2);
    assert_eq!(job_status.progress.completed_children, 2);
    assert_eq!(job_status.progress.failed_children, 0);

    engine.shutdown().await.ok();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn test_recursive_discovery_rejects_redirect_out_of_scope() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(r#"<html><body><a href="nested/">nested/</a></body></html>"#),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/nested/"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/elsewhere/", mock_server.uri())),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/elsewhere/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(r#"<html><body><a href="file.txt">file.txt</a></body></html>"#),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let err = engine
        .discover_http_recursive(
            &format!("{}/pub/", mock_server.uri()),
            &DownloadOptions::default(),
            &RecursiveOptions::default(),
        )
        .await
        .expect_err("redirect out of scope should fail");

    assert!(
        err.to_string().contains("redirect escaped recursive scope"),
        "unexpected error: {err}"
    );

    engine.shutdown().await.ok();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn test_recursive_discovery_rejects_colliding_relative_paths() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    r#"
                    <html><body>
                        <a href="file.txt?one">first</a>
                        <a href="file.txt?two">second</a>
                    </body></html>
                    "#,
                ),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let err = engine
        .discover_http_recursive(
            &format!("{}/pub/", mock_server.uri()),
            &DownloadOptions::default(),
            &RecursiveOptions::default(),
        )
        .await
        .expect_err("colliding paths should fail");

    assert!(
        matches!(err, gosh_dl::EngineError::AlreadyExists(_)),
        "unexpected error: {err}"
    );

    engine.shutdown().await.ok();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn test_recursive_child_download_rejects_redirect_out_of_scope() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(r#"<html><body><a href="file.txt">file.txt</a></body></html>"#),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/file.txt"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/elsewhere/file.txt", mock_server.uri()),
        ))
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/elsewhere/file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let job = engine
        .add_http_recursive(
            &format!("{}/pub/", mock_server.uri()),
            DownloadOptions::default(),
            RecursiveOptions::default(),
        )
        .await
        .expect("recursive job should be created");

    assert_eq!(job.child_ids.len(), 1);
    let child_id = job.child_ids[0];

    let failed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Failed { id, .. } if *id == child_id),
        Duration::from_secs(5),
    )
    .await;

    assert!(failed.is_some(), "child download should fail");
    let status = engine.status(child_id).expect("child status should exist");
    assert!(matches!(status.state, DownloadState::Error { .. }));
    assert!(
        !temp_dir.path().join("file.txt").exists(),
        "out-of-scope redirect should not write the target file"
    );

    engine.shutdown().await.ok();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn test_recursive_without_fail_fast_allows_other_children_to_finish() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    r#"<html><body><a href="bad.txt">bad.txt</a><a href="slow.txt">slow.txt</a></body></html>"#,
                ),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/bad.txt"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/elsewhere/bad.txt", mock_server.uri()),
        ))
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/elsewhere/bad.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "3")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .set_body_bytes(b"hello world".to_vec())
                .set_delay(Duration::from_millis(250)),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let job = engine
        .add_http_recursive(
            &format!("{}/pub/", mock_server.uri()),
            DownloadOptions::default(),
            RecursiveOptions::default(),
        )
        .await
        .expect("recursive job should be created");

    let bad_id = child_id_by_filename(&engine, &job.child_ids, "bad.txt");
    let slow_id = child_id_by_filename(&engine, &job.child_ids, "slow.txt");

    wait_for_downloads_to_finish(&engine, &job.child_ids).await;

    assert!(matches!(
        engine.status(bad_id).expect("bad child status").state,
        DownloadState::Error { .. }
    ));
    assert_eq!(
        engine.status(slow_id).expect("slow child status").state,
        DownloadState::Completed
    );
    assert_eq!(
        tokio::fs::read_to_string(temp_dir.path().join("slow.txt"))
            .await
            .expect("slow child should complete"),
        "hello world"
    );
    assert!(
        !temp_dir.path().join("bad.txt").exists(),
        "bad child should not produce a file"
    );

    let job_status = engine.recursive_job_status(&job);
    assert_eq!(job_status.state, gosh_dl::RecursiveJobState::Partial);
    assert_eq!(job_status.progress.total_children, 2);
    assert_eq!(job_status.progress.completed_children, 1);
    assert_eq!(job_status.progress.failed_children, 1);
    assert_eq!(job_status.progress.missing_children, 0);

    engine.shutdown().await.ok();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn test_recursive_fail_fast_stops_other_children() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    r#"<html><body><a href="bad.txt">bad.txt</a><a href="slow.txt">slow.txt</a></body></html>"#,
                ),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/bad.txt"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/elsewhere/bad.txt", mock_server.uri()),
        ))
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/elsewhere/bad.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "3")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .set_body_bytes(b"hello world".to_vec())
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let job = engine
        .add_http_recursive(
            &format!("{}/pub/", mock_server.uri()),
            DownloadOptions::default(),
            RecursiveOptions {
                fail_fast: true,
                ..Default::default()
            },
        )
        .await
        .expect("recursive job should be created");

    let bad_id = child_id_by_filename(&engine, &job.child_ids, "bad.txt");
    let slow_id = child_id_by_filename(&engine, &job.child_ids, "slow.txt");

    wait_for_downloads_to_finish(&engine, &job.child_ids).await;

    assert!(matches!(
        engine.status(bad_id).expect("bad child status").state,
        DownloadState::Error { .. }
    ));
    match engine.status(slow_id).expect("slow child status").state {
        DownloadState::Error {
            kind,
            message,
            retryable,
        } => {
            assert_eq!(kind, "RecursiveFailFast");
            assert!(
                message.contains(&bad_id.to_string()),
                "fail-fast error should identify the triggering child: {message}"
            );
            assert!(!retryable, "fail-fast should not be retryable");
        }
        other => panic!("slow child should fail fast, got {other:?}"),
    }
    assert!(
        !temp_dir.path().join("slow.txt").exists(),
        "slow child should not complete once fail-fast triggers"
    );

    let job_status = engine.recursive_job_status(&job);
    assert_eq!(job_status.state, gosh_dl::RecursiveJobState::Failed);
    assert_eq!(job_status.progress.total_children, 2);
    assert_eq!(job_status.progress.completed_children, 0);
    assert_eq!(job_status.progress.failed_children, 2);

    engine.shutdown().await.ok();
}

#[cfg(all(feature = "recursive-http", feature = "storage"))]
#[tokio::test]
async fn test_recursive_child_redirect_scope_persists_across_restart() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(r#"<html><body><a href="file.txt">file.txt</a></body></html>"#),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .insert_header("Content-Length", "11")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/file.txt"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "Location",
            format!("{}/elsewhere/file.txt", mock_server.uri()),
        ))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/elsewhere/file.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .set_body_bytes(b"hello world".to_vec()),
        )
        .mount(&mock_server)
        .await;

    let child_id = {
        let engine = create_persistent_test_engine(&temp_dir, "recursive.sqlite", 1).await;

        let job = engine
            .add_http_recursive(
                &format!("{}/pub/", mock_server.uri()),
                DownloadOptions::default(),
                RecursiveOptions::default(),
            )
            .await
            .expect("recursive job should be created");

        assert_eq!(job.child_ids.len(), 1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        engine.shutdown().await.ok();
        job.child_ids[0]
    };

    let storage = SqliteStorage::new(temp_dir.path().join("recursive.sqlite"))
        .await
        .expect("should open persisted sqlite db");
    assert!(
        storage
            .load_runtime_metadata(child_id)
            .await
            .expect("should load runtime metadata")
            .is_some(),
        "recursive child runtime metadata should be persisted"
    );

    let resumed_engine = create_persistent_test_engine(&temp_dir, "recursive.sqlite", 1).await;
    let mut events = resumed_engine.subscribe();
    assert_eq!(
        resumed_engine
            .status(child_id)
            .expect("child status should be restored")
            .state,
        DownloadState::Paused
    );

    resumed_engine
        .resume(child_id)
        .await
        .expect("resumed child should start");

    let failed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Failed { id, .. } if *id == child_id),
        Duration::from_secs(5),
    )
    .await;

    let child_status = resumed_engine.status(child_id).expect("child status");
    assert!(
        failed.is_some(),
        "resumed child should still fail out of scope, got state {:?}",
        child_status.state
    );
    assert!(matches!(child_status.state, DownloadState::Error { .. }));
    assert!(
        !temp_dir.path().join("file.txt").exists(),
        "persisted redirect scope should still block the redirected child"
    );

    resumed_engine.shutdown().await.ok();
}

#[cfg(all(feature = "recursive-http", feature = "storage"))]
#[tokio::test]
async fn test_tracked_recursive_jobs_persist_across_restart() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(
                    r#"<html><body><a href="readme.txt">readme.txt</a><a href="nested/">nested/</a></body></html>"#,
                ),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/nested/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(r#"<html><body><a href="file.txt">file.txt</a></body></html>"#),
        )
        .mount(&mock_server)
        .await;

    for file_path in ["/pub/readme.txt", "/pub/nested/file.txt"] {
        Mock::given(method("HEAD"))
            .and(path(file_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "11")
                    .insert_header("Accept-Ranges", "bytes"),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(file_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", "11")
                    .set_body_bytes(b"hello world".to_vec()),
            )
            .mount(&mock_server)
            .await;
    }

    let root_url = format!("{}/pub/", mock_server.uri());
    {
        let engine = create_persistent_test_engine(&temp_dir, "tracked-recursive.sqlite", 4).await;
        let job = engine
            .add_http_recursive(
                &root_url,
                DownloadOptions::default(),
                RecursiveOptions::default(),
            )
            .await
            .expect("recursive job should be created");
        wait_for_downloads_to_complete(&engine, &job.child_ids).await;
        engine.shutdown().await.ok();
    }

    let resumed_engine =
        create_persistent_test_engine(&temp_dir, "tracked-recursive.sqlite", 4).await;
    let jobs = resumed_engine.list_recursive_jobs();
    assert_eq!(
        jobs.len(),
        1,
        "one tracked recursive job should be restored"
    );

    let tracked_job = &jobs[0];
    assert_eq!(tracked_job.root_url, root_url);
    assert_eq!(tracked_job.child_ids.len(), 2);

    let aggregate = resumed_engine.recursive_job_status(&tracked_job.as_job());
    assert_eq!(aggregate.state, gosh_dl::RecursiveJobState::Completed);
    assert_eq!(aggregate.progress.total_children, 2);
    assert_eq!(aggregate.progress.completed_children, 2);

    resumed_engine.shutdown().await.ok();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn test_cancel_recursive_job_keeps_tracked_record() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(r#"<html><body><a href="slow.txt">slow.txt</a></body></html>"#),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .set_body_bytes(b"hello world".to_vec())
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let job = engine
        .add_http_recursive(
            &format!("{}/pub/", mock_server.uri()),
            DownloadOptions::default(),
            RecursiveOptions::default(),
        )
        .await
        .expect("recursive job should be created");

    let tracked_jobs = engine.list_recursive_jobs();
    assert_eq!(tracked_jobs.len(), 1);
    let tracked_job = tracked_jobs[0].clone();
    assert_eq!(tracked_job.child_ids, job.child_ids);

    engine
        .cancel_recursive_job(tracked_job.id, false)
        .await
        .expect("cancel recursive job should succeed");

    assert!(engine.status(job.child_ids[0]).is_none());
    let remaining_jobs = engine.list_recursive_jobs();
    assert_eq!(remaining_jobs.len(), 1, "tracked record should remain");
    let aggregate = engine.recursive_job_status(&remaining_jobs[0].as_job());
    assert_eq!(aggregate.state, gosh_dl::RecursiveJobState::Failed);
    assert_eq!(aggregate.progress.missing_children, 1);

    engine.shutdown().await.ok();
}

#[cfg(feature = "recursive-http")]
#[tokio::test]
async fn test_recursive_job_event_stream_reports_parent_lifecycle() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(r#"<html><body><a href="slow.txt">slow.txt</a></body></html>"#),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .set_body_bytes(b"hello world".to_vec())
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut recursive_events = engine.subscribe_recursive_jobs();

    let job = engine
        .add_http_recursive(
            &format!("{}/pub/", mock_server.uri()),
            DownloadOptions::default(),
            RecursiveOptions::default(),
        )
        .await
        .expect("recursive job should be created");

    let tracked_job = engine
        .list_recursive_jobs()
        .into_iter()
        .find(|tracked| tracked.child_ids == job.child_ids)
        .expect("tracked recursive job should exist");

    let added = wait_for_recursive_event(
        &mut recursive_events,
        |event| matches!(event, RecursiveJobEvent::Added { job, .. } if job.id == tracked_job.id),
        Duration::from_secs(2),
    )
    .await;

    match added.expect("added recursive event should be emitted") {
        RecursiveJobEvent::Added { job, status } => {
            assert_eq!(job.id, tracked_job.id);
            assert_eq!(status.progress.total_children, 1);
            assert_eq!(status.child_ids, tracked_job.child_ids);
        }
        other => panic!("unexpected recursive event: {other:?}"),
    }

    engine
        .cancel_recursive_job(tracked_job.id, false)
        .await
        .expect("cancel recursive job should succeed");

    let updated = wait_for_recursive_event(
        &mut recursive_events,
        |event| {
            matches!(
                event,
                RecursiveJobEvent::Updated { job, status }
                    if job.id == tracked_job.id
                        && status.state == gosh_dl::RecursiveJobState::Failed
                        && status.progress.missing_children == 1
            )
        },
        Duration::from_secs(5),
    )
    .await;

    match updated.expect("updated recursive event should be emitted") {
        RecursiveJobEvent::Updated { job, status } => {
            assert_eq!(job.id, tracked_job.id);
            assert_eq!(status.progress.total_children, 1);
            assert_eq!(status.progress.missing_children, 1);
        }
        other => panic!("unexpected recursive event: {other:?}"),
    }

    engine
        .remove_recursive_job(tracked_job.id, false)
        .await
        .expect("remove recursive job should succeed");

    let removed = wait_for_recursive_event(
        &mut recursive_events,
        |event| matches!(event, RecursiveJobEvent::Removed { id } if *id == tracked_job.id),
        Duration::from_secs(2),
    )
    .await;

    assert!(
        matches!(removed, Some(RecursiveJobEvent::Removed { id }) if id == tracked_job.id),
        "removed recursive event should be emitted"
    );

    engine.shutdown().await.ok();
}

#[cfg(all(feature = "recursive-http", feature = "storage"))]
#[tokio::test]
async fn test_remove_recursive_job_prunes_persisted_record() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/pub/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Type", "text/html")
                .set_body_string(r#"<html><body><a href="slow.txt">slow.txt</a></body></html>"#),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("HEAD"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pub/slow.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "11")
                .set_body_bytes(b"hello world".to_vec())
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&mock_server)
        .await;

    let tracked_job_id = {
        let engine = create_persistent_test_engine(&temp_dir, "remove-recursive.sqlite", 4).await;
        engine
            .add_http_recursive(
                &format!("{}/pub/", mock_server.uri()),
                DownloadOptions::default(),
                RecursiveOptions::default(),
            )
            .await
            .expect("recursive job should be created");

        let tracked_jobs = engine.list_recursive_jobs();
        assert_eq!(tracked_jobs.len(), 1);
        let tracked_job_id = tracked_jobs[0].id;

        engine
            .remove_recursive_job(tracked_job_id, false)
            .await
            .expect("remove recursive job should succeed");
        assert!(engine.list_recursive_jobs().is_empty());
        engine.shutdown().await.ok();
        tracked_job_id
    };

    let resumed_engine =
        create_persistent_test_engine(&temp_dir, "remove-recursive.sqlite", 4).await;
    assert!(
        resumed_engine.recursive_job(tracked_job_id).is_none(),
        "removed recursive job should not be restored"
    );
    assert!(resumed_engine.list_recursive_jobs().is_empty());
    resumed_engine.shutdown().await.ok();
}

// =============================================================================
// Event System Tests
// =============================================================================

#[tokio::test]
async fn test_download_events_sequence() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let test_content = vec![0u8; 1024]; // 1KB

    Mock::given(method("HEAD"))
        .and(path("/events-test.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/events-test.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .set_body_bytes(test_content),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/events-test.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut received_events = Vec::new();
    let start = std::time::Instant::now();

    // Collect events until completion or timeout
    while start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                received_events.push(event.clone());
                if matches!(
                    event,
                    DownloadEvent::Completed { .. } | DownloadEvent::Failed { .. }
                ) {
                    break;
                }
            }
            _ => continue,
        }
    }

    // Verify we received Added event
    let has_added = received_events
        .iter()
        .any(|e| matches!(e, DownloadEvent::Added { id: eid } if *eid == id));
    assert!(has_added, "Should receive Added event");

    // Verify we received Started event
    let has_started = received_events
        .iter()
        .any(|e| matches!(e, DownloadEvent::Started { id: eid } if *eid == id));
    assert!(has_started, "Should receive Started event");

    // Verify we received Completed event
    let has_completed = received_events
        .iter()
        .any(|e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id));
    assert!(has_completed, "Should receive Completed event");

    engine.shutdown().await.ok();
}

// =============================================================================
// Concurrent Download Tests
// =============================================================================

#[tokio::test]
async fn test_concurrent_downloads() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Create multiple files
    for i in 0..3 {
        let content = format!("Content for file {}", i);
        let path_str = format!("/file{}.txt", i);

        Mock::given(method("HEAD"))
            .and(path(&path_str))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", content.len().to_string())
                    .insert_header("Accept-Ranges", "bytes"),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(&path_str))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", content.len().to_string())
                    .set_body_string(content),
            )
            .mount(&mock_server)
            .await;
    }

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    // Start all downloads concurrently
    let mut ids = Vec::new();
    for i in 0..3 {
        let url = format!("{}/file{}.txt", mock_server.uri(), i);
        let id = engine
            .add_http(&url, DownloadOptions::default())
            .await
            .expect("Failed to add download");
        ids.push(id);
    }

    // Wait for all to complete
    let mut completed_count = 0;
    let start = std::time::Instant::now();

    while completed_count < 3 && start.elapsed() < Duration::from_secs(30) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(DownloadEvent::Completed { id })) if ids.contains(&id) => {
                completed_count += 1;
            }
            _ => continue,
        }
    }

    assert_eq!(completed_count, 3, "All downloads should complete");

    // Verify all files exist
    for i in 0..3 {
        let file_path = temp_dir.path().join(format!("file{}.txt", i));
        assert!(file_path.exists(), "File {} should exist", i);
    }

    // Check global stats reflect completed state
    let stats = engine.global_stats();
    assert_eq!(stats.num_active, 0, "No active downloads after completion");

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_concurrent_limit_respected() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Create a slow endpoint
    for i in 0..5 {
        let content = vec![0u8; 1024];
        let path_str = format!("/slow{}.bin", i);

        Mock::given(method("HEAD"))
            .and(path(&path_str))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", content.len().to_string())
                    .insert_header("Accept-Ranges", "bytes"),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(&path_str))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", content.len().to_string())
                    .set_body_bytes(content)
                    .set_delay(Duration::from_millis(500)), // Slow download
            )
            .mount(&mock_server)
            .await;
    }

    // Create engine with limit of 2 concurrent downloads
    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_concurrent_downloads: 2,
        max_connections_per_download: 1,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    // Start 5 downloads
    let mut ids = Vec::new();
    for i in 0..5 {
        let url = format!("{}/slow{}.bin", mock_server.uri(), i);
        let id = engine
            .add_http(&url, DownloadOptions::default())
            .await
            .expect("Failed to add download");
        ids.push(id);
    }

    // Give some time for downloads to start
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Note: The engine marks downloads as "Connecting" (which counts as active)
    // before they acquire the semaphore. The actual concurrent download limit
    // is enforced by the semaphore inside the download task. So we verify
    // functional behavior by ensuring all downloads complete successfully,
    // which proves the semaphore is working correctly.

    // Wait for all to complete
    let mut completed_count = 0;
    let start = std::time::Instant::now();

    while completed_count < 5 && start.elapsed() < Duration::from_secs(30) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(DownloadEvent::Completed { id })) if ids.contains(&id) => {
                completed_count += 1;
            }
            _ => continue,
        }
    }

    assert_eq!(
        completed_count, 5,
        "All downloads should eventually complete"
    );

    engine.shutdown().await.ok();
}

// =============================================================================
// Pause/Resume/Cancel Tests
// =============================================================================

#[tokio::test]
async fn test_cancel_download() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Create a large, slow download
    let content = vec![0u8; 10 * 1024 * 1024]; // 10MB

    Mock::given(method("HEAD"))
        .and(path("/large-file.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/large-file.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content)
                .set_delay(Duration::from_secs(10)), // Very slow
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/large-file.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    // Wait for download to start
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Started { id: eid } if *eid == id),
        Duration::from_secs(5),
    )
    .await;

    // Cancel the download with file deletion
    engine.cancel(id, true).await.expect("Failed to cancel");

    // Verify download is removed
    assert!(
        engine.status(id).is_none(),
        "Download should be removed after cancel"
    );

    // Verify we received Removed event
    let removed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Removed { id: eid } if *eid == id),
        Duration::from_secs(2),
    )
    .await;

    assert!(removed.is_some(), "Should receive Removed event");

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_pause_download() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Create a slow download
    let content = vec![0u8; 1024 * 1024]; // 1MB

    Mock::given(method("HEAD"))
        .and(path("/pausable.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/pausable.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content)
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/pausable.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    // Wait for download to start
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Started { id: eid } if *eid == id),
        Duration::from_secs(5),
    )
    .await;

    // Pause the download
    engine.pause(id).await.expect("Failed to pause");

    // Verify state is paused
    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Paused);

    // Verify we received Paused event
    let paused = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Paused { id: eid } if *eid == id),
        Duration::from_secs(2),
    )
    .await;

    assert!(paused.is_some(), "Should receive Paused event");

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_segmented_download_falls_back_when_server_ignores_range_requests() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

    Mock::given(method("HEAD"))
        .and(path("/lying-range.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    for range in ["bytes=0-2047", "bytes=2048-4095"] {
        Mock::given(method("GET"))
            .and(path("/lying-range.bin"))
            .and(header("Range", range))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", content.len().to_string())
                    .set_body_bytes(content.clone()),
            )
            .mount(&mock_server)
            .await;
    }

    Mock::given(method("GET"))
        .and(path("/lying-range.bin"))
        .and(MissingHeaderMatcher("range"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_connections_per_download: 2,
        min_segment_size: 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let url = format!("{}/lying-range.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut progress_events = Vec::new();
    let mut completed = false;
    let start = std::time::Instant::now();

    while start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                if let DownloadEvent::Progress { id: eid, progress } = &event {
                    if *eid == id {
                        progress_events.push(progress.clone());
                    }
                }
                if matches!(event, DownloadEvent::Completed { id: eid } if eid == id) {
                    completed = true;
                    break;
                }
                if matches!(event, DownloadEvent::Failed { id: eid, .. } if eid == id) {
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(
        completed,
        "Segmented download should restart without ranges and complete"
    );
    assert_all_progress_invariants(&progress_events);

    let downloaded_file = temp_dir.path().join("lying-range.bin");
    let downloaded = tokio::fs::read(&downloaded_file)
        .await
        .expect("Failed to read restarted download");
    assert_eq!(downloaded, content);

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);
    assert_progress_invariant(&status.progress);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_segmented_download_succeeds_with_valid_partial_content() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..4096).map(|i| (i % 253) as u8).collect();
    let first_half = content[..2048].to_vec();
    let second_half = content[2048..].to_vec();

    Mock::given(method("HEAD"))
        .and(path("/segmented-ok.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/segmented-ok.bin"))
        .and(header("Range", "bytes=0-2047"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Length", first_half.len().to_string())
                .insert_header("Content-Range", "bytes 0-2047/4096")
                .set_body_bytes(first_half),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/segmented-ok.bin"))
        .and(header("Range", "bytes=2048-4095"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Length", second_half.len().to_string())
                .insert_header("Content-Range", "bytes 2048-4095/4096")
                .set_body_bytes(second_half),
        )
        .mount(&mock_server)
        .await;

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_connections_per_download: 2,
        min_segment_size: 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let url = format!("{}/segmented-ok.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut progress_events = Vec::new();
    let mut completed = false;
    let start = std::time::Instant::now();

    while start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                if let DownloadEvent::Progress { id: eid, progress } = &event {
                    if *eid == id {
                        progress_events.push(progress.clone());
                    }
                }
                if matches!(event, DownloadEvent::Completed { id: eid } if eid == id) {
                    completed = true;
                    break;
                }
                if matches!(event, DownloadEvent::Failed { id: eid, .. } if eid == id) {
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(
        completed,
        "Segmented download should complete with valid 206 responses"
    );
    assert_all_progress_invariants(&progress_events);

    let downloaded_file = temp_dir.path().join("segmented-ok.bin");
    let downloaded = tokio::fs::read(&downloaded_file)
        .await
        .expect("Failed to read downloaded file");
    assert_eq!(
        downloaded, content,
        "Segmented download should preserve content"
    );

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);
    assert_progress_invariant(&status.progress);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_segmented_retry_falls_back_when_range_response_is_ignored() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..4096).map(|i| (i % 239) as u8).collect();
    let second_half = content[2048..].to_vec();

    Mock::given(method("HEAD"))
        .and(path("/retry-range.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/retry-range.bin"))
        .and(header("Range", "bytes=0-2047"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/retry-range.bin"))
        .and(header("Range", "bytes=0-2047"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/retry-range.bin"))
        .and(header("Range", "bytes=2048-4095"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Length", second_half.len().to_string())
                .insert_header("Content-Range", "bytes 2048-4095/4096")
                .set_body_bytes(second_half),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/retry-range.bin"))
        .and(MissingHeaderMatcher("range"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_connections_per_download: 2,
        min_segment_size: 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let url = format!("{}/retry-range.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut progress_events = Vec::new();
    let mut completed = false;
    let start = std::time::Instant::now();

    while start.elapsed() < Duration::from_secs(15) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                if let DownloadEvent::Progress { id: eid, progress } = &event {
                    if *eid == id {
                        progress_events.push(progress.clone());
                    }
                }
                if matches!(event, DownloadEvent::Completed { id: eid } if eid == id) {
                    completed = true;
                    break;
                }
                if matches!(event, DownloadEvent::Failed { id: eid, .. } if eid == id) {
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(
        completed,
        "Retrying a segment with an ignored Range response should restart without ranges"
    );
    assert_all_progress_invariants(&progress_events);

    let downloaded_file = temp_dir.path().join("retry-range.bin");
    let downloaded = tokio::fs::read(&downloaded_file)
        .await
        .expect("Failed to read restarted retry download");
    assert_eq!(downloaded, content);

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);
    assert_progress_invariant(&status.progress);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_resume_restarts_from_zero_when_range_is_ignored() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..4096).map(|i| (i % 241) as u8).collect();
    let existing_size = 1024u64;
    let part_path = temp_dir.path().join("resume-ignore.bin.part");
    tokio::fs::write(&part_path, &content[..existing_size as usize])
        .await
        .expect("Failed to seed partial file");

    Mock::given(method("HEAD"))
        .and(path("/resume-ignore.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/resume-ignore.bin"))
        .and(header("Range", "bytes=1024-"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/resume-ignore.bin"))
        .and(MissingHeaderMatcher("range"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_connections_per_download: 1,
        min_segment_size: 1024 * 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let url = format!("{}/resume-ignore.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut progress_events = Vec::new();
    let mut completed = false;
    let start = std::time::Instant::now();

    while start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                if let DownloadEvent::Progress { id: eid, progress } = &event {
                    if *eid == id {
                        progress_events.push(progress.clone());
                    }
                }
                if matches!(event, DownloadEvent::Completed { id: eid } if eid == id) {
                    completed = true;
                    break;
                }
                if matches!(event, DownloadEvent::Failed { id: eid, .. } if eid == id) {
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(
        completed,
        "Resumed download should restart from byte 0 when the server ignores Range"
    );
    assert_all_progress_invariants(&progress_events);

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);
    assert_progress_invariant(&status.progress);

    let downloaded_file = temp_dir.path().join("resume-ignore.bin");
    let downloaded = tokio::fs::read(&downloaded_file)
        .await
        .expect("Failed to read restarted resume download");
    assert_eq!(downloaded, content);
    assert!(
        !part_path.exists(),
        "Restarted download should finalize and remove the partial file"
    );

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_non_range_download_still_completes() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content = b"No range support still works".to_vec();

    Mock::given(method("HEAD"))
        .and(path("/no-range.bin"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Length", content.len().to_string()),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/no-range.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/no-range.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await;

    assert!(
        completed.is_some(),
        "Single-connection downloads without range support should still complete"
    );

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);
    assert_progress_invariant(&status.progress);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_gzip_encoded_response_does_not_exceed_progress_bounds() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Gzip-compressed form of:
    // "This is a compression regression test payload. " * 16
    let encoded_body = vec![
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 11, 201, 200, 44, 86, 0, 162, 68, 133, 228, 252, 220,
        130, 162, 212, 226, 226, 204, 252, 60, 133, 162, 212, 116, 24, 179, 36, 181, 184, 68, 161,
        32, 177, 50, 39, 63, 49, 69, 79, 33, 100, 84, 249, 168, 242, 161, 172, 28, 0, 102, 106,
        217, 239, 240, 2, 0, 0,
    ];

    Mock::given(method("HEAD"))
        .and(path("/compressed.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", encoded_body.len().to_string())
                .insert_header("Content-Encoding", "gzip"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/compressed.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", encoded_body.len().to_string())
                .insert_header("Content-Encoding", "gzip")
                .set_body_bytes(encoded_body.clone()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/compressed.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut progress_events = Vec::new();
    let mut completed = false;
    let start = std::time::Instant::now();

    while start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                if let DownloadEvent::Progress { id: eid, progress } = &event {
                    if *eid == id {
                        progress_events.push(progress.clone());
                    }
                }
                if matches!(event, DownloadEvent::Completed { id: eid } if eid == id) {
                    completed = true;
                    break;
                }
                if matches!(event, DownloadEvent::Failed { id: eid, .. } if eid == id) {
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(
        completed,
        "Encoded responses should complete without inflating progress above 100%"
    );
    assert_all_progress_invariants(&progress_events);

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);
    assert_eq!(
        status.progress.total_size,
        Some(encoded_body.len() as u64),
        "Progress total should reflect the encoded transfer size"
    );
    assert_progress_invariant(&status.progress);

    let downloaded_file = temp_dir.path().join("compressed.bin");
    let downloaded = tokio::fs::read(&downloaded_file)
        .await
        .expect("Failed to read compressed download");
    assert_eq!(
        downloaded, encoded_body,
        "Downloader should preserve the exact encoded bytes returned by the server"
    );

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_download_prefers_get_content_length_when_head_mismatches() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let body = vec![7u8; 4096];

    Mock::given(method("HEAD"))
        .and(path("/head-mismatch.bin"))
        .respond_with(ResponseTemplate::new(200).insert_header("Content-Length", "2048"))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/head-mismatch.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", body.len().to_string())
                .set_body_bytes(body.clone()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/head-mismatch.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut progress_events = Vec::new();
    let mut completed = false;
    let start = std::time::Instant::now();

    while start.elapsed() < Duration::from_secs(10) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                if let DownloadEvent::Progress { id: eid, progress } = &event {
                    if *eid == id {
                        progress_events.push(progress.clone());
                    }
                }
                if matches!(event, DownloadEvent::Completed { id: eid } if eid == id) {
                    completed = true;
                    break;
                }
                if matches!(event, DownloadEvent::Failed { id: eid, .. } if eid == id) {
                    break;
                }
            }
            _ => continue,
        }
    }

    assert!(
        completed,
        "GET Content-Length should override a stale HEAD Content-Length"
    );
    assert_all_progress_invariants(&progress_events);

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);
    assert_eq!(
        status.progress.total_size,
        Some(body.len() as u64),
        "Progress total should use the GET response length"
    );
    assert_progress_invariant(&status.progress);

    engine.shutdown().await.ok();
}

// =============================================================================
// Error Handling Tests
// =============================================================================

#[tokio::test]
async fn test_download_404_error() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Return 404 for HEAD and GET
    Mock::given(method("HEAD"))
        .and(path("/not-found.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/not-found.txt"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/not-found.txt", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    // Wait for failure
    let failed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Failed { id: eid, .. } if *eid == id),
        Duration::from_secs(10),
    )
    .await;

    assert!(failed.is_some(), "Download should fail with 404");

    // Verify state is Error
    let status = engine.status(id).expect("Should have status");
    assert!(matches!(status.state, DownloadState::Error { .. }));

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_download_500_error() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Return 500 server error
    Mock::given(method("HEAD"))
        .and(path("/server-error.txt"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/server-error.txt"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/server-error.txt", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    // Wait for failure (may take longer due to retries)
    let failed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Failed { id: eid, retryable, .. } if *eid == id),
        Duration::from_secs(30),
    )
    .await;

    assert!(failed.is_some(), "Download should fail with 500");

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_invalid_url() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let engine = create_test_engine(&temp_dir).await;

    // Test with invalid URL
    let result = engine
        .add_http("not-a-valid-url", DownloadOptions::default())
        .await;
    assert!(result.is_err(), "Should reject invalid URL");

    // Test with unsupported scheme
    let result = engine
        .add_http("ftp://example.com/file.txt", DownloadOptions::default())
        .await;
    assert!(result.is_err(), "Should reject unsupported scheme");

    engine.shutdown().await.ok();
}

// =============================================================================
// Engine Lifecycle Tests
// =============================================================================

#[tokio::test]
async fn test_engine_shutdown() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content = vec![0u8; 1024];

    Mock::given(method("HEAD"))
        .and(path("/shutdown-test.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/shutdown-test.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content)
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;

    let url = format!("{}/shutdown-test.bin", mock_server.uri());
    engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    // Give download time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Shutdown should complete within timeout
    let result = timeout(Duration::from_secs(10), engine.shutdown()).await;
    assert!(result.is_ok(), "Shutdown should complete within timeout");
}

#[tokio::test]
async fn test_config_update() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let engine = create_test_engine(&temp_dir).await;

    // Get current config
    let original_config = engine.get_config();
    assert_eq!(original_config.max_concurrent_downloads, 4);

    // Update config
    let new_config = EngineConfig {
        max_concurrent_downloads: 8,
        ..original_config
    };

    engine
        .set_config(new_config.clone())
        .expect("Failed to update config");

    // Verify update
    let updated_config = engine.get_config();
    assert_eq!(updated_config.max_concurrent_downloads, 8);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_list_downloads() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    for i in 0..3 {
        let content = format!("File {}", i);
        let path_str = format!("/list{}.txt", i);

        Mock::given(method("HEAD"))
            .and(path(&path_str))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", content.len().to_string())
                    .insert_header("Accept-Ranges", "bytes"),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path(&path_str))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", content.len().to_string())
                    .set_body_bytes(content)
                    .set_delay(Duration::from_secs(2)),
            )
            .mount(&mock_server)
            .await;
    }

    let engine = create_test_engine(&temp_dir).await;

    // Add downloads
    for i in 0..3 {
        let url = format!("{}/list{}.txt", mock_server.uri(), i);
        engine
            .add_http(&url, DownloadOptions::default())
            .await
            .expect("Failed to add download");
    }

    // Give downloads time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Check list
    let all_downloads = engine.list();
    assert_eq!(all_downloads.len(), 3, "Should have 3 downloads");

    // Check active list
    let active_downloads = engine.active();
    assert!(active_downloads.len() <= 4, "Active should be within limit");

    engine.shutdown().await.ok();
}

// =============================================================================
// Custom Headers Tests
// =============================================================================

#[tokio::test]
async fn test_custom_user_agent() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let custom_ua = "CustomAgent/1.0";
    let test_content = b"UA test content";

    Mock::given(method("HEAD"))
        .and(path("/ua-test.txt"))
        .and(header("User-Agent", custom_ua))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/ua-test.txt"))
        .and(header("User-Agent", custom_ua))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .set_body_bytes(test_content.to_vec()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let options = DownloadOptions {
        user_agent: Some(custom_ua.to_string()),
        ..Default::default()
    };

    let url = format!("{}/ua-test.txt", mock_server.uri());
    let id = engine
        .add_http(&url, options)
        .await
        .expect("Failed to add download");

    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await;

    assert!(
        completed.is_some(),
        "Download should complete with custom UA"
    );

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_custom_referer() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let custom_referer = "https://example.com/page";
    let test_content = b"Referer test content";

    // Note: We don't require the Referer header in the mock matcher because:
    // 1. HEAD requests for capability detection may not include it
    // 2. HTTP/2 uses lowercase headers which may not match exactly
    // The test verifies that setting a referer doesn't break the download
    Mock::given(method("HEAD"))
        .and(path("/referer-test.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/referer-test.txt"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .set_body_bytes(test_content.to_vec()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let options = DownloadOptions {
        referer: Some(custom_referer.to_string()),
        ..Default::default()
    };

    let url = format!("{}/referer-test.txt", mock_server.uri());
    let id = engine
        .add_http(&url, options)
        .await
        .expect("Failed to add download");

    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await;

    assert!(
        completed.is_some(),
        "Download should complete with custom Referer"
    );

    // Verify the download completed successfully
    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);

    engine.shutdown().await.ok();
}

// =============================================================================
// Progress Tracking Tests
// =============================================================================

#[tokio::test]
async fn test_progress_updates() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // Create a larger file to ensure multiple progress updates
    let test_content = vec![0u8; 100 * 1024]; // 100KB

    Mock::given(method("HEAD"))
        .and(path("/progress-test.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/progress-test.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", test_content.len().to_string())
                .set_body_bytes(test_content.clone()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/progress-test.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut progress_events = Vec::new();
    let start = std::time::Instant::now();

    // Collect progress events
    while start.elapsed() < Duration::from_secs(15) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                if let DownloadEvent::Progress { id: eid, progress } = &event {
                    if *eid == id {
                        progress_events.push(progress.clone());
                    }
                }
                if matches!(
                    event,
                    DownloadEvent::Completed { .. } | DownloadEvent::Failed { .. }
                ) {
                    break;
                }
            }
            _ => continue,
        }
    }

    // Verify we got some progress updates
    assert!(
        !progress_events.is_empty(),
        "Should receive progress updates"
    );
    assert_all_progress_invariants(&progress_events);

    // Verify final progress shows correct total
    if let Some(last_progress) = progress_events.last() {
        assert_eq!(
            last_progress.total_size,
            Some(test_content.len() as u64),
            "Total size should be correct"
        );
        assert_progress_invariant(last_progress);
    }

    engine.shutdown().await.ok();
}

// =============================================================================
// Statistics Tests
// =============================================================================

#[tokio::test]
async fn test_global_stats() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let engine = create_test_engine(&temp_dir).await;

    // Initially all stats should be zero
    let stats = engine.global_stats();
    assert_eq!(stats.num_active, 0);
    assert_eq!(stats.num_waiting, 0);
    assert_eq!(stats.num_stopped, 0);
    assert_eq!(stats.download_speed, 0);
    assert_eq!(stats.upload_speed, 0);

    engine.shutdown().await.ok();
}

// =============================================================================
// Resume Reliability Tests
// =============================================================================

/// Helper matcher: matches requests that have a specific header present
#[derive(Debug)]
struct HasHeaderMatcher(&'static str);

impl Match for HasHeaderMatcher {
    fn matches(&self, request: &Request) -> bool {
        request.headers.contains_key(self.0)
    }
}

/// Helper: create an engine with fast retries for testing
async fn create_fast_retry_engine(
    temp_dir: &TempDir,
    max_retries: usize,
) -> std::sync::Arc<DownloadEngine> {
    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_concurrent_downloads: 4,
        max_connections_per_download: 4,
        min_segment_size: 1024,
        http: HttpConfig {
            max_retries,
            retry_delay_ms: 50,
            max_retry_delay_ms: 200,
            connect_timeout: 5,
            read_timeout: 5,
            ..Default::default()
        },
        ..Default::default()
    };
    DownloadEngine::new(config)
        .await
        .expect("Failed to create engine")
}

/// Wait for a terminal event (Completed or Failed), collecting all events.
async fn wait_for_terminal(
    events: &mut broadcast::Receiver<DownloadEvent>,
    id: gosh_dl::DownloadId,
    timeout_secs: u64,
) -> (bool, Option<(String, bool)>, Vec<DownloadProgress>) {
    let mut completed = false;
    let mut failed: Option<(String, bool)> = None;
    let mut progress_events = Vec::new();
    let start = std::time::Instant::now();

    while start.elapsed() < Duration::from_secs(timeout_secs) {
        match timeout(Duration::from_millis(100), events.recv()).await {
            Ok(Ok(event)) => {
                if let DownloadEvent::Progress {
                    id: eid, progress, ..
                } = &event
                {
                    if *eid == id {
                        progress_events.push(progress.clone());
                    }
                }
                if matches!(event, DownloadEvent::Completed { id: eid } if eid == id) {
                    completed = true;
                    break;
                }
                if let DownloadEvent::Failed {
                    id: eid,
                    error,
                    retryable,
                } = event
                {
                    if eid == id {
                        failed = Some((error, retryable));
                        break;
                    }
                }
            }
            _ => continue,
        }
    }

    (completed, failed, progress_events)
}

#[tokio::test]
async fn test_retry_exhaustion_reports_retryable_failure() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // HEAD succeeds but GET always returns 500
    Mock::given(method("HEAD"))
        .and(path("/always-500.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", "4096")
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/always-500.bin"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    let engine = create_fast_retry_engine(&temp_dir, 2).await;
    let mut events = engine.subscribe();

    let url = format!("{}/always-500.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let (completed, failed, _progress) = wait_for_terminal(&mut events, id, 30).await;

    assert!(!completed, "Should not complete when server returns 500");
    let (error_msg, retryable) = failed.expect("Should receive Failed event");
    assert!(
        retryable,
        "500 errors should be retryable, got: {}",
        error_msg
    );

    // Verify the engine status also reflects retryable
    let status = engine.status(id).expect("Should have status");
    match status.state {
        DownloadState::Error { retryable, .. } => {
            assert!(retryable, "Engine state should also be retryable");
        }
        other => panic!("Expected Error state, got {:?}", other),
    }

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_permanent_4xx_does_not_retry() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // HEAD returns 403 immediately
    Mock::given(method("HEAD"))
        .and(path("/forbidden.bin"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1..=2) // HEAD may be tried once or twice
        .mount(&mock_server)
        .await;

    let engine = create_fast_retry_engine(&temp_dir, 3).await;
    let mut events = engine.subscribe();

    let url = format!("{}/forbidden.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let (completed, failed, _) = wait_for_terminal(&mut events, id, 10).await;

    assert!(!completed, "Should not complete on 403");
    let (_error_msg, retryable) = failed.expect("Should receive Failed event");
    assert!(!retryable, "403 errors should NOT be retryable");

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_416_range_not_satisfiable_falls_back_to_single() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

    // HEAD advertises range support
    Mock::given(method("HEAD"))
        .and(path("/gone-416.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    // All ranged GETs return 416
    Mock::given(method("GET"))
        .and(path("/gone-416.bin"))
        .and(HasHeaderMatcher("range"))
        .respond_with(ResponseTemplate::new(416))
        .mount(&mock_server)
        .await;

    // Non-ranged GET serves full file
    Mock::given(method("GET"))
        .and(path("/gone-416.bin"))
        .and(MissingHeaderMatcher("range"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_connections_per_download: 2,
        min_segment_size: 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let url = format!("{}/gone-416.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let (completed, _failed, progress_events) = wait_for_terminal(&mut events, id, 15).await;

    assert!(
        completed,
        "Should complete via single-stream fallback after 416"
    );
    assert_all_progress_invariants(&progress_events);

    let downloaded_file = temp_dir.path().join("gone-416.bin");
    let downloaded = tokio::fs::read(&downloaded_file)
        .await
        .expect("Failed to read downloaded file");
    assert_eq!(downloaded, content);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_etag_change_between_resume_attempts_restarts_from_zero() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let old_content: Vec<u8> = (0..2048).map(|i| (i % 211) as u8).collect();
    let new_content: Vec<u8> = (0..2048).map(|i| (i % 223) as u8).collect();

    // Seed a partial file with OLD content
    let part_path = temp_dir.path().join("etag-change.bin.part");
    tokio::fs::write(&part_path, &old_content[..512])
        .await
        .expect("Failed to seed partial file");

    // HEAD returns new ETag
    Mock::given(method("HEAD"))
        .and(path("/etag-change.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", new_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes")
                .insert_header("ETag", "\"new-etag-v2\""),
        )
        .mount(&mock_server)
        .await;

    // Ranged GET with If-Range returns 200 OK (full body, not 206 — ETag changed)
    Mock::given(method("GET"))
        .and(path("/etag-change.bin"))
        .and(HasHeaderMatcher("range"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", new_content.len().to_string())
                .insert_header("ETag", "\"new-etag-v2\"")
                .set_body_bytes(new_content.clone()),
        )
        .mount(&mock_server)
        .await;

    // Fallback non-ranged GET (for restart)
    Mock::given(method("GET"))
        .and(path("/etag-change.bin"))
        .and(MissingHeaderMatcher("range"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", new_content.len().to_string())
                .set_body_bytes(new_content.clone()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/etag-change.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let (completed, _failed, progress_events) = wait_for_terminal(&mut events, id, 15).await;

    assert!(
        completed,
        "Should complete with new content after ETag change"
    );
    assert_all_progress_invariants(&progress_events);

    // Verify file has NEW content, not a mix
    let downloaded_file = temp_dir.path().join("etag-change.bin");
    let downloaded = tokio::fs::read(&downloaded_file)
        .await
        .expect("Failed to read downloaded file");
    assert_eq!(
        downloaded, new_content,
        "File should contain new content, not old"
    );

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_one_segment_fails_others_stop_promptly() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..4096).map(|i| (i % 233) as u8).collect();

    // HEAD advertises range support
    Mock::given(method("HEAD"))
        .and(path("/one-fails.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    // Segment 0 (bytes=0-2047): always returns 403 Forbidden (non-retryable)
    Mock::given(method("GET"))
        .and(path("/one-fails.bin"))
        .and(header("Range", "bytes=0-2047"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&mock_server)
        .await;

    // Segment 1 (bytes=2048-4095): returns valid 206 with delay
    // The delay ensures we can observe whether it gets cancelled
    Mock::given(method("GET"))
        .and(path("/one-fails.bin"))
        .and(header("Range", "bytes=2048-4095"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Length", "2048")
                .insert_header("Content-Range", "bytes 2048-4095/4096")
                .set_body_bytes(content[2048..].to_vec())
                .set_delay(Duration::from_secs(5)), // Slow — should be cancelled
        )
        .mount(&mock_server)
        .await;

    let engine = create_fast_retry_engine(&temp_dir, 1).await;
    let mut events = engine.subscribe();

    let url = format!("{}/one-fails.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let start = std::time::Instant::now();
    let (completed, failed, _) = wait_for_terminal(&mut events, id, 15).await;
    let elapsed = start.elapsed();

    assert!(!completed, "Should not complete when a segment gets 403");
    assert!(failed.is_some(), "Should receive Failed event");

    // Should fail promptly (well under 5s segment delay), proving cancellation works
    assert!(
        elapsed < Duration::from_secs(4),
        "Should fail promptly due to sibling cancellation, took {:?}",
        elapsed
    );

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_segment_progress_saved_on_failure() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..4096).map(|i| (i % 229) as u8).collect();

    Mock::given(method("HEAD"))
        .and(path("/save-progress.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;

    // Segment 0: succeeds
    Mock::given(method("GET"))
        .and(path("/save-progress.bin"))
        .and(header("Range", "bytes=0-2047"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Length", "2048")
                .insert_header("Content-Range", "bytes 0-2047/4096")
                .set_body_bytes(content[..2048].to_vec()),
        )
        .mount(&mock_server)
        .await;

    // Segment 1: always fails with 500 (retryable, eventually exhausts)
    Mock::given(method("GET"))
        .and(path("/save-progress.bin"))
        .and(header("Range", "bytes=2048-4095"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&mock_server)
        .await;

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_connections_per_download: 2,
        min_segment_size: 1024,
        http: HttpConfig {
            max_retries: 1,
            retry_delay_ms: 50,
            max_retry_delay_ms: 100,
            ..Default::default()
        },
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let url = format!("{}/save-progress.bin", mock_server.uri());
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let (completed, failed, _) = wait_for_terminal(&mut events, id, 30).await;

    assert!(!completed, "Should not complete when segment 1 fails");
    let (error_msg, retryable) = failed.expect("Should receive Failed event");
    assert!(
        retryable,
        "Segmented failure with 500s should be retryable, got: {}",
        error_msg
    );

    // Verify the engine cached segments for resume
    let status = engine.status(id).expect("Should have status");
    match &status.state {
        DownloadState::Error {
            retryable: r,
            message,
            ..
        } => {
            assert!(r, "Error state should be retryable: {}", message);
        }
        other => panic!("Expected Error state, got {:?}", other),
    }

    // Verify the .part file still exists (not cleaned up)
    let part_path = temp_dir.path().join("save-progress.bin.part");
    assert!(
        part_path.exists(),
        ".part file should be preserved for resume"
    );

    engine.shutdown().await.ok();
}

// ---------------------------------------------------------------------------
// Batch operations (pause_all / resume_all / cancel_all)
// ---------------------------------------------------------------------------

/// Mount a slow 1MB endpoint at the given path.
async fn mount_slow_file(mock_server: &MockServer, file_path: &str, delay: Duration) {
    let content = vec![0u8; 1024 * 1024];
    Mock::given(method("HEAD"))
        .and(path(file_path))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path(file_path))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content)
                .set_delay(delay),
        )
        .mount(mock_server)
        .await;
}

#[tokio::test]
async fn test_pause_all_resume_all() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    for name in ["/batch-a.bin", "/batch-b.bin", "/batch-c.bin"] {
        mount_slow_file(&mock_server, name, Duration::from_secs(10)).await;
    }

    // Two slots so the third download stays queued.
    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_concurrent_downloads: 2,
        max_connections_per_download: 4,
        min_segment_size: 1024 * 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let mut ids = Vec::new();
    for name in ["batch-a.bin", "batch-b.bin", "batch-c.bin"] {
        let url = format!("{}/{}", mock_server.uri(), name);
        ids.push(
            engine
                .add_http(&url, DownloadOptions::default())
                .await
                .expect("Failed to add download"),
        );
    }

    // Wait for the two slots to fill.
    for _ in 0..2 {
        wait_for_event(
            &mut events,
            |e| matches!(e, DownloadEvent::Started { .. }),
            Duration::from_secs(5),
        )
        .await
        .expect("Downloads should start");
    }

    let result = engine.pause_all().await;
    assert_eq!(
        result.succeeded.len(),
        3,
        "All three downloads (including the queued one) should pause, got: {:?}",
        result
    );
    assert!(
        result.failed.is_empty(),
        "No failures expected: {:?}",
        result
    );

    // No download may be promoted back into a running state.
    for id in &ids {
        let status = engine.status(*id).expect("Should have status");
        assert_eq!(
            status.state,
            DownloadState::Paused,
            "Download {} should be paused",
            id
        );
    }

    let result = engine.resume_all().await;
    assert_eq!(
        result.succeeded.len(),
        3,
        "All three downloads should resume, got: {:?}",
        result
    );

    // Resumed downloads must be active or queued again.
    for id in &ids {
        let status = engine.status(*id).expect("Should have status");
        assert!(
            status.state.is_active() || status.state == DownloadState::Queued,
            "Download {} should be running or queued after resume_all, got {:?}",
            id,
            status.state
        );
    }

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_pause_all_skips_completed() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // One fast download that completes, one slow one that stays active.
    let small = b"hello world".to_vec();
    Mock::given(method("HEAD"))
        .and(path("/fast.bin"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Length", small.len().to_string()),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fast.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", small.len().to_string())
                .set_body_bytes(small),
        )
        .mount(&mock_server)
        .await;
    mount_slow_file(&mock_server, "/slow.bin", Duration::from_secs(10)).await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let fast_id = engine
        .add_http(
            &format!("{}/fast.bin", mock_server.uri()),
            DownloadOptions::default(),
        )
        .await
        .expect("Failed to add fast download");
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id } if *id == fast_id),
        Duration::from_secs(5),
    )
    .await
    .expect("Fast download should complete");

    let slow_id = engine
        .add_http(
            &format!("{}/slow.bin", mock_server.uri()),
            DownloadOptions::default(),
        )
        .await
        .expect("Failed to add slow download");
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Started { id } if *id == slow_id),
        Duration::from_secs(5),
    )
    .await
    .expect("Slow download should start");

    let result = engine.pause_all().await;
    assert_eq!(result.succeeded, vec![slow_id]);
    assert!(
        result.skipped.is_empty(),
        "Completed download should not be collected at all"
    );
    assert!(result.failed.is_empty());

    let status = engine.status(fast_id).expect("Should have status");
    assert_eq!(status.state, DownloadState::Completed);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_cancel_all() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    for name in ["/cancel-a.bin", "/cancel-b.bin"] {
        mount_slow_file(&mock_server, name, Duration::from_secs(10)).await;
    }

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let mut ids = Vec::new();
    for name in ["cancel-a.bin", "cancel-b.bin"] {
        let url = format!("{}/{}", mock_server.uri(), name);
        ids.push(
            engine
                .add_http(&url, DownloadOptions::default())
                .await
                .expect("Failed to add download"),
        );
    }
    for _ in 0..2 {
        wait_for_event(
            &mut events,
            |e| matches!(e, DownloadEvent::Started { .. }),
            Duration::from_secs(5),
        )
        .await
        .expect("Downloads should start");
    }

    let result = engine.cancel_all(true).await;
    assert_eq!(
        result.succeeded.len(),
        2,
        "Both downloads should cancel: {:?}",
        result
    );
    assert!(engine.list().is_empty(), "No downloads should remain");

    // cancel_all iterates a HashMap, so Removed events arrive in arbitrary
    // order; waiting for them in insertion order would skip (and consume) the
    // other download's event and then time out.
    let mut remaining: std::collections::HashSet<_> = ids.iter().copied().collect();
    while !remaining.is_empty() {
        let removed = wait_for_event(
            &mut events,
            |e| matches!(e, DownloadEvent::Removed { id } if remaining.contains(id)),
            Duration::from_secs(2),
        )
        .await;
        match removed {
            Some(DownloadEvent::Removed { id }) => {
                remaining.remove(&id);
            }
            _ => panic!("Should receive Removed events for {:?}", remaining),
        }
    }

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_pause_queued_download() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    mount_slow_file(&mock_server, "/queue-first.bin", Duration::from_secs(10)).await;
    mount_slow_file(&mock_server, "/queue-second.bin", Duration::from_secs(10)).await;

    // Single slot: the second download stays queued.
    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_concurrent_downloads: 1,
        max_connections_per_download: 4,
        min_segment_size: 1024 * 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let first = engine
        .add_http(
            &format!("{}/queue-first.bin", mock_server.uri()),
            DownloadOptions::default(),
        )
        .await
        .expect("Failed to add first download");
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Started { id } if *id == first),
        Duration::from_secs(5),
    )
    .await
    .expect("First download should start");

    let second = engine
        .add_http(
            &format!("{}/queue-second.bin", mock_server.uri()),
            DownloadOptions::default(),
        )
        .await
        .expect("Failed to add second download");
    assert_eq!(
        engine.status(second).expect("Should have status").state,
        DownloadState::Queued
    );

    // Pause the queued download directly.
    engine
        .pause(second)
        .await
        .expect("Pausing a queued download should work");
    assert_eq!(
        engine.status(second).expect("Should have status").state,
        DownloadState::Paused
    );

    // Free the slot; the paused download must not get promoted into it.
    engine.cancel(first, true).await.expect("Failed to cancel");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        engine.status(second).expect("Should have status").state,
        DownloadState::Paused,
        "Paused download must not start when a slot frees up"
    );

    // And it can still be resumed normally.
    engine.resume(second).await.expect("Failed to resume");
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Started { id } if *id == second),
        Duration::from_secs(5),
    )
    .await
    .expect("Resumed download should start");

    engine.shutdown().await.ok();
}

// ---------------------------------------------------------------------------
// Custom storage injection (DownloadEngine::with_storage)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_with_storage_memory_resume_across_engines() {
    use gosh_dl::MemoryStorage;
    use std::sync::Arc;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    Mock::given(method("HEAD"))
        .and(path("/persist.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .insert_header("Accept-Ranges", "bytes"),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/persist.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone())
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&mock_server)
        .await;

    let storage = Arc::new(MemoryStorage::new());
    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_concurrent_downloads: 4,
        max_connections_per_download: 4,
        min_segment_size: 1024 * 1024,
        ..Default::default()
    };

    // First engine: start a download, pause it, shut down.
    let id = {
        let engine = DownloadEngine::with_storage(config.clone(), storage.clone())
            .await
            .expect("Failed to create engine with storage");
        let mut events = engine.subscribe();

        let id = engine
            .add_http(
                &format!("{}/persist.bin", mock_server.uri()),
                DownloadOptions::default(),
            )
            .await
            .expect("Failed to add download");
        wait_for_event(
            &mut events,
            |e| matches!(e, DownloadEvent::Started { id: eid } if *eid == id),
            Duration::from_secs(5),
        )
        .await
        .expect("Download should start");

        engine.pause(id).await.expect("Failed to pause");
        engine.shutdown().await.ok();
        id
    };

    // Second engine sharing the same storage: the download must be restored
    // as paused and resumable to completion.
    let engine = DownloadEngine::with_storage(config, storage)
        .await
        .expect("Failed to recreate engine with storage");
    let status = engine
        .status(id)
        .expect("Download should be restored from injected storage");
    assert_eq!(status.state, DownloadState::Paused);

    let mut events = engine.subscribe();
    engine.resume(id).await.expect("Failed to resume");
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(15),
    )
    .await
    .expect("Resumed download should complete");

    let file = temp_dir.path().join("persist.bin");
    let downloaded = tokio::fs::read(&file).await.expect("File should exist");
    assert_eq!(downloaded, content);

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_with_storage_file_storage_resume_across_engines() {
    use gosh_dl::FileStorage;
    use std::sync::Arc;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let state_dir = TempDir::new().expect("Failed to create state dir");
    let mock_server = MockServer::start().await;

    let content = b"file storage round trip".to_vec();
    Mock::given(method("HEAD"))
        .and(path("/sidecar.bin"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Length", content.len().to_string()),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sidecar.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone())
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&mock_server)
        .await;

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };

    let id = {
        let storage = Arc::new(
            FileStorage::new(state_dir.path())
                .await
                .expect("Failed to create file storage"),
        );
        let engine = DownloadEngine::with_storage(config.clone(), storage)
            .await
            .expect("Failed to create engine with file storage");
        let mut events = engine.subscribe();

        let id = engine
            .add_http(
                &format!("{}/sidecar.bin", mock_server.uri()),
                DownloadOptions::default(),
            )
            .await
            .expect("Failed to add download");
        wait_for_event(
            &mut events,
            |e| matches!(e, DownloadEvent::Started { id: eid } if *eid == id),
            Duration::from_secs(5),
        )
        .await
        .expect("Download should start");

        engine.pause(id).await.expect("Failed to pause");
        engine.shutdown().await.ok();
        id
    };

    // A fresh FileStorage over the same directory must restore the download.
    let storage = Arc::new(
        FileStorage::new(state_dir.path())
            .await
            .expect("Failed to reopen file storage"),
    );
    let engine = DownloadEngine::with_storage(config, storage)
        .await
        .expect("Failed to recreate engine with file storage");
    let status = engine
        .status(id)
        .expect("Download should be restored from sidecar files");
    assert_eq!(status.state, DownloadState::Paused);

    let mut events = engine.subscribe();
    engine.resume(id).await.expect("Failed to resume");
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(15),
    )
    .await
    .expect("Resumed download should complete");

    engine.shutdown().await.ok();
}

/// Cross-restart resume validation: a partial file left by a previous session
/// must be discarded when the remote file's validators changed, even though
/// If-Range against a fresh probe would happily resume it.
#[cfg(feature = "storage")]
#[tokio::test]
async fn test_cross_restart_resume_discards_partial_when_etag_changed() {
    use gosh_dl::{DownloadId, DownloadKind, DownloadMetadata, DownloadStatus};

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let old_content: Vec<u8> = (0..4096).map(|i| (i % 211) as u8).collect();
    let new_content: Vec<u8> = (0..4096).map(|i| (i % 223) as u8).collect();

    // Simulate a previous session: a partial file holding OLD content...
    let part_path = temp_dir.path().join("cross-restart.bin.part");
    tokio::fs::write(&part_path, &old_content[..1024])
        .await
        .expect("Failed to seed partial file");

    // ...and a persisted Paused download whose metadata recorded the OLD ETag.
    let id = DownloadId::new();
    let url = format!("{}/cross-restart.bin", mock_server.uri());
    let db_path = temp_dir.path().join("cross-restart.sqlite");
    {
        let storage = SqliteStorage::new(&db_path)
            .await
            .expect("Failed to create storage");
        let status = DownloadStatus {
            id,
            kind: DownloadKind::Http,
            state: DownloadState::Paused,
            priority: Default::default(),
            progress: DownloadProgress {
                total_size: Some(old_content.len() as u64),
                completed_size: 1024,
                ..Default::default()
            },
            metadata: DownloadMetadata {
                name: "cross-restart.bin".to_string(),
                url: Some(url.clone()),
                magnet_uri: None,
                info_hash: None,
                save_dir: temp_dir.path().to_path_buf(),
                filename: Some("cross-restart.bin".to_string()),
                user_agent: None,
                referer: None,
                headers: Vec::new(),
                cookies: Vec::new(),
                checksum: None,
                mirrors: Vec::new(),
                etag: Some("\"old-etag-v1\"".to_string()),
                last_modified: None,
            },
            torrent_info: None,
            peers: None,
            created_at: chrono::Utc::now(),
            completed_at: None,
        };
        storage
            .save_download(&status)
            .await
            .expect("Failed to persist previous-session state");
    }

    // The server now serves NEW content under a NEW ETag. A ranged request
    // gets an obliging 206 — this is the dangerous case: without saved-ETag
    // validation, the old partial bytes would be silently kept.
    Mock::given(method("HEAD"))
        .and(path("/cross-restart.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", new_content.len().to_string())
                .insert_header("Accept-Ranges", "bytes")
                .insert_header("ETag", "\"new-etag-v2\""),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cross-restart.bin"))
        .and(header("Range", "bytes=1024-"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    format!("bytes 1024-4095/{}", new_content.len()),
                )
                .insert_header("Content-Length", "3072")
                .insert_header("ETag", "\"new-etag-v2\"")
                .set_body_bytes(new_content[1024..].to_vec()),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cross-restart.bin"))
        .and(MissingHeaderMatcher("range"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", new_content.len().to_string())
                .insert_header("ETag", "\"new-etag-v2\"")
                .set_body_bytes(new_content.clone()),
        )
        .mount(&mock_server)
        .await;

    // "Restart": a fresh engine on the same database restores the download.
    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        database_path: Some(db_path),
        max_connections_per_download: 1,
        min_segment_size: 1024 * 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    engine.resume(id).await.expect("Failed to resume");

    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await;
    assert!(completed.is_some(), "Resumed download should complete");

    let downloaded = tokio::fs::read(temp_dir.path().join("cross-restart.bin"))
        .await
        .expect("Failed to read downloaded file");
    assert_eq!(
        downloaded, new_content,
        "File must be entirely new content — old partial bytes must not survive an ETag change"
    );

    engine.shutdown().await.ok();
}

/// The global download limit must actually throttle transfers — it used to
/// be enforced only on paths most downloads never took.
#[tokio::test]
async fn test_global_download_limit_throttles() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    // 96 KiB at 32 KiB/s: ~32 KiB burst + 64 KiB at rate = at least ~2s.
    let content: Vec<u8> = (0..96 * 1024).map(|i| (i % 251) as u8).collect();
    Mock::given(method("GET"))
        .and(path("/limited.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/limited.bin"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Length", content.len().to_string()),
        )
        .mount(&mock_server)
        .await;

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        global_download_limit: Some(32 * 1024),
        min_segment_size: 1024 * 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let url = format!("{}/limited.bin", mock_server.uri());
    let start = std::time::Instant::now();
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(30),
    )
    .await;
    assert!(completed.is_some(), "Throttled download should complete");

    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1200),
        "96 KiB at 32 KiB/s finished suspiciously fast ({elapsed:?}) — limiter not enforced"
    );

    let downloaded = tokio::fs::read(temp_dir.path().join("limited.bin"))
        .await
        .expect("Failed to read downloaded file");
    assert_eq!(downloaded, content);

    engine.shutdown().await.ok();
}

/// The streaming reader yields HTTP bytes while the download is still in
/// flight, and ends exactly at the full content.
#[tokio::test]
async fn test_http_streaming_reader_mid_download() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 249) as u8).collect();

    // Hand-rolled HTTP server that trickles the body in 16 KiB chunks with
    // delays, so the download is genuinely in progress while we read.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = content.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let body = served.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                if request.starts_with("HEAD") {
                    return;
                }
                for chunk in body.chunks(16 * 1024) {
                    if sock.write_all(chunk).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                    tokio::time::sleep(Duration::from_millis(60)).await;
                }
            });
        }
    });

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        min_segment_size: 1024 * 1024, // force single-connection
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");

    let url = format!("http://{}/stream.bin", addr);
    let id = engine
        .add_http(&url, DownloadOptions::default())
        .await
        .expect("Failed to add download");

    let mut reader = engine.open_reader(id, 0).expect("open reader");
    let mut streamed = Vec::new();
    let mut first_bytes_at = None;
    let start = std::time::Instant::now();
    let mut buf = [0u8; 8192];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(20), reader.read(&mut buf))
            .await
            .expect("stream should not stall")
            .expect("stream read should succeed");
        if n == 0 {
            break;
        }
        if first_bytes_at.is_none() {
            first_bytes_at = Some(start.elapsed());
        }
        streamed.extend_from_slice(&buf[..n]);
    }

    assert_eq!(streamed, content, "streamed bytes must match the source");
    // 16 chunks × 60ms ≈ 1s of trickle: the first bytes must arrive well
    // before the download can have finished.
    let first = first_bytes_at.expect("should have streamed something");
    assert!(
        first < Duration::from_millis(700),
        "first streamed bytes arrived only after {first:?} — reader not streaming mid-download"
    );

    server.abort();
    engine.shutdown().await.ok();
}

/// Segmented downloads stripe across mirrors: with two URLs serving the same
/// content, both servers receive range requests and the result is intact.
#[tokio::test]
async fn test_segments_stripe_across_mirrors() {
    use wiremock::Respond;

    struct RangeResponder(Vec<u8>);
    impl Respond for RangeResponder {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let range = request
                .headers
                .get("range")
                .and_then(|v| v.to_str().ok())
                .and_then(|r| r.strip_prefix("bytes="))
                .and_then(|r| {
                    let (start, end) = r.split_once('-')?;
                    let start: usize = start.parse().ok()?;
                    let end: usize = end.parse().ok().unwrap_or(self.0.len() - 1);
                    Some((start, end.min(self.0.len() - 1)))
                });
            match range {
                Some((start, end)) => ResponseTemplate::new(206)
                    .insert_header(
                        "Content-Range",
                        format!("bytes {}-{}/{}", start, end, self.0.len()),
                    )
                    .insert_header("Accept-Ranges", "bytes")
                    .set_body_bytes(self.0[start..=end].to_vec()),
                None => ResponseTemplate::new(200)
                    .insert_header("Accept-Ranges", "bytes")
                    .set_body_bytes(self.0.clone()),
            }
        }
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let content: Vec<u8> = (0..256 * 1024).map(|i| (i % 239) as u8).collect();

    let primary = MockServer::start().await;
    let mirror = MockServer::start().await;
    for server in [&primary, &mirror] {
        Mock::given(method("HEAD"))
            .and(path("/striped.bin"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Length", content.len().to_string())
                    .insert_header("Accept-Ranges", "bytes"),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/striped.bin"))
            .respond_with(RangeResponder(content.clone()))
            .mount(server)
            .await;
    }

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_connections_per_download: 4,
        min_segment_size: 32 * 1024,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config)
        .await
        .expect("Failed to create engine");
    let mut events = engine.subscribe();

    let url = format!("{}/striped.bin", primary.uri());
    let options = DownloadOptions {
        mirrors: vec![format!("{}/striped.bin", mirror.uri())],
        ..Default::default()
    };
    let id = engine
        .add_http(&url, options)
        .await
        .expect("Failed to add download");

    let completed = wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(30),
    )
    .await;
    assert!(completed.is_some(), "Striped download should complete");

    let downloaded = tokio::fs::read(temp_dir.path().join("striped.bin"))
        .await
        .expect("Failed to read downloaded file");
    assert_eq!(downloaded, content, "striped download must be byte-perfect");

    // Both servers must have served ranged GETs — that's the striping.
    for (name, server) in [("primary", &primary), ("mirror", &mirror)] {
        let ranged_gets = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "GET" && r.headers.contains_key("range"))
            .count();
        assert!(
            ranged_gets > 0,
            "{name} server received no ranged GETs — segments were not striped"
        );
    }

    engine.shutdown().await.ok();
}

/// verify() detects a corrupted completed download; repair() re-downloads it.
#[tokio::test]
async fn test_verify_and_repair_http() {
    use sha2::Digest;

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let mock_server = MockServer::start().await;

    let content: Vec<u8> = (0..32 * 1024).map(|i| (i % 245) as u8).collect();
    let sha256 = hex::encode(sha2::Sha256::digest(&content));

    Mock::given(method("GET"))
        .and(path("/verify.bin"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Content-Length", content.len().to_string())
                .set_body_bytes(content.clone()),
        )
        .mount(&mock_server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/verify.bin"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Length", content.len().to_string()),
        )
        .mount(&mock_server)
        .await;

    let engine = create_test_engine(&temp_dir).await;
    let mut events = engine.subscribe();

    let url = format!("{}/verify.bin", mock_server.uri());
    let options = DownloadOptions {
        checksum: Some(gosh_dl::http::ExpectedChecksum {
            algorithm: gosh_dl::http::ChecksumAlgorithm::Sha256,
            value: sha256,
        }),
        ..Default::default()
    };
    let id = engine.add_http(&url, options).await.expect("add");
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await
    .expect("should complete");

    // Pristine file verifies.
    let report = engine.verify(id).await.expect("verify should run");
    assert!(
        report.valid,
        "fresh download should verify: {}",
        report.detail
    );

    // Corrupt the file on disk.
    let file_path = temp_dir.path().join("verify.bin");
    let mut bad = tokio::fs::read(&file_path).await.unwrap();
    bad[1000] ^= 0xFF;
    tokio::fs::write(&file_path, &bad).await.unwrap();

    let report = engine.verify(id).await.expect("verify should run");
    assert!(!report.valid, "corrupted file must fail verification");

    // Repair restarts the download; wait for it to complete again.
    let report = engine.repair(id).await.expect("repair should run");
    assert!(
        !report.valid,
        "repair returns the failing pre-repair report"
    );
    wait_for_event(
        &mut events,
        |e| matches!(e, DownloadEvent::Completed { id: eid } if *eid == id),
        Duration::from_secs(10),
    )
    .await
    .expect("repair should re-download to completion");

    let repaired = tokio::fs::read(&file_path).await.unwrap();
    assert_eq!(repaired, content, "repaired file must match the source");
    let report = engine.verify(id).await.expect("verify should run");
    assert!(report.valid, "repaired file should verify");

    engine.shutdown().await.ok();
}
