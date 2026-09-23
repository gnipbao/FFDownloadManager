use ffdownload::{
    download::{download, DownloadRequest},
    test_server::{fixture_data, ServerConfig, TestServer},
};
use sha2::{Digest, Sha256};
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
