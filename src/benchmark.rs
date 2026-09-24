use crate::{
    download::{download, sha256_file, DownloadReport, DownloadRequest},
    test_server::{fixture_data, ServerConfig, TestServer},
};
use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::AsyncWriteExt;

#[derive(Serialize, Deserialize)]
pub struct Sample {
    pub scenario: String,
    pub implementation: String,
    pub round: usize,
    pub server_range_requests: usize,
    pub report: DownloadReport,
}

#[derive(Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema: u32,
    pub unix_time: u64,
    pub platform: String,
    pub file_bytes: u64,
    pub rounds: usize,
    pub scope: String,
    pub samples: Vec<Sample>,
}

pub async fn stream_baseline(
    url: &str,
    output: &Path,
    expected: &str,
    timeout: Duration,
) -> Result<DownloadReport> {
    let started = Instant::now();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .timeout(timeout)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .build()?;
    let response = client
        .get(url)
        .header("Accept-Encoding", "identity")
        .send()
        .await?
        .error_for_status()?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "baseline expected a full 200 response"
    );
    let expected_len = response.content_length();
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)
        .await?;
    let mut writer = tokio::io::BufWriter::with_capacity(1024 * 1024, file);
    let mut stream = response.bytes_stream();
    let mut bytes = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        bytes += chunk.len() as u64;
        writer.write_all(&chunk).await?;
    }
    if let Some(expected_len) = expected_len {
        ensure!(bytes == expected_len, "baseline length mismatch");
    }
    writer.flush().await?;
    let transfer_seconds = started.elapsed().as_secs_f64();
    let finalize_started = Instant::now();
    writer.get_ref().sync_all().await?;
    drop(writer);
    let path = output.to_path_buf();
    let hash = tokio::task::spawn_blocking(move || sha256_file(&path)).await??;
    ensure!(hash == expected, "baseline SHA-256 mismatch");
    let finalize_seconds = finalize_started.elapsed().as_secs_f64();
    let total_seconds = started.elapsed().as_secs_f64();
    Ok(DownloadReport {
        engine: "reqwest 0.13.5 single stream / 1 MiB buffer (Rust)".into(),
        outcome: "completed".into(),
        output: output.to_path_buf(),
        connections: 1,
        bytes,
        starting_progress_bytes: 0,
        transfer_seconds,
        finalize_seconds,
        total_seconds,
        average_mbps: Some(bytes as f64 * 8.0 / total_seconds / 1_000_000.0),
        sha256: Some(hash),
    })
}

/// Run clients sequentially. Rotate their order each round to reduce order bias.
pub async fn local_benchmark(
    mib: usize,
    rounds: usize,
    connections: Vec<usize>,
    output: &Path,
) -> Result<BenchmarkReport> {
    ensure!((8..=1024).contains(&mib), "fixture size must be 8–1024 MiB");
    ensure!((1..=20).contains(&rounds), "rounds must be 1–20");
    ensure!(
        !connections.is_empty() && connections.iter().all(|c| (1..=16).contains(c)),
        "connections must be 1–16"
    );
    let data = fixture_data(mib * 1024 * 1024);
    let expected = format!("{:x}", Sha256::digest(&data));
    let mut report = BenchmarkReport {
        schema: 1,
        unix_time: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        file_bytes: data.len() as u64, rounds,
        scope: "HTTP/1.1 loopback, Rust fixture server on the same machine; warm OS cache; not WAN, TLS, or a Neat comparison. Timings include sync and SHA-256. Pacing limits each response, not the physical link.".into(),
        samples: vec![],
    };
    for (scenario, rate) in [
        ("loopback_uncapped", None),
        ("per_response_100mbps", Some(100.0)),
    ] {
        let mut server_config = ServerConfig::new(data.clone());
        server_config.per_response_mbps = rate;
        let server = TestServer::start(server_config).await?;
        let mut cases = vec![("rust_stream".to_string(), 1_usize)];
        cases.extend(connections.iter().map(|c| (format!("gosh_{c}"), *c)));
        // One untimed warmup per scenario; no downloaded fixture is reused.
        let warmup = tempfile::tempdir()?;
        stream_baseline(
            &server.url,
            &warmup.path().join("warmup.bin"),
            &expected,
            Duration::from_secs(120),
        )
        .await?;
        for round in 1..=rounds {
            for offset in 0..cases.len() {
                let (name, connections) = &cases[(offset + round - 1) % cases.len()];
                eprintln!("benchmark: {scenario}, round {round}/{rounds}, {name}");
                let work = tempfile::tempdir()?;
                let ranges_before = server.range_requests();
                let path = work.path().join("sample.bin");
                let sample = if name == "rust_stream" {
                    stream_baseline(&server.url, &path, &expected, Duration::from_secs(120)).await?
                } else {
                    download(DownloadRequest {
                        url: Some(server.url.clone()),
                        output: path,
                        connections: *connections,
                        resume: false,
                        expected_sha256: Some(expected.clone()),
                        pause_after: None,
                        timeout: Duration::from_secs(120),
                        interactive: false,
                    })
                    .await?
                };
                ensure!(
                    sample.bytes == report.file_bytes && sample.outcome == "completed",
                    "incomplete benchmark sample"
                );
                eprintln!(
                    "  {:.2} s, {:.1} Mbps, SHA-256 OK",
                    sample.total_seconds,
                    sample.average_mbps.unwrap_or(0.0)
                );
                report.samples.push(Sample {
                    scenario: scenario.into(),
                    implementation: name.clone(),
                    round,
                    server_range_requests: server.range_requests() - ranges_before,
                    report: sample,
                });
                write_report(output, &report)?;
            }
        }
        server.stop().await?;
    }
    Ok(report)
}

pub fn write_report(output: &Path, report: &BenchmarkReport) -> Result<()> {
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(output)
        .with_context(|| format!("cannot write {}", output.display()))?;
    serde_json::to_writer_pretty(file, report)?;
    Ok(())
}

pub fn summarize(report: &BenchmarkReport) -> String {
    let mut groups = std::collections::BTreeMap::<(String, String), Vec<&DownloadReport>>::new();
    for sample in &report.samples {
        groups
            .entry((sample.scenario.clone(), sample.implementation.clone()))
            .or_default()
            .push(&sample.report);
    }
    let mut result = String::from("scenario\tclient\tmedian seconds\tmedian Mbps\n");
    for ((scenario, implementation), samples) in groups {
        let mut seconds: Vec<f64> = samples.iter().map(|s| s.total_seconds).collect();
        let mut rates: Vec<f64> = samples.iter().filter_map(|s| s.average_mbps).collect();
        seconds.sort_by(f64::total_cmp);
        rates.sort_by(f64::total_cmp);
        let median = |v: &[f64]| {
            if v.len().is_multiple_of(2) {
                (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
            } else {
                v[v.len() / 2]
            }
        };
        result.push_str(&format!(
            "{scenario}\t{implementation}\t{:.3}\t{:.1}\n",
            median(&seconds),
            median(&rates)
        ));
    }
    result
}

pub fn default_report_path() -> PathBuf {
    PathBuf::from("benchmarks/local-results.json")
}

/// One reproducible test at a time; the UI receives measured samples and hashes.
pub struct WebBenchmark {
    status: std::sync::Mutex<serde_json::Value>,
    output: PathBuf,
}
impl WebBenchmark {
    pub fn new(output: PathBuf) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            status: std::sync::Mutex::new(
                serde_json::json!({"running":false,"report":null,"error":null}),
            ),
            output,
        })
    }
    pub fn snapshot(&self) -> serde_json::Value {
        self.status.lock().unwrap().clone()
    }
    pub fn start(self: &std::sync::Arc<Self>) -> Result<()> {
        ensure!(cfg!(debug_assertions), "内核测速仅在开发模式可用");
        let mut status = self.status.lock().unwrap();
        ensure!(status["running"] != true, "测速正在进行中");
        *status = serde_json::json!({"running":true,"report":null,"error":null});
        let this = self.clone();
        tokio::spawn(async move {
            let result = local_benchmark(32, 1, vec![1, 4, 8], &this.output).await;
            *this.status.lock().unwrap() = match result {
                Ok(report) => serde_json::json!({"running":false,"report":report,"error":null}),
                Err(error) => {
                    serde_json::json!({"running":false,"report":null,"error":crate::media::safe_error(&error.to_string())})
                }
            };
        });
        Ok(())
    }
}
