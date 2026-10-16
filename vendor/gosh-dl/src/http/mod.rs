//! HTTP Download Engine
//!
//! This module handles HTTP/HTTPS downloads with support for:
//! - Single and multi-connection (segmented) downloads
//! - Resume capability via Range headers
//! - Progress tracking
//! - Custom headers, user-agent, referer
//! - Connection pooling with rate limiting
//! - Retry logic with exponential backoff
//! - Checksum verification (MD5/SHA256)

pub(crate) mod checksum;
pub(crate) mod connection;
#[cfg(feature = "recursive-http")]
pub(crate) mod crawl;
pub(crate) mod mirror;
pub(crate) mod resume;
pub(crate) mod segment;

pub use checksum::{compute_checksum, verify_checksum, ChecksumAlgorithm, ExpectedChecksum};
pub use connection::{ConnectionPool, RetryPolicy, SpeedCalculator};
pub use mirror::MirrorManager;
pub use resume::{check_resume, ResumeInfo, ResumeValidators};
pub use segment::{calculate_segment_count, probe_server, SegmentedDownload, ServerCapabilities};

use crate::config::EngineConfig;
use crate::error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result, StorageErrorKind};
use crate::storage::Segment;
use crate::types::DownloadProgress;

use futures::StreamExt;
use parking_lot::RwLock;
use reqwest::{Client, Response};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

pub(crate) const ACCEPT_ENCODING_IDENTITY: &str = "identity";

/// Shared cell for resume validators: the caller seeds it with the validators
/// saved from a previous session, and the download path replaces them with the
/// server's current values once probed, so they can be persisted for the next
/// session.
pub(crate) type SharedValidators = Arc<RwLock<ResumeValidators>>;

fn log_progress_invariant(context: &str, progress: &DownloadProgress) {
    if let Some(total_size) = progress.total_size {
        if progress.completed_size > total_size {
            debug_assert!(
                progress.completed_size <= total_size,
                "{} progress exceeded total size: {} > {}",
                context,
                progress.completed_size,
                total_size
            );
            tracing::warn!(
                "{} progress exceeded total size: {} > {}",
                context,
                progress.completed_size,
                total_size
            );
        }
    }
}

pub(crate) fn partial_path_for(save_path: &Path) -> PathBuf {
    save_path.with_extension(
        save_path
            .extension()
            .map(|e| format!("{}.part", e.to_string_lossy()))
            .unwrap_or_else(|| "part".to_string()),
    )
}

/// Reject filenames that would escape the save directory.
///
/// Filenames can come from untrusted sources (Content-Disposition headers,
/// percent-decoded URL segments), so every download path must run this before
/// joining the name onto `save_dir`. Relative subpaths (`sub/file.bin`) are
/// allowed; `..`, absolute paths, and Windows prefixes are not.
pub(crate) fn validate_filename_components(final_filename: &str) -> Result<()> {
    use std::path::Component;
    if !Path::new(final_filename)
        .components()
        .any(|c| matches!(c, Component::Normal(_)))
    {
        return Err(EngineError::invalid_input(
            "filename",
            "Filename must name a file",
        ));
    }
    for component in Path::new(final_filename).components() {
        match component {
            Component::ParentDir => {
                return Err(EngineError::storage(
                    StorageErrorKind::PathTraversal,
                    Path::new(final_filename),
                    "Invalid filename: contains parent directory reference (..)",
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(EngineError::storage(
                    StorageErrorKind::PathTraversal,
                    Path::new(final_filename),
                    "Invalid filename: contains absolute path",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// HTTP Downloader
pub struct HttpDownloader {
    pool: Arc<ConnectionPool>,
    config: HttpDownloaderConfig,
    retry_policy: RetryPolicy,
}

/// Configuration for HTTP downloader
#[derive(Debug, Clone)]
pub struct HttpDownloaderConfig {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub max_redirects: usize,
    pub default_user_agent: String,
}

impl HttpDownloader {
    /// Create a new HTTP downloader
    pub fn new(config: &EngineConfig) -> Result<Self> {
        // Create connection pool with rate limiting if configured
        let pool = ConnectionPool::with_limits(
            &config.http,
            config.global_download_limit,
            config.global_upload_limit,
        )?;

        // Create retry policy from config
        let retry_policy = RetryPolicy::new(
            config.http.max_retries as u32,
            config.http.retry_delay_ms,
            config.http.max_retry_delay_ms,
        );

        Ok(Self {
            pool: Arc::new(pool),
            config: HttpDownloaderConfig {
                connect_timeout: Duration::from_secs(config.http.connect_timeout),
                read_timeout: Duration::from_secs(config.http.read_timeout),
                max_redirects: config.http.max_redirects,
                default_user_agent: config.user_agent.clone(),
            },
            retry_policy,
        })
    }

    /// Get the underlying client
    fn client(&self) -> &Client {
        self.pool.client()
    }

    /// Get the retry policy
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }

    /// Update the live HTTP bandwidth limits used by future requests.
    pub fn set_bandwidth_limits(&self, download_limit: Option<u64>, upload_limit: Option<u64>) {
        self.pool.set_download_limit(download_limit);
        self.pool.set_upload_limit(upload_limit);
    }

    /// The shared global (download, upload) limiters. The engine hands these
    /// to the torrent paths so a single global limit covers all traffic.
    pub fn limiters(
        &self,
    ) -> (
        Arc<crate::limiter::RateLimiter>,
        Arc<crate::limiter::RateLimiter>,
    ) {
        self.pool.limiters()
    }

    /// Download a file from a URL
    ///
    /// Returns the final path of the downloaded file
    #[allow(clippy::too_many_arguments)]
    pub async fn download<F>(
        &self,
        url: &str,
        save_dir: &Path,
        filename: Option<&str>,
        user_agent: Option<&str>,
        referer: Option<&str>,
        headers: &[(String, String)],
        cookies: Option<&[String]>,
        checksum: Option<&ExpectedChecksum>,
        cancel_token: CancellationToken,
        progress_callback: F,
    ) -> Result<PathBuf>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        self.download_with_scope(
            url,
            save_dir,
            filename,
            user_agent,
            referer,
            headers,
            cookies,
            checksum,
            #[cfg(feature = "recursive-http")]
            None,
            None,
            None,
            cancel_token,
            progress_callback,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn download_with_scope<F>(
        &self,
        url: &str,
        save_dir: &Path,
        filename: Option<&str>,
        user_agent: Option<&str>,
        referer: Option<&str>,
        headers: &[(String, String)],
        cookies: Option<&[String]>,
        checksum: Option<&ExpectedChecksum>,
        #[cfg(feature = "recursive-http")] redirect_scope: Option<
            crate::http::crawl::RedirectScope,
        >,
        validators: Option<SharedValidators>,
        download_limiter: Option<Arc<crate::limiter::RateLimiter>>,
        cancel_token: CancellationToken,
        progress_callback: F,
    ) -> Result<PathBuf>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        let progress_callback = Arc::new(progress_callback);
        // Validators saved from a previous session, for cross-restart resume
        // validation against the server's current ones.
        let saved_validators = validators.as_ref().map(|cell| cell.read().clone());
        // Build the request
        let mut request = self.client().get(url);

        // Set user agent
        let ua = user_agent.unwrap_or(&self.config.default_user_agent);
        request = request.header("User-Agent", ua);

        // Set referer if provided
        if let Some(ref_url) = referer {
            request = request.header("Referer", ref_url);
        }

        // Add custom headers
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request = request.header("Accept-Encoding", ACCEPT_ENCODING_IDENTITY);

        // Add cookies if provided
        if let Some(cookie_list) = cookies {
            if !cookie_list.is_empty() {
                let cookie_value = cookie_list.join("; ");
                request = request.header("Cookie", cookie_value);
            }
        }

        // Send HEAD request first to get metadata
        // Keep metadata probes out of the reusable GET connection pool.
        let mut head_request = self
            .client()
            .head(url)
            .header("Connection", "close")
            .header("User-Agent", ua);
        if let Some(referer) = referer {
            head_request = head_request.header("Referer", referer);
        }
        for (name, value) in headers {
            head_request = head_request.header(name.as_str(), value.as_str());
        }
        if let Some(cookie_list) = cookies {
            if !cookie_list.is_empty() {
                head_request = head_request.header("Cookie", cookie_list.join("; "));
            }
        }
        head_request = head_request.header("Accept-Encoding", ACCEPT_ENCODING_IDENTITY);
        let head_response = head_request.send().await;

        let (content_length, supports_range, suggested_filename, etag, last_modified) =
            match head_response {
                Ok(resp) if resp.status().is_success() => {
                    #[cfg(feature = "recursive-http")]
                    if let Some(scope) = redirect_scope.as_ref() {
                        crate::http::crawl::validate_redirect_scope(resp.url(), scope)?;
                    }
                    let length = resp
                        .headers()
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());

                    let supports_range = resp
                        .headers()
                        .get("accept-ranges")
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.contains("bytes"))
                        .unwrap_or(false);

                    // Try to get filename from Content-Disposition
                    let suggested = resp
                        .headers()
                        .get("content-disposition")
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_content_disposition);

                    let etag = resp
                        .headers()
                        .get("etag")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());

                    let last_modified = resp
                        .headers()
                        .get("last-modified")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());

                    (length, supports_range, suggested, etag, last_modified)
                }
                _ => {
                    // HEAD failed, we'll get metadata from GET response
                    (None, false, None, None, None)
                }
            };

        // Publish the server's current validators so the caller can persist
        // them for the next session. (On HEAD failure both stay None; the
        // saved copy taken above is what we validate against.)
        let current_validators = ResumeValidators {
            etag: etag.clone(),
            last_modified: last_modified.clone(),
        };
        if let Some(cell) = validators.as_ref() {
            *cell.write() = current_validators.clone();
        }

        // Check for cancellation
        if cancel_token.is_cancelled() {
            return Err(EngineError::Shutdown);
        }

        // Determine filename
        let final_filename = filename
            .map(|s| s.to_string())
            .or(suggested_filename)
            .or_else(|| extract_filename_from_url(url))
            .unwrap_or_else(|| "download".to_string());

        // Ensure save directory exists
        if !save_dir.exists() {
            tokio::fs::create_dir_all(save_dir).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    save_dir,
                    format!("Failed to create directory: {}", e),
                )
            })?;
        }

        // Validate filename for path traversal attacks (security)
        validate_filename_components(&final_filename)?;

        let save_path = save_dir.join(&final_filename);
        if let Some(parent) = save_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Use .part extension during download
        let part_path = partial_path_for(&save_path);

        // Check if we can resume
        let existing_size = if supports_range && part_path.exists() {
            tokio::fs::metadata(&part_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        let mut allow_resume = existing_size > 0;

        // Cross-restart validation: a partial file left by a previous session
        // must not be resumed if the remote file changed in the meantime —
        // If-Range against a *fresh* probe can't catch that, because the fresh
        // validator always matches the current remote file.
        if allow_resume {
            if let Some(saved) = saved_validators.as_ref() {
                if !resume::validators_match(saved, &current_validators) {
                    tracing::warn!(
                        "Remote file {} changed since the partial file was written \
                         (saved validators {:?}, current {:?}); restarting from byte 0",
                        url,
                        saved,
                        current_validators
                    );
                    allow_resume = false;
                }
            }
        }

        let mut stream_attempt = 0u32;

        loop {
            // Re-measure the partial file on every attempt: a mid-stream
            // failure leaves more bytes on disk than the pre-loop measurement
            // saw, and requesting a Range from a stale offset while appending
            // would silently corrupt the file.
            let on_disk_size = if supports_range && part_path.exists() {
                tokio::fs::metadata(&part_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0)
            } else {
                0
            };
            let resume_from = if allow_resume { on_disk_size } else { 0 };
            let if_range = if resume_from > 0 {
                etag.as_deref().or(last_modified.as_deref())
            } else {
                None
            };

            let mut attempt_request = request.try_clone().ok_or_else(|| {
                EngineError::Internal(
                    "Failed to clone HTTP request builder for restartless retry".to_string(),
                )
            })?;

            if resume_from > 0 {
                attempt_request =
                    attempt_request.header("Range", format!("bytes={}-", resume_from));
                if let Some(if_range_val) = if_range {
                    attempt_request = attempt_request.header("If-Range", if_range_val);
                }
            }

            // Send the request
            let response = attempt_request.send().await?;
            #[cfg(feature = "recursive-http")]
            if let Some(scope) = redirect_scope.as_ref() {
                crate::http::crawl::validate_redirect_scope(response.url(), scope)?;
            }

            // Check response status
            let status = response.status();
            if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                return Err(EngineError::network(
                    NetworkErrorKind::HttpStatus(status.as_u16()),
                    format!("HTTP error: {}", status),
                ));
            }

            if resume_from > 0 {
                let range_validation = resume::validate_ranged_response(
                    resume_from,
                    None,
                    status,
                    response
                        .headers()
                        .get("content-range")
                        .and_then(|v| v.to_str().ok()),
                    resume::RangedResponseContext {
                        sent_if_range: if_range.is_some(),
                        expected_etag: etag.as_deref(),
                        expected_last_modified: last_modified.as_deref(),
                        response_etag: response.headers().get("etag").and_then(|v| v.to_str().ok()),
                        response_last_modified: response
                            .headers()
                            .get("last-modified")
                            .and_then(|v| v.to_str().ok()),
                    },
                );

                if let Err(err) = range_validation {
                    if resume::should_restart_without_ranges(&err) {
                        tracing::warn!(
                            "HTTP resume for {} cannot continue safely ({}). Restarting from byte 0 with a single stream.",
                            url,
                            err
                        );
                        allow_resume = false;
                        continue;
                    }
                    return Err(err);
                }
            }

            // Get actual content length from response if not from HEAD
            let response_content_length = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|len| len + resume_from);

            if let (Some(head_len), Some(get_len)) = (content_length, response_content_length) {
                if head_len != get_len {
                    tracing::warn!(
                        "HEAD content-length mismatch for {}: HEAD={}, GET={}",
                        url,
                        head_len,
                        get_len
                    );
                }
            }

            // Prefer the GET response length over HEAD when available.
            let total_size = response_content_length.or(content_length);

            // Open file for writing
            let file = if resume_from > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT {
                // Append mode for resume
                OpenOptions::new()
                    .write(true)
                    .append(true)
                    .open(&part_path)
                    .await
                    .map_err(|e| {
                        EngineError::storage(
                            StorageErrorKind::Io,
                            &part_path,
                            format!("Failed to open file for append: {}", e),
                        )
                    })?
            } else {
                // Create a fresh partial file, truncating any stale resume data.
                File::create(&part_path).await.map_err(|e| {
                    EngineError::storage(
                        StorageErrorKind::Io,
                        &part_path,
                        format!("Failed to create file: {}", e),
                    )
                })?
            };

            // Download with progress tracking
            let downloaded = Arc::new(AtomicU64::new(resume_from));

            // Stream the response body
            let result = self
                .stream_to_file(
                    response,
                    file,
                    downloaded.clone(),
                    total_size,
                    download_limiter.clone(),
                    cancel_token.clone(),
                    {
                        let progress_callback = Arc::clone(&progress_callback);
                        move |completed, speed| {
                            let progress = DownloadProgress {
                                total_size,
                                completed_size: completed,
                                download_speed: speed,
                                upload_speed: 0,
                                connections: 1,
                                seeders: 0,
                                peers: 0,
                                eta_seconds: total_size.and_then(|total| {
                                    total.saturating_sub(completed).checked_div(speed)
                                }),
                            };
                            log_progress_invariant("http download", &progress);
                            progress_callback(progress);
                        }
                    },
                )
                .await;

            match result {
                Ok(_) => {
                    // Verify checksum before renaming (if checksum was provided)
                    if let Some(expected) = checksum {
                        let verified = verify_checksum(&part_path, expected).await?;
                        if !verified {
                            let actual = compute_checksum(&part_path, expected.algorithm).await?;
                            return Err(checksum::checksum_mismatch_error(
                                &expected.value,
                                &actual,
                            ));
                        }
                        tracing::debug!(
                            "Checksum verified: {} matches expected",
                            expected.algorithm
                        );
                    }

                    // Rename .part file to final name
                    tokio::fs::rename(&part_path, &save_path)
                        .await
                        .map_err(|e| {
                            EngineError::storage(
                                StorageErrorKind::Io,
                                &save_path,
                                format!("Failed to rename file: {}", e),
                            )
                        })?;

                    return Ok(save_path);
                }
                Err(e) => {
                    // Keep .part file for potential resume
                    if e.is_retryable() && self.retry_policy.should_retry(stream_attempt, &e) {
                        stream_attempt += 1;
                        let delay = self.retry_policy.delay_for_attempt(stream_attempt - 1);
                        if supports_range {
                            tracing::warn!(
                                "Stream error for {} (attempt {}/{}), will resume from partial: {}",
                                url,
                                stream_attempt,
                                self.retry_policy.max_attempts,
                                e
                            );
                            // Re-enter the loop: it will detect the .part file
                            // and send a Range request to resume from where we left off
                            allow_resume = true;
                        } else {
                            tracing::warn!(
                                "Stream error for {} (attempt {}/{}), restarting (no range support): {}",
                                url,
                                stream_attempt,
                                self.retry_policy.max_attempts,
                                e
                            );
                            // Server doesn't support ranges — must restart from byte 0
                            allow_resume = false;
                        }
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// Stream response body to file with progress tracking
    #[allow(clippy::too_many_arguments)]
    async fn stream_to_file<F>(
        &self,
        response: Response,
        mut file: File,
        downloaded: Arc<AtomicU64>,
        total_size: Option<u64>,
        extra_limiter: Option<Arc<crate::limiter::RateLimiter>>,
        cancel_token: CancellationToken,
        progress_callback: F,
    ) -> Result<()>
    where
        F: Fn(u64, u64) + Send,
    {
        let mut stream = response.bytes_stream();
        let mut last_update = Instant::now();
        let mut bytes_since_update: u64 = 0;
        let update_interval = Duration::from_millis(250); // Update progress 4 times per second

        while let Some(chunk_result) = tokio::select! {
            chunk = stream.next() => chunk,
            _ = cancel_token.cancelled() => {
                file.flush().await.ok();
                return Err(EngineError::Shutdown);
            }
        } {
            let chunk: bytes::Bytes = chunk_result.map_err(EngineError::from)?;

            let chunk_len = chunk.len() as u64;

            // Apply rate limiting if configured (global, then per-download)
            self.pool.acquire_download(chunk_len).await;
            if let Some(limiter) = extra_limiter.as_ref() {
                limiter.acquire(chunk_len).await;
            }

            // Write chunk to file
            file.write_all(&chunk).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    PathBuf::new(),
                    format!("Failed to write: {}", e),
                )
            })?;

            // Record downloaded bytes for stats
            self.pool.record_download(chunk_len);

            // Update counters
            let new_total = downloaded.fetch_add(chunk_len, Ordering::Relaxed) + chunk_len;
            if let Some(expected) = total_size {
                if new_total > expected {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::InvalidResponse,
                        format!(
                            "Response exceeded expected size: received {} bytes, expected {} bytes",
                            new_total, expected
                        ),
                    ));
                }
            }
            bytes_since_update += chunk_len;

            // Emit progress at intervals
            let now = Instant::now();
            if now.duration_since(last_update) >= update_interval {
                let elapsed_secs = now.duration_since(last_update).as_secs_f64();
                let speed = if elapsed_secs > 0.0 {
                    (bytes_since_update as f64 / elapsed_secs) as u64
                } else {
                    0
                };

                progress_callback(new_total, speed);

                last_update = now;
                bytes_since_update = 0;
            }
        }

        // Flush and sync
        file.flush().await.map_err(|e| {
            EngineError::storage(
                StorageErrorKind::Io,
                PathBuf::new(),
                format!("Failed to flush: {}", e),
            )
        })?;

        file.sync_all().await.map_err(|e| {
            EngineError::storage(
                StorageErrorKind::Io,
                PathBuf::new(),
                format!("Failed to sync: {}", e),
            )
        })?;

        // Final progress update
        let final_size = downloaded.load(Ordering::Relaxed);
        progress_callback(final_size, 0);

        // Validate received size matches expected (if known)
        if let Some(expected) = total_size {
            if final_size != expected {
                return Err(EngineError::protocol(
                    ProtocolErrorKind::InvalidResponse,
                    format!(
                        "Download size mismatch: received {} bytes, expected {} bytes",
                        final_size, expected
                    ),
                ));
            }
        }

        Ok(())
    }

    /// Download a file using multiple connections (segmented download)
    ///
    /// This method probes the server first and uses segmented downloads
    /// if the server supports Range requests and the file is large enough.
    #[allow(clippy::too_many_arguments)]
    /// Download with segmented multi-connection support.
    ///
    /// Returns the final path and optionally an Arc reference to the SegmentedDownload
    /// (only when using segmented download mode).
    pub async fn download_segmented<F>(
        &self,
        url: &str,
        save_dir: &Path,
        filename: Option<&str>,
        user_agent: Option<&str>,
        referer: Option<&str>,
        headers: &[(String, String)],
        cookies: Option<&[String]>,
        checksum: Option<&ExpectedChecksum>,
        max_connections: usize,
        min_segment_size: u64,
        cancel_token: CancellationToken,
        saved_segments: Option<Vec<Segment>>,
        progress_callback: F,
        segmented_ref: Option<Arc<RwLock<Option<Arc<SegmentedDownload>>>>>,
    ) -> Result<(PathBuf, Option<Arc<SegmentedDownload>>)>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        self.download_segmented_with_scope(
            url,
            save_dir,
            filename,
            user_agent,
            referer,
            headers,
            cookies,
            checksum,
            #[cfg(feature = "recursive-http")]
            None,
            max_connections,
            min_segment_size,
            cancel_token,
            saved_segments,
            None,
            None,
            None,
            progress_callback,
            segmented_ref,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn download_segmented_with_scope<F>(
        &self,
        url: &str,
        save_dir: &Path,
        filename: Option<&str>,
        user_agent: Option<&str>,
        referer: Option<&str>,
        headers: &[(String, String)],
        cookies: Option<&[String]>,
        checksum: Option<&ExpectedChecksum>,
        #[cfg(feature = "recursive-http")] redirect_scope: Option<
            crate::http::crawl::RedirectScope,
        >,
        max_connections: usize,
        min_segment_size: u64,
        cancel_token: CancellationToken,
        saved_segments: Option<Vec<Segment>>,
        validators: Option<SharedValidators>,
        download_limiter: Option<Arc<crate::limiter::RateLimiter>>,
        mirror_manager: Option<Arc<MirrorManager>>,
        progress_callback: F,
        segmented_ref: Option<Arc<RwLock<Option<Arc<SegmentedDownload>>>>>,
    ) -> Result<(PathBuf, Option<Arc<SegmentedDownload>>)>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        let progress_callback = Arc::new(progress_callback);
        let ua = user_agent.unwrap_or(&self.config.default_user_agent);
        let mut saved_segments = saved_segments;

        // Validators saved from a previous session, for cross-restart resume
        // validation against the server's current ones.
        let saved_validators = validators.as_ref().map(|cell| cell.read().clone());

        // Probe server capabilities
        let mut probe = self.client().head(url).header("User-Agent", ua);
        if let Some(referer) = referer {
            probe = probe.header("Referer", referer);
        }
        for (name, value) in headers {
            probe = probe.header(name.as_str(), value.as_str());
        }
        if let Some(cookies) = cookies.filter(|c| !c.is_empty()) {
            probe = probe.header("Cookie", cookies.join("; "));
        }
        let capabilities = segment::probe_request(probe).await.unwrap_or_else(|err| {
            tracing::debug!("HEAD probe failed for {} ({}); trying GET", url, err);
            ServerCapabilities {
                content_length: None,
                supports_range: false,
                etag: None,
                last_modified: None,
                suggested_filename: None,
            }
        });

        // NOTE: the shared cell must keep the *saved* validators until the
        // segmented-vs-single decision below — the single-connection fallback
        // re-reads the cell as its saved values, and publishing the fresh
        // probe here would make its comparison vacuously succeed.
        let current_validators = ResumeValidators {
            etag: capabilities.etag.clone(),
            last_modified: capabilities.last_modified.clone(),
        };

        // Cross-restart validation: saved segments describe bytes written by a
        // previous session; if the remote file changed since then, resuming
        // them would merge two versions of the file. If-Range with a freshly
        // probed validator cannot detect this.
        if saved_segments.is_some() {
            if let Some(saved) = saved_validators.as_ref() {
                if !resume::validators_match(saved, &current_validators) {
                    tracing::warn!(
                        "Remote file {} changed since segments were saved \
                         (saved validators {:?}, current {:?}); discarding saved segments",
                        url,
                        saved,
                        current_validators
                    );
                    saved_segments = None;
                }
            }
        }

        // Determine filename
        let final_filename = filename
            .map(|s| s.to_string())
            .or(capabilities.suggested_filename.clone())
            .or_else(|| extract_filename_from_url(url))
            .unwrap_or_else(|| "download".to_string());

        // Validate filename for path traversal attacks (security)
        validate_filename_components(&final_filename)?;

        // Ensure save directory exists
        if !save_dir.exists() {
            tokio::fs::create_dir_all(save_dir).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    save_dir,
                    format!("Failed to create directory: {}", e),
                )
            })?;
        }

        let save_path = save_dir.join(&final_filename);

        // Decide whether to use segmented download
        let use_segmented = capabilities.supports_range
            && capabilities
                .content_length
                .map(|l| l > min_segment_size)
                .unwrap_or(false);

        if use_segmented {
            // Publish the server's current validators so the caller can
            // persist them for the next session. (The single-connection
            // fallback below publishes its own from its own HEAD probe.)
            if let Some(cell) = validators.as_ref() {
                *cell.write() = current_validators.clone();
            }

            let total_size = capabilities.content_length.unwrap();

            // Create segmented download
            let mut download = SegmentedDownload::new(
                url.to_string(),
                total_size,
                save_path.clone(),
                true,
                capabilities.etag,
                capabilities.last_modified,
            );

            // Stripe segments across mirrors when more than one URL exists.
            if let Some(mirrors) = mirror_manager {
                if mirrors.available_count() > 1 {
                    download.set_mirror_manager(mirrors);
                }
            }

            // Restore or initialize segments. Saved segments are only valid
            // if the partial file still exists: restoring progress against a
            // fresh preallocated file would leave zero-filled holes for every
            // range the segments claim to have downloaded.
            let part_exists = tokio::fs::try_exists(partial_path_for(&save_path))
                .await
                .unwrap_or(false);
            match saved_segments {
                Some(segments) if part_exists => {
                    tracing::debug!("Restoring {} saved segments", segments.len());
                    download.restore_segments(segments);
                }
                Some(_) => {
                    tracing::warn!(
                        "Partial file for {} is missing; discarding saved segments and restarting",
                        url
                    );
                    download.init_segments(max_connections, min_segment_size);
                }
                None => {
                    download.init_segments(max_connections, min_segment_size);
                }
            }

            // Wrap in Arc for sharing
            let download = Arc::new(download);
            let download_ref = Arc::clone(&download);

            // Populate shared reference for external access during download (for persistence)
            if let Some(ref slot) = segmented_ref {
                *slot.write() = Some(Arc::clone(&download));
            }

            // Build headers vec
            let mut all_headers = headers.to_vec();
            if let Some(r) = referer {
                all_headers.push(("Referer".to_string(), r.to_string()));
            }
            // Add cookies to headers
            if let Some(cookie_list) = cookies {
                if !cookie_list.is_empty() {
                    all_headers.push(("Cookie".to_string(), cookie_list.join("; ")));
                }
            }

            // Start download. Segment tasks draw from the global limiter and
            // (if configured) the per-download one.
            let mut limiters = vec![self.pool.limiters().0];
            if let Some(l) = download_limiter.clone() {
                limiters.push(l);
            }
            let segmented_result = download
                .start_with_scope(
                    self.client(),
                    ua,
                    &all_headers,
                    max_connections,
                    &self.retry_policy,
                    #[cfg(feature = "recursive-http")]
                    redirect_scope.clone(),
                    limiters,
                    cancel_token.clone(),
                    {
                        let progress_callback = Arc::clone(&progress_callback);
                        move |progress| progress_callback(progress)
                    },
                )
                .await;

            if let Err(err) = segmented_result {
                if resume::should_restart_without_ranges(&err) && !cancel_token.is_cancelled() {
                    tracing::warn!(
                        "Segmented download for {} cannot continue safely ({}). Restarting from byte 0 with a single stream.",
                        url,
                        err
                    );
                    if let Some(ref slot) = segmented_ref {
                        *slot.write() = None;
                    }
                    resume::cleanup_partial(&partial_path_for(&save_path)).await?;
                    let path = self
                        .download_with_scope(
                            url,
                            save_dir,
                            Some(&final_filename),
                            user_agent,
                            referer,
                            headers,
                            cookies,
                            checksum,
                            #[cfg(feature = "recursive-http")]
                            redirect_scope,
                            validators,
                            download_limiter.clone(),
                            cancel_token,
                            {
                                let progress_callback = Arc::clone(&progress_callback);
                                move |progress| progress_callback(progress)
                            },
                        )
                        .await?;
                    return Ok((path, None));
                }
                return Err(err);
            }

            // Verify checksum if provided
            if let Some(expected) = checksum {
                let verified = verify_checksum(&save_path, expected).await?;
                if !verified {
                    let actual = compute_checksum(&save_path, expected.algorithm).await?;
                    return Err(checksum::checksum_mismatch_error(&expected.value, &actual));
                }
                tracing::debug!("Checksum verified: {} matches expected", expected.algorithm);
            }

            Ok((save_path, Some(download_ref)))
        } else {
            // Fall back to single-connection download
            let path = self
                .download_with_scope(
                    url,
                    save_dir,
                    Some(&final_filename),
                    user_agent,
                    referer,
                    headers,
                    cookies,
                    checksum,
                    #[cfg(feature = "recursive-http")]
                    redirect_scope,
                    validators,
                    download_limiter,
                    cancel_token,
                    {
                        let progress_callback = Arc::clone(&progress_callback);
                        move |progress| progress_callback(progress)
                    },
                )
                .await?;
            Ok((path, None))
        }
    }
}

/// Parse filename from Content-Disposition header
pub fn parse_content_disposition(header: &str) -> Option<String> {
    // Look for filename="..." or filename*=UTF-8''...
    if let Some(start) = header.find("filename=") {
        let rest = &header[start + 9..];
        if let Some(stripped) = rest.strip_prefix('"') {
            // Quoted filename
            let end = stripped.find('"')?;
            return Some(stripped[..end].to_string());
        } else {
            // Unquoted filename
            let end = rest.find(';').unwrap_or(rest.len());
            return Some(rest[..end].trim().to_string());
        }
    }

    if let Some(start) = header.find("filename*=") {
        let rest = &header[start + 10..];
        // UTF-8'' prefix
        if let Some(quote_start) = rest.find("''") {
            let encoded = &rest[quote_start + 2..];
            let end = encoded.find(';').unwrap_or(encoded.len());
            // URL decode
            if let Ok(decoded) = urlencoding::decode(&encoded[..end]) {
                return Some(decoded.to_string());
            }
        }
    }

    None
}

/// Extract filename from URL path
fn extract_filename_from_url(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()?
        .path_segments()?
        .next_back()
        .filter(|s| !s.is_empty())
        .map(|s| {
            // URL decode the filename
            urlencoding::decode(s)
                .map(|d| d.to_string())
                .unwrap_or_else(|_| s.to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_filename_components() {
        assert!(validate_filename_components("file.zip").is_ok());
        assert!(validate_filename_components("sub/dir/file.zip").is_ok());
        assert!(validate_filename_components("./file.zip").is_ok());

        // Traversal and absolute paths must be rejected — these arrive from
        // Content-Disposition headers and percent-decoded URL segments.
        assert!(validate_filename_components("../evil").is_err());
        assert!(validate_filename_components("a/../../evil").is_err());
        assert!(validate_filename_components("/etc/passwd").is_err());

        // Percent-decoded traversal from a URL segment (`..%2F..%2Fevil`).
        let decoded = urlencoding::decode("..%2F..%2Fevil").unwrap();
        assert!(validate_filename_components(&decoded).is_err());
    }

    #[test]
    fn test_parse_content_disposition() {
        assert_eq!(
            parse_content_disposition("attachment; filename=\"test.zip\""),
            Some("test.zip".to_string())
        );

        assert_eq!(
            parse_content_disposition("attachment; filename=test.zip"),
            Some("test.zip".to_string())
        );
    }

    #[test]
    fn test_extract_filename_from_url() {
        assert_eq!(
            extract_filename_from_url("https://example.com/path/to/file.zip"),
            Some("file.zip".to_string())
        );

        assert_eq!(
            extract_filename_from_url("https://example.com/path/to/file%20name.zip"),
            Some("file name.zip".to_string())
        );
    }
}
