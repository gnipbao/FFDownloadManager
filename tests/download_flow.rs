use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, Response},
    routing::get,
    Router,
};
use ffdownload::{
    download::{download, download_with_headers, DownloadRequest},
    test_server::{fixture_data, ServerConfig, TestServer},
};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};
use std::{path::Path, time::Duration};

fn request(url: &str, output: &Path, hash: &str) -> DownloadRequest {
    DownloadRequest {
        url: Some(url.into()),
        output: output.into(),
        connections: 4,
        resume: false,
        expected_sha256: Some(hash.into()),
        pause_after: None,
        timeout: Duration::from_secs(20),
        interactive: false,
    }
}

#[tokio::test]
async fn head_rejection_probe_uses_total_range_length() {
    let mut config = ServerConfig::new(fixture_data(8192));
    config.reject_head = true;
    let server = TestServer::start(config).await.unwrap();
    let metadata = ffdownload::download::probe(&server.url).await.unwrap();
    assert_eq!(metadata.status, 206);
    assert_eq!(metadata.method, "GET bytes=0-0");
    assert_eq!(metadata.content_length, Some(8192));
    assert_eq!(server.range_requests(), 1);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn segmented_download_verifies_bytes_and_never_overwrites() {
    let data = fixture_data(8 * 1024 * 1024);
    let hash = format!("{:x}", Sha256::digest(&data));
    let server = TestServer::start(ServerConfig::new(data.clone()))
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("result.bin");
    let report = download(request(&server.url, &output, &hash))
        .await
        .unwrap();
    assert_eq!(report.outcome, "completed");
    assert_eq!(report.sha256.as_deref(), Some(hash.as_str()));
    assert!(server.range_requests() >= 4, "exercise the segmented path");
    assert_eq!(std::fs::read(&output).unwrap(), data);
    assert!(download(request(&server.url, &output, &hash))
        .await
        .unwrap_err()
        .to_string()
        .contains("overwrite"));
    assert_eq!(std::fs::read(&output).unwrap(), data);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn cli_pause_and_resume_in_a_new_process() {
    let data = fixture_data(8 * 1024 * 1024);
    let hash = format!("{:x}", Sha256::digest(&data));
    let mut config = ServerConfig::new(data.clone());
    config.per_response_mbps = Some(16.0);
    let server = TestServer::start(config).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("resumed.bin");
    let paused = tokio::process::Command::new(env!("CARGO_BIN_EXE_ffdm"))
        .args(["download", &server.url, "--output"])
        .arg(&output)
        .args([
            "--connections",
            "4",
            "--sha256",
            &hash,
            "--pause-after",
            "0.45",
            "--timeout",
            "20",
            "--quiet",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        paused.status.success(),
        "{}",
        String::from_utf8_lossy(&paused.stderr)
    );
    let report: ffdownload::download::DownloadReport =
        serde_json::from_slice(&paused.stdout).unwrap();
    assert_eq!(report.outcome, "paused");
    assert!(!output.exists());
    let resumed = tokio::process::Command::new(env!("CARGO_BIN_EXE_ffdm"))
        .arg("resume")
        .arg(&output)
        .args(["--timeout", "20", "--quiet"])
        .output()
        .await
        .unwrap();
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let report: ffdownload::download::DownloadReport =
        serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(report.outcome, "completed");
    assert!(
        report.starting_progress_bytes > 0,
        "resume should retain checkpoint progress"
    );
    assert!(
        report.average_mbps.is_none(),
        "do not count old bytes as newly transferred"
    );
    assert_eq!(std::fs::read(&output).unwrap(), data);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn server_without_ranges_falls_back_to_single_stream() {
    let data = fixture_data(5 * 1024 * 1024);
    let hash = format!("{:x}", Sha256::digest(&data));
    let mut config = ServerConfig::new(data.clone());
    config.ranges = false;
    let server = TestServer::start(config).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("sequential.bin");
    let report = download(request(&server.url, &output, &hash))
        .await
        .unwrap();
    assert_eq!(report.outcome, "completed");
    assert_eq!(std::fs::read(&output).unwrap(), data);
    server.stop().await.unwrap();
}

#[tokio::test]
async fn invalid_ranges_restart_safely_and_bad_hash_is_not_published() {
    let data = fixture_data(8 * 1024 * 1024);
    let hash = format!("{:x}", Sha256::digest(&data));
    let mut config = ServerConfig::new(data.clone());
    config.incorrect_content_range = true;
    let server = TestServer::start(config).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("bad-range.bin");
    let report = download(request(&server.url, &output, &hash))
        .await
        .unwrap();
    assert!(server.range_requests() > 0);
    assert_eq!(report.outcome, "completed");
    assert_eq!(std::fs::read(&output).unwrap(), data);
    server.stop().await.unwrap();

    let server = TestServer::start(ServerConfig::new(data)).await.unwrap();
    let output = dir.path().join("bad-hash.bin");
    let error = download(request(&server.url, &output, &"0".repeat(64)))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SHA-256 mismatch"));
    assert!(!output.exists());
    server.stop().await.unwrap();
}

struct RecoveringSource {
    content: RwLock<(bytes::Bytes, &'static str)>,
    fail_tail: AtomicBool,
    ranges: Mutex<Vec<(usize, usize)>>,
    sent_bytes: AtomicU64,
}

struct RecoveringServer {
    url: String,
    source: Arc<RecoveringSource>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RecoveringServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RecoveringServer {
    async fn start(data: bytes::Bytes) -> Self {
        let source = Arc::new(RecoveringSource {
            content: RwLock::new((data, "\"retry-v1\"")),
            fail_tail: AtomicBool::new(true),
            ranges: Mutex::new(Vec::new()),
            sent_bytes: AtomicU64::new(0),
        });
        let app = Router::new()
            .route("/file.bin", get(recovering_file))
            .with_state(source.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, source, task }
    }

    fn recover(&self) {
        self.source.fail_tail.store(false, Ordering::SeqCst);
        self.source.ranges.lock().unwrap().clear();
        self.source.sent_bytes.store(0, Ordering::SeqCst);
    }
}

async fn recovering_file(
    State(source): State<Arc<RecoveringSource>>,
    method: Method,
    headers: HeaderMap,
) -> Response<Body> {
    // This header also checks that a media track's persisted request metadata
    // survives Error -> Paused instead of being rebuilt with default options.
    if headers
        .get("x-resume-test")
        .is_none_or(|value| value != "preserve-me")
    {
        return Response::builder().status(403).body(Body::empty()).unwrap();
    }
    let (data, etag) = source.content.read().unwrap().clone();
    let total = data.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(start, end)| {
            Some((
                start.parse::<usize>().ok()?,
                if end.is_empty() {
                    total - 1
                } else {
                    end.parse::<usize>().ok()?
                },
            ))
        });
    let range = range.filter(|_| {
        headers
            .get(header::IF_RANGE)
            .is_none_or(|value| value == etag)
    });
    let (start, end) = range.unwrap_or((0, total - 1));
    if method != Method::HEAD {
        source.ranges.lock().unwrap().push((start, end));
        // Other segments finish while this range exhausts real HTTP 503 retries.
        if source.fail_tail.load(Ordering::SeqCst) && start >= total * 3 / 4 {
            return Response::builder().status(503).body(Body::empty()).unwrap();
        }
    }
    let mut response = Response::builder()
        .status(if range.is_some() { 206 } else { 200 })
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag);
    if range.is_some() {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        source
            .sent_bytes
            .fetch_add((end - start + 1) as u64, Ordering::SeqCst);
        let data = data.slice(start..=end);
        Body::from_stream(futures_util::stream::unfold(
            (data, 0_usize),
            |(data, at)| async move {
                if at == data.len() {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
                let end = (at + 128 * 1024).min(data.len());
                Some((
                    Ok::<_, std::convert::Infallible>(data.slice(at..end)),
                    (data, end),
                ))
            },
        ))
    };
    response.body(body).unwrap()
}

async fn failed_checkpoint(
    output: &Path,
) -> (
    serde_json::Value,
    gosh_dl::DownloadStatus,
    Vec<gosh_dl::Segment>,
) {
    use gosh_dl::Storage;
    let state = output.with_file_name(format!(
        "{}.ffdm-state",
        output.file_name().unwrap().to_str().unwrap()
    ));
    let job: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("job.json")).unwrap()).unwrap();
    let id = serde_json::from_value(job["id"].clone()).unwrap();
    let storage = gosh_dl::SqliteStorage::new(state.join("engine.sqlite"))
        .await
        .unwrap();
    let status = storage.load_download(id).await.unwrap().unwrap();
    assert!(matches!(status.state, gosh_dl::DownloadState::Error { .. }));
    let segments = storage.load_segments(id).await.unwrap();
    (job, status, segments)
}

#[tokio::test]
async fn failed_503_download_resumes_original_job_and_reuses_completed_ranges_and_headers() {
    let data = fixture_data(8 * 1024 * 1024);
    let hash = format!("{:x}", Sha256::digest(&data));
    let server = RecoveringServer::start(data.clone()).await;
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("retry.bin");
    let error = download_with_headers(
        request(&server.url, &output, &hash),
        None,
        vec![("X-Resume-Test".into(), "preserve-me".into())],
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("503"));
    assert!(!output.exists());
    let (job, status, segments) = failed_checkpoint(&output).await;
    let reused: u64 = segments.iter().map(|segment| segment.downloaded).sum();
    assert_eq!(reused, 6 * 1024 * 1024);
    assert_eq!(
        segments
            .iter()
            .filter(|segment| segment.is_complete())
            .count(),
        3
    );
    assert!(status
        .metadata
        .headers
        .iter()
        .any(|(name, value)| name == "X-Resume-Test" && value == "preserve-me"));
    assert_eq!(status.metadata.etag.as_deref(), Some("\"retry-v1\""));
    server.recover();
    let mut resume = request(&server.url, &output, &hash);
    resume.resume = true;
    // No new headers: the original job must provide them for both HEAD and GET.
    let report = download(resume).await.unwrap();
    assert_eq!(report.outcome, "completed");
    assert!(report.starting_progress_bytes > 0);
    assert_eq!(
        server.source.sent_bytes.load(Ordering::SeqCst),
        data.len() as u64 - reused
    );
    assert_eq!(
        *server.source.ranges.lock().unwrap(),
        vec![(6 * 1024 * 1024, data.len() - 1)]
    );
    assert!(
        std::fs::read(&output).unwrap() == data,
        "resumed payload must match the source"
    );
    let state = directory.path().join("retry.bin.ffdm-state/job.json");
    let retained: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
    assert_eq!(retained, job);
}

#[tokio::test]
async fn failed_download_retry_revalidates_changed_source_before_reusing_old_ranges() {
    for missing_saved_identity in [false, true] {
        let old_data = fixture_data(8 * 1024 * 1024);
        let server = RecoveringServer::start(old_data.clone()).await;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("changed.bin");
        let mut initial = request(&server.url, &output, &"0".repeat(64));
        initial.expected_sha256 = None;
        let error = download_with_headers(
            initial.clone(),
            None,
            vec![("X-Resume-Test".into(), "preserve-me".into())],
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("503"));
        let (job, status, segments) = failed_checkpoint(&output).await;
        assert!(segments.iter().any(|segment| segment.downloaded > 0));
        assert_eq!(status.metadata.etag.as_deref(), Some("\"retry-v1\""));
        if missing_saved_identity {
            // Existing fast-failure checkpoints can have ranges but no validator
            // because the upstream progress callback had not fired before failure.
            use gosh_dl::Storage;
            let storage = gosh_dl::SqliteStorage::new(
                directory
                    .path()
                    .join("changed.bin.ffdm-state/engine.sqlite"),
            )
            .await
            .unwrap();
            let id = serde_json::from_value(job["id"].clone()).unwrap();
            let mut status = storage.load_download(id).await.unwrap().unwrap();
            status.metadata.etag = None;
            status.metadata.last_modified = None;
            storage.save_download(&status).await.unwrap();
        }
        let mut replacement = old_data.to_vec();
        replacement[..1024 * 1024].fill(0x5a);
        *server.source.content.write().unwrap() =
            (bytes::Bytes::from(replacement.clone()), "\"retry-v2\"");
        server.recover();
        initial.resume = true;
        let report = download(initial).await.unwrap();
        assert_eq!(report.outcome, "completed");
        assert!(
            std::fs::read(&output).unwrap() == replacement,
            "retry must not combine old and new source bytes"
        );
        assert_eq!(
            server.source.sent_bytes.load(Ordering::SeqCst),
            old_data.len() as u64
        );
        assert!(server
            .source
            .ranges
            .lock()
            .unwrap()
            .iter()
            .any(|(start, _)| *start == 0));
        let retained: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("changed.bin.ffdm-state/job.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(retained, job);
    }
}
