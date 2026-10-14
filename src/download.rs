use anyhow::{bail, ensure, Context, Result};
use fs2::FileExt;
use gosh_dl::{
    DownloadEngine, DownloadEvent, DownloadId, DownloadOptions, DownloadState, EngineConfig,
    HttpConfig,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::watch;

mod auth;
pub use auth::FileAuthorization;

/// Transport-independent controls shared by the CLI, local web service, and a
/// future desktop host. Pausing waits for the engine to checkpoint its workers.
pub struct DownloadControl {
    pub pause: watch::Receiver<bool>,
    pub progress: watch::Sender<DownloadSnapshot>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DownloadSnapshot {
    pub phase: String,
    pub total_bytes: Option<u64>,
    pub completed_bytes: u64,
    pub speed_bytes: u64,
    pub connections: u32,
    pub eta_seconds: Option<u64>,
}

async fn pause_requested(control: &mut Option<DownloadControl>) {
    if let Some(control) = control {
        loop {
            if *control.pause.borrow_and_update() {
                return;
            }
            // A vanished host also checkpoints the download.
            if control.pause.changed().await.is_err() {
                return;
            }
        }
    } else {
        std::future::pending::<()>().await;
    }
}

fn publish_progress(control: &Option<DownloadControl>, engine: &DownloadEngine, id: DownloadId) {
    if let (Some(control), Some(status)) = (control, engine.status(id)) {
        let phase = match status.state {
            DownloadState::Downloading => "downloading",
            DownloadState::Completed => "verifying",
            _ => "connecting",
        };
        control.progress.send_replace(DownloadSnapshot {
            phase: phase.into(),
            total_bytes: status.progress.total_size,
            completed_bytes: status.progress.completed_size,
            speed_bytes: status.progress.download_speed,
            connections: status.progress.connections,
            eta_seconds: status.progress.eta_seconds,
        });
    }
}

#[derive(Clone, Debug)]
pub struct DownloadRequest {
    pub url: Option<String>,
    pub output: PathBuf,
    pub connections: usize,
    pub resume: bool,
    pub expected_sha256: Option<String>,
    pub pause_after: Option<Duration>,
    pub timeout: Duration,
    pub interactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadReport {
    pub engine: String,
    pub outcome: String,
    pub output: PathBuf,
    pub connections: usize,
    pub bytes: u64,
    pub starting_progress_bytes: u64,
    pub transfer_seconds: f64,
    pub finalize_seconds: f64,
    pub total_seconds: f64,
    /// Fresh downloads only; resumed bytes are not assumed to be reused.
    pub average_mbps: Option<f64>,
    pub sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Job {
    schema: u32,
    id: DownloadId,
    url: String,
    output: PathBuf,
    connections: usize,
    expected_sha256: Option<String>,
    #[serde(default)]
    auth_required: bool,
}

struct Paths {
    output: PathBuf,
    part: PathBuf,
    state: PathBuf,
    job: PathBuf,
    parent: PathBuf,
}

fn paths(output: &Path) -> Result<Paths> {
    let output = std::path::absolute(output)?;
    let parent = output.parent().context("output needs a parent directory")?;
    std::fs::create_dir_all(parent)?;
    let parent = parent.canonicalize()?;
    let name = output
        .file_name()
        .and_then(|v| v.to_str())
        .context("output filename must be valid UTF-8")?;
    ensure!(!name.is_empty(), "output filename is empty");
    let state = parent.join(format!("{name}.ffdm-state"));
    Ok(Paths {
        output: parent.join(name),
        part: parent.join(format!("{name}.ffdm.part")),
        job: state.join("job.json"),
        state,
        parent,
    })
}

fn exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn save_job(path: &Path, job: &Job, replace: bool) -> Result<()> {
    let mut temp =
        tempfile::NamedTempFile::new_in(path.parent().context("missing state directory")?)?;
    serde_json::to_writer_pretty(&mut temp, job)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    if replace {
        temp.persist(path).map_err(|e| e.error)?;
    } else {
        temp.persist_noclobber(path).map_err(|e| e.error)?;
    }
    Ok(())
}

/// Prepare an explicit retry while its output lock excludes all other writers.
/// The engine only resumes Paused jobs. Keep valid ranges and their identity,
/// but never combine unverified bytes with a potentially replaced resource.
async fn prepare_resume(
    storage: &dyn gosh_dl::Storage,
    id: DownloadId,
    paths: &Paths,
    source_max_connections: Option<usize>,
) -> Result<()> {
    let mut status = storage
        .load_download(id)
        .await?
        .context("saved task was not restored by the engine")?;
    if status.state == DownloadState::Completed {
        return Ok(());
    }
    if let Some(max_connections) = source_max_connections {
        if let Some(json) = storage.load_runtime_metadata(id).await? {
            // gosh-dl 0.6.3 restores DownloadOptions from this sidecar and
            // prioritizes its connection count over EngineConfig. Update that
            // count too, preserving the rest of its persisted runtime data.
            let mut runtime: serde_json::Value =
                serde_json::from_str(&json).context("saved engine runtime metadata is invalid")?;
            if let Some(options) = runtime
                .get_mut("options")
                .and_then(|value| value.as_object_mut())
            {
                let previous = options
                    .get("max_connections")
                    .and_then(|value| value.as_u64());
                let effective = previous.map_or(max_connections as u64, |count| {
                    count.min(max_connections as u64)
                });
                options.insert("max_connections".into(), effective.into());
                storage
                    .save_runtime_metadata(id, &serde_json::to_string(&runtime)?)
                    .await?;
            }
        }
    }
    // Older checkpoints stored a custom UA in the generic header list. The
    // engine also sends its default UA, so migrate this header into its explicit
    // metadata slot before resuming without changing validators or ranges.
    if let Some(user_agent) = take_user_agent(&mut status.metadata.headers)? {
        status.metadata.user_agent = Some(user_agent);
    }
    let segments = storage.load_segments(id).await?;
    let engine_part = paths.part.with_extension("part.part");
    let has_bytes = status.progress.completed_size > 0
        || segments.iter().any(|segment| segment.downloaded > 0)
        || std::fs::metadata(&engine_part).is_ok_and(|file| file.len() > 0);
    let missing_identity =
        status.metadata.etag.is_none() && status.metadata.last_modified.is_none();
    if has_bytes && missing_identity {
        // A very fast failed transfer may precede gosh-dl's first progress
        // callback, leaving ranges but no persisted validator. Removing bytes
        // before their checkpoint also keeps an interrupted reset safe.
        if exists(&engine_part)? {
            std::fs::remove_file(&engine_part)
                .context("could not restart unverified partial file")?;
        }
        storage.delete_segments(id).await?;
        status.progress.completed_size = 0;
    }
    if matches!(status.state, DownloadState::Error { .. }) {
        status.state = DownloadState::Paused;
    }
    storage.save_download(&status).await?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub status: u16,
    pub protocol: String,
    pub method: String,
    pub content_length: Option<u64>,
    pub accept_ranges: Option<String>,
}

/// Probe without consuming a full response body. Some download endpoints reject HEAD.
pub async fn probe(url: &str) -> Result<ProbeReport> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(12))
        .build()?;
    let head = client
        .head(url)
        .header("Accept-Encoding", "identity")
        .send()
        .await;
    let (response, method) = match head {
        Ok(response)
            if response.status().is_success()
                && response
                    .headers()
                    .contains_key(reqwest::header::CONTENT_LENGTH) =>
        {
            (response, "HEAD")
        }
        _ => (
            client
                .get(url)
                .header("Accept-Encoding", "identity")
                .header("Range", "bytes=0-0")
                .send()
                .await?,
            "GET bytes=0-0",
        ),
    };
    let headers = response.headers();
    let content_length = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        headers
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit_once('/'))
            .and_then(|(_, total)| total.parse().ok())
    } else {
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
    };
    Ok(ProbeReport {
        status: response.status().as_u16(),
        protocol: format!("{:?}", response.version()),
        method: method.into(),
        content_length,
        accept_ranges: headers
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    })
}

fn validate_hash(hash: &Option<String>) -> Result<()> {
    if let Some(hash) = hash {
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "SHA-256 must contain exactly 64 hexadecimal characters"
        );
    }
    Ok(())
}

/// Adapter over the unmodified gosh-dl HTTP engine. One output owns one session.
pub async fn download(request: DownloadRequest) -> Result<DownloadReport> {
    download_with_control(request, None).await
}

pub async fn download_with_control(
    request: DownloadRequest,
    control: Option<DownloadControl>,
) -> Result<DownloadReport> {
    download_with_headers(request, control, Vec::new()).await
}

pub fn validate_headers(headers: &[(String, String)]) -> Result<()> {
    ffdm_resolver::validation::validate_headers(headers)
}

fn take_user_agent(headers: &mut Vec<(String, String)>) -> Result<Option<String>> {
    let values: Vec<_> = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        .map(|(_, value)| value.clone())
        .collect();
    ensure!(values.len() <= 1, "媒体请求不能包含多个 User-Agent");
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("user-agent"));
    Ok(values.into_iter().next())
}

/// Platform headers belong to the media request, not the page or UI transport.
/// gosh-dl persists these options with its checkpoint for subsequent resumes.
pub async fn download_with_headers(
    request: DownloadRequest,
    control: Option<DownloadControl>,
    headers: Vec<(String, String)>,
) -> Result<DownloadReport> {
    download_inner(request, control, headers, None).await
}

/// Download a user's authorized file without putting its Cookie into a job,
/// SQLite database or resume checkpoint. A fresh credential can resume old ranges.
pub async fn download_with_authorization(
    request: DownloadRequest,
    control: Option<DownloadControl>,
    authorization: FileAuthorization,
) -> Result<DownloadReport> {
    let result = download_inner(
        request,
        control,
        authorization.headers(),
        Some(&authorization),
    )
    .await;
    result.map_err(|error| authorization.safe_error(&error))
}

async fn download_inner(
    request: DownloadRequest,
    mut control: Option<DownloadControl>,
    mut headers: Vec<(String, String)>,
    authorization: Option<&FileAuthorization>,
) -> Result<DownloadReport> {
    validate_headers(&headers)?;
    let mut user_agent = take_user_agent(&mut headers)?;
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("accept-encoding"));
    headers.push(("Accept-Encoding".into(), "identity".into()));
    let started = Instant::now();
    ensure!(
        (1..=16).contains(&request.connections),
        "connections must be between 1 and 16"
    );
    ensure!(!request.timeout.is_zero(), "timeout must be positive");
    validate_hash(&request.expected_sha256)?;
    let paths = paths(&request.output)?;
    ensure!(
        !exists(&paths.output)?,
        "output already exists; refusing to overwrite: {}",
        paths.output.display()
    );
    std::fs::create_dir_all(&paths.state)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.state.join("session.lock"))?;
    lock.try_lock_exclusive()
        .context("another process is using this download")?;
    let mut saved: Option<Job> = if request.resume {
        let job: Job = serde_json::from_reader(
            File::open(&paths.job).context("no saved session for this output")?,
        )?;
        ensure!(
            job.schema == 1 && job.output == paths.output,
            "saved session does not match this output"
        );
        ensure!(
            !job.auth_required || authorization.is_some(),
            "此下载需要重新提供本次 Cookie 授权后继续"
        );
        validate_hash(&job.expected_sha256)?;
        Some(job)
    } else {
        ensure!(
            !exists(&paths.part)?
                && !exists(&paths.job)?
                && !exists(&paths.state.join("engine.sqlite"))?,
            "unfinished session exists; use `ffdm resume` or another output name"
        );
        None
    };
    let url = saved
        .as_ref()
        .map(|j| j.url.clone())
        .or(request.url.clone())
        .context("download URL is required")?;
    let parsed = reqwest::Url::parse(&url).context("invalid URL")?;
    ensure!(
        matches!(parsed.scheme(), "http" | "https"),
        "only HTTP and HTTPS are supported"
    );
    if let Some(auth) = authorization {
        auth.validate_url(&url)?;
        if let Some(input) = request.url.as_deref() {
            auth.validate_url(input)?;
        }
        if saved.is_some() {
            auth.verify_resume_authorization(&url).await?;
        }
        if let Some(job) = &mut saved {
            if !job.auth_required {
                job.auth_required = true;
                save_job(&paths.job, job, true)?;
            }
        }
    }
    let source_limits = ffdm_resolver::source_policy::limits(&url);
    if source_limits.is_some() && user_agent.is_none() {
        user_agent = Some(ffdm_resolver::source_policy::USER_AGENT.into());
    }
    let requested_connections = saved
        .as_ref()
        .map_or(request.connections, |j| j.connections);
    let connections = source_limits.map_or(requested_connections, |policy| {
        requested_connections.min(policy.max_connections)
    });
    if let Some(job) = &mut saved {
        if job.connections != connections {
            // Adopt a source's current limits without changing its identity,
            // validators or already completed ranges in the engine database.
            job.connections = connections;
            save_job(&paths.job, job, true)?;
        }
    }
    let mut preflight_headers = headers.clone();
    if let Some(agent) = &user_agent {
        preflight_headers.push(("User-Agent".into(), agent.clone()));
    }
    let expected_hash = saved
        .as_ref()
        .and_then(|j| j.expected_sha256.clone())
        .or(request.expected_sha256.clone());
    let mut config = EngineConfig {
        download_dir: paths.parent.clone(),
        max_concurrent_downloads: 1,
        max_connections_per_download: connections,
        global_download_limit: source_limits.map(|policy| policy.bytes_per_second),
        min_segment_size: 1024 * 1024,
        database_path: Some(paths.state.join("engine.sqlite")),
        http: HttpConfig {
            proxy_url: crate::capture_transport::download_proxy(),
            max_retries: 2,
            connect_timeout: 15,
            read_timeout: 30,
            ..Default::default()
        },
        ..Default::default()
    };
    let engine = if let Some(auth) = authorization {
        // Never allow a copied Cookie to cross an origin boundary. Even same-
        // origin redirects require the caller to supply the final file address.
        config.http.max_redirects = 0;
        #[cfg(test)]
        {
            config.http.accept_invalid_certs = auth.accepts_local_test_certificate();
        }
        let database = config.database_path.take().unwrap();
        let storage = std::sync::Arc::new(auth::AuthorizedStorage::new(
            std::sync::Arc::new(gosh_dl::SqliteStorage::new(database).await?),
            auth.clone(),
        ));
        if let Some(job) = &saved {
            prepare_resume(
                storage.as_ref(),
                job.id,
                &paths,
                source_limits.map(|policy| policy.max_connections),
            )
            .await?;
        }
        DownloadEngine::with_storage(config, storage).await?
    } else if let Some(job) = &saved {
        // The output lock above excludes other engines for this job. A
        // failed HTTP attempt is explicitly retryable by the user, but
        // gosh-dl's resume() only accepts Paused. Change that state alone;
        // keep its ID, headers, validators and range checkpoints intact.
        let database = config.database_path.take().unwrap();
        let storage = std::sync::Arc::new(gosh_dl::SqliteStorage::new(database).await?);
        prepare_resume(
            storage.as_ref(),
            job.id,
            &paths,
            source_limits.map(|policy| policy.max_connections),
        )
        .await?;
        DownloadEngine::with_storage(config, storage).await?
    } else {
        DownloadEngine::new(config).await?
    };
    let mut events = engine.subscribe();
    let id = if let Some(job) = saved {
        job.id
    } else {
        let options = DownloadOptions {
            start_paused: true,
            save_dir: Some(paths.parent.clone()),
            filename: Some(
                paths
                    .part
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
            max_connections: Some(connections),
            user_agent,
            headers,
            ..Default::default()
        };
        let result = async {
            let id = engine.add_http(&url, options).await?;
            save_job(
                &paths.job,
                &Job {
                    schema: 1,
                    id,
                    url: url.clone(),
                    output: paths.output.clone(),
                    connections,
                    expected_sha256: expected_hash.clone(),
                    auth_required: authorization.is_some(),
                },
                false,
            )?;
            Ok::<_, anyhow::Error>(id)
        }
        .await;
        match result {
            Ok(id) => id,
            Err(e) => {
                let _ = engine.shutdown().await;
                return Err(e);
            }
        }
    };
    let starting_progress = engine.status(id).map_or(0, |s| s.progress.completed_size);
    let transfer_started = Instant::now();
    // Keep the permit until shutdown has joined the workers, including on
    // pause/failure. Releasing it on a terminal event alone can exceed the
    // source's concurrency budget while the old workers are still exiting.
    let mut source_permit = None;
    let mut engine_resumed = false;
    let outcome: Result<bool> = async {
        let status = engine.status(id).context("saved task was not restored by the engine")?;
        if status.state == DownloadState::Completed { return Ok(true); }
        if control.as_ref().is_some_and(|control| *control.pause.borrow()) {
            return Ok(false);
        }
        let pause_deadline = tokio::time::Instant::now() + request.pause_after.unwrap_or(Duration::from_secs(365 * 86400));
        let timeout_deadline = tokio::time::Instant::now() + request.timeout;
        source_permit = tokio::select! {
            result = ffdm_resolver::source_policy::acquire(&url) => result?,
            _ = pause_requested(&mut control) => return Ok(false),
            _ = tokio::time::sleep_until(pause_deadline), if request.pause_after.is_some() => return Ok(false),
            _ = tokio::time::sleep_until(timeout_deadline) => bail!("download timed out while waiting for the source; saved session retained"),
            signal = tokio::signal::ctrl_c(), if request.interactive => { signal?; return Ok(false); },
        };
        if source_limits.is_some() && authorization.is_none() {
            tokio::select! {
                result = ffdm_resolver::source_policy::preflight(&url, &preflight_headers) => result?,
                _ = pause_requested(&mut control) => return Ok(false),
                _ = tokio::time::sleep_until(pause_deadline), if request.pause_after.is_some() => return Ok(false),
                _ = tokio::time::sleep_until(timeout_deadline) => bail!("download timed out during source check; saved session retained"),
                signal = tokio::signal::ctrl_c(), if request.interactive => { signal?; return Ok(false); },
            }
        }
        engine.resume(id).await?;
        engine_resumed = true;
        let mut ticks = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                biased;
                event = events.recv() => {
                    match event {
                        Ok(DownloadEvent::Completed { id: event_id }) if event_id == id => {
                            publish_progress(&control, &engine, id);
                            return Ok(true);
                        },
                        Ok(DownloadEvent::Failed { id: event_id, error, .. }) if event_id == id => bail!("download failed: {error}"),
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => bail!("engine event stream closed"),
                        _ => {},
                    }
                }
                _ = pause_requested(&mut control) => return Ok(false),
                _ = tokio::time::sleep_until(pause_deadline), if request.pause_after.is_some() => return Ok(false),
                _ = tokio::time::sleep_until(timeout_deadline) => bail!("download timed out; saved session retained"),
                signal = tokio::signal::ctrl_c(), if request.interactive => {
                    signal?;
                    return Ok(false);
                }
                _ = ticks.tick() => {
                    publish_progress(&control, &engine, id);
                    if let Some(status) = engine.status(id) {
                        match status.state {
                            DownloadState::Completed => return Ok(true),
                            DownloadState::Error { message, .. } => bail!("download failed: {message}"),
                            _ => {},
                        }
                        if request.interactive {
                            eprint!("\r{:.1} MiB  {:.1} MB/s  {} segment(s) requested     ", status.progress.completed_size as f64 / 1048576.0, status.progress.download_speed as f64 / 1_000_000.0, connections);
                        }
                    }
                }
            }
        }
    }.await;
    let transfer_seconds = transfer_started.elapsed().as_secs_f64();
    // shutdown snapshots the live ranges and joins workers. Calling pause first
    // would discard gosh-dl's HTTP task handle before shutdown can join it.
    let shutdown_result = engine.shutdown().await;
    if request.interactive {
        eprintln!();
    }
    shutdown_result?;
    // gosh-dl does not retain Retry-After from later requests. If a transfer
    // races a new limit after preflight, impose the conservative fallback.
    if engine_resumed
        && outcome
            .as_ref()
            .is_err_and(|error| error.to_string().contains("429"))
    {
        let _ = ffdm_resolver::source_policy::record_retry_after(&url, None);
    }
    drop(source_permit);
    let complete = outcome?;
    let bytes = engine.status(id).map_or(0, |s| s.progress.completed_size);
    drop(events);
    drop(engine);
    let finalize_started = Instant::now();
    let part = paths.part.clone();
    let output = paths.output.clone();
    let hash = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
        if exists(&part)? {
            OpenOptions::new().write(true).open(&part)?.sync_all()?;
        }
        if !complete {
            return Ok(None);
        }
        let hash = sha256_file(&part)?;
        if let Some(expected) = expected_hash {
            ensure!(
                hash.eq_ignore_ascii_case(&expected),
                "SHA-256 mismatch: got {hash}; temporary file retained"
            );
        }
        // Same-directory hard link publishes without ever replacing an existing
        // destination. Interrupted publication leaves a complete final file.
        std::fs::hard_link(&part, &output)
            .context("could not publish completed output without overwriting")?;
        // Publication has succeeded and the destination is complete. Failure
        // to remove its temporary hard link must not turn it into a failed job
        // that can never retry because the final file already exists.
        let _ = std::fs::remove_file(&part);
        Ok(Some(hash))
    })
    .await??;
    let finalize_seconds = finalize_started.elapsed().as_secs_f64();
    let total_seconds = started.elapsed().as_secs_f64();
    let bytes = if complete {
        std::fs::metadata(&paths.output)?.len()
    } else {
        bytes
    };
    let report = DownloadReport {
        engine: "gosh-dl 0.6.3 (Rust HTTP/storage)".into(),
        outcome: if complete { "completed" } else { "paused" }.into(),
        output: paths.output,
        connections,
        bytes,
        starting_progress_bytes: starting_progress,
        transfer_seconds,
        finalize_seconds,
        total_seconds,
        average_mbps: (complete && !request.resume)
            .then_some(bytes as f64 * 8.0 / total_seconds / 1_000_000.0),
        sha256: hash,
    };
    drop(lock);
    Ok(report)
}
