//! WebSeed Support (BEP 19 / BEP 17)
//!
//! This module implements HTTP-based downloading of torrent pieces from web servers.
//! - BEP 19 (GetRight-style): URL points directly to the file content
//! - BEP 17 (Hoffman-style): URL is a seed server that understands torrent metadata
//!
//! WebSeeds download pieces in parallel with BitTorrent peers for maximum throughput.

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use reqwest::Client;
use tokio::sync::{mpsc, Semaphore};

use super::metainfo::{FileInfo, Metainfo, Sha1Hash};
use super::piece::PieceManager;
use crate::error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result};

/// WebSeed type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSeedType {
    /// BEP 19: GetRight-style - URL is the file itself
    GetRight,
    /// BEP 17: Hoffman-style - URL is a seed server
    Hoffman,
}

/// WebSeed connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSeedState {
    /// Ready to accept requests
    Idle,
    /// Currently downloading a piece
    Downloading,
    /// Temporarily unavailable (backing off after error)
    Backoff,
    /// Permanently failed (too many errors)
    Failed,
}

/// Statistics for a single web seed
#[derive(Debug, Clone, Default)]
pub struct WebSeedStats {
    /// Total bytes downloaded from this seed
    pub downloaded: u64,
    /// Number of successful piece downloads
    pub pieces_completed: u32,
    /// Number of failed requests
    pub failures: u32,
    /// Average download speed (bytes/sec)
    pub avg_speed: u64,
    /// Last error message
    pub last_error: Option<String>,
}

/// A single web seed source
pub struct WebSeed {
    /// Base URL for this web seed
    pub url: String,
    /// Type of web seed (BEP 19 or BEP 17)
    pub seed_type: WebSeedType,
    /// Current state
    state: RwLock<WebSeedState>,
    /// Statistics
    stats: RwLock<WebSeedStats>,
    /// Backoff until this time
    backoff_until: RwLock<Option<Instant>>,
    /// Consecutive failure count (for exponential backoff)
    consecutive_failures: AtomicU32,
    /// Currently downloading piece (if any)
    current_piece: RwLock<Option<u32>>,
}

impl WebSeed {
    /// Create a new web seed
    pub fn new(url: String, seed_type: WebSeedType) -> Self {
        Self {
            url,
            seed_type,
            state: RwLock::new(WebSeedState::Idle),
            stats: RwLock::new(WebSeedStats::default()),
            backoff_until: RwLock::new(None),
            consecutive_failures: AtomicU32::new(0),
            current_piece: RwLock::new(None),
        }
    }

    /// Check if seed is available for work
    pub fn is_available(&self) -> bool {
        let state = *self.state.read();

        match state {
            WebSeedState::Idle => true,
            WebSeedState::Backoff => {
                // Check if backoff period has passed
                if let Some(until) = *self.backoff_until.read() {
                    if Instant::now() >= until {
                        *self.state.write() = WebSeedState::Idle;
                        return true;
                    }
                }
                false
            }
            WebSeedState::Downloading | WebSeedState::Failed => false,
        }
    }

    /// Mark seed as downloading a piece
    pub fn set_downloading(&self, piece_index: u32) {
        *self.state.write() = WebSeedState::Downloading;
        *self.current_piece.write() = Some(piece_index);
    }

    /// Clear downloading status
    pub fn clear_downloading(&self) {
        *self.current_piece.write() = None;
        let state = *self.state.read();
        if state == WebSeedState::Downloading {
            *self.state.write() = WebSeedState::Idle;
        }
    }

    /// Record a successful download
    pub fn record_success(&self, bytes: u64) {
        let mut stats = self.stats.write();
        stats.downloaded += bytes;
        stats.pieces_completed += 1;

        // Reset consecutive failures on success
        self.consecutive_failures.store(0, Ordering::Relaxed);
        *self.state.write() = WebSeedState::Idle;
    }

    /// Record a failure and apply backoff
    pub fn record_failure(
        &self,
        error: &str,
        max_failures: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) {
        let mut stats = self.stats.write();
        stats.failures += 1;
        stats.last_error = Some(error.to_string());

        let consecutive = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;

        if consecutive >= max_failures {
            // Too many failures - disable this seed
            *self.state.write() = WebSeedState::Failed;
            tracing::warn!(
                "WebSeed {} disabled after {} consecutive failures",
                self.url,
                consecutive
            );
            return;
        }

        // Calculate backoff with exponential increase
        let backoff = initial_backoff * 2u32.pow((consecutive - 1).min(6));
        let backoff = if backoff > max_backoff {
            max_backoff
        } else {
            backoff
        };

        // Add jitter (±25%) and ensure we don't exceed max_backoff
        let jitter = (rand::random::<f64>() - 0.5) * 0.5;
        let backoff_ms = (backoff.as_millis() as f64 * (1.0 + jitter)) as u64;
        let backoff_ms = backoff_ms.min(max_backoff.as_millis() as u64);

        *self.backoff_until.write() = Some(Instant::now() + Duration::from_millis(backoff_ms));
        *self.state.write() = WebSeedState::Backoff;

        tracing::debug!(
            "WebSeed {} backing off for {}ms after failure: {}",
            self.url,
            backoff_ms,
            error
        );
    }

    /// Get current stats
    pub fn stats(&self) -> WebSeedStats {
        self.stats.read().clone()
    }

    /// Get current state
    pub fn state(&self) -> WebSeedState {
        *self.state.read()
    }
}

/// Message sent from WebSeedManager to coordinator
#[derive(Debug)]
pub enum WebSeedEvent {
    /// A piece was successfully downloaded and verified
    PieceComplete {
        piece_index: u32,
        data: Vec<u8>,
        source_url: String,
    },
    /// A piece download failed
    PieceFailed {
        piece_index: u32,
        source_url: String,
        error: String,
        retryable: bool,
    },
    /// Speed update for display
    SpeedUpdate { source_url: String, speed: u64 },
}

/// Configuration for web seed downloads
#[derive(Debug, Clone)]
pub struct WebSeedConfig {
    /// Maximum concurrent web seed connections
    pub max_connections: usize,
    /// Request timeout for piece downloads
    pub request_timeout: Duration,
    /// Initial backoff duration after failure
    pub initial_backoff: Duration,
    /// Maximum backoff duration
    pub max_backoff: Duration,
    /// Maximum consecutive failures before disabling seed
    pub max_failures: u32,
    /// User agent string
    pub user_agent: String,
}

impl Default for WebSeedConfig {
    fn default() -> Self {
        Self {
            max_connections: 4,
            request_timeout: Duration::from_secs(30),
            initial_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(300),
            max_failures: 5,
            user_agent: format!("gosh-dl/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

/// Manages all web seeds for a torrent
pub struct WebSeedManager {
    /// Metainfo reference
    metainfo: Arc<Metainfo>,
    /// Piece manager reference (for coordination)
    piece_manager: Arc<PieceManager>,
    /// HTTP client
    client: Client,
    /// Configuration
    config: WebSeedConfig,
    /// Active web seeds
    seeds: Vec<Arc<WebSeed>>,
    /// Event sender (to coordinator)
    event_tx: mpsc::Sender<WebSeedEvent>,
    /// Pieces being downloaded by webseeds
    webseed_pending: Arc<RwLock<HashSet<u32>>>,
    /// Shutdown flag
    shutdown: Arc<AtomicBool>,
    /// Total bytes downloaded via webseeds
    downloaded_bytes: Arc<AtomicU64>,
}

impl WebSeedManager {
    /// Create a new WebSeedManager
    ///
    /// Returns an error if the HTTP client cannot be created (e.g., due to
    /// invalid TLS configuration or system resource constraints).
    pub fn new(
        metainfo: Arc<Metainfo>,
        piece_manager: Arc<PieceManager>,
        config: WebSeedConfig,
    ) -> Result<(Self, mpsc::Receiver<WebSeedEvent>)> {
        let channel_capacity = config.max_connections * 2;
        let (event_tx, event_rx) = mpsc::channel(channel_capacity);

        // Build HTTP client
        let client = Client::builder()
            .read_timeout(config.request_timeout)
            .user_agent(&config.user_agent)
            .gzip(false)
            .brotli(false)
            .build()
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::Other,
                    format!("Failed to create HTTP client for WebSeed: {}", e),
                )
            })?;

        // Initialize seeds from metainfo
        let seeds = Self::seeds_from_metainfo(&metainfo);

        tracing::info!("WebSeedManager initialized with {} seeds", seeds.len());
        for seed in &seeds {
            tracing::debug!("  WebSeed: {}", seed.url);
        }

        Ok((
            Self {
                metainfo,
                piece_manager,
                client,
                config,
                seeds,
                event_tx,
                webseed_pending: Arc::new(RwLock::new(HashSet::new())),
                shutdown: Arc::new(AtomicBool::new(false)),
                downloaded_bytes: Arc::new(AtomicU64::new(0)),
            },
            event_rx,
        ))
    }

    /// Build the seed list from metainfo, preserving each URL's origin:
    /// `url-list` entries are BEP 19 (GetRight-style) file URLs, while
    /// `httpseeds` entries are BEP 17 (Hoffman-style) seed servers.
    fn seeds_from_metainfo(metainfo: &Metainfo) -> Vec<Arc<WebSeed>> {
        let mut seeds: Vec<Arc<WebSeed>> = metainfo
            .url_list
            .iter()
            .map(|url| Arc::new(WebSeed::new(url.clone(), WebSeedType::GetRight)))
            .collect();

        // Add httpseeds, avoiding duplicates with url-list entries
        for url in &metainfo.httpseeds {
            if seeds.iter().any(|s| s.url == *url) {
                continue;
            }
            seeds.push(Arc::new(WebSeed::new(url.clone(), WebSeedType::Hoffman)));
        }

        seeds
    }

    /// Start the web seed download loop
    /// Runs concurrently with peer downloads
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let semaphore = Arc::new(Semaphore::new(self.config.max_connections));

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                tracing::debug!("WebSeedManager shutting down");
                break;
            }

            // Check if download is complete
            if self.piece_manager.is_complete() {
                tracing::debug!("Download complete, WebSeedManager stopping");
                break;
            }

            // Find an idle seed and a needed piece
            if let Some((seed, piece_index)) = self.find_work() {
                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        // All connections busy, wait a bit
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };

                let this = Arc::clone(&self);
                let seed = Arc::clone(&seed);

                tokio::spawn(async move {
                    let _permit = permit;
                    this.download_piece(seed, piece_index).await;
                });
            } else {
                // No work available, wait briefly
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        Ok(())
    }

    /// Find an available seed and piece to download
    fn find_work(&self) -> Option<(Arc<WebSeed>, u32)> {
        for seed in &self.seeds {
            // Check if seed is available
            if !seed.is_available() {
                continue;
            }

            // Find a piece that needs downloading and isn't being
            // downloaded by peers or other webseeds
            if let Some(piece_index) = self.select_piece_for_webseed() {
                // Mark as pending
                self.webseed_pending.write().insert(piece_index);
                seed.set_downloading(piece_index);
                return Some((Arc::clone(seed), piece_index));
            }
        }

        None
    }

    /// Select a piece suitable for web seed download
    fn select_piece_for_webseed(&self) -> Option<u32> {
        // Get pieces we don't have and aren't being downloaded
        let have = self.piece_manager.bitfield();
        let pending = self.piece_manager.pending_pieces();
        let webseed_pending = self.webseed_pending.read();
        let num_pieces = self.metainfo.info.num_pieces();

        // Find first needed piece not being downloaded
        for i in 0..num_pieces {
            let idx = i as u32;
            if !have.get(i).map(|b| *b).unwrap_or(false)
                && !pending.contains(&idx)
                && !webseed_pending.contains(&idx)
                && self.piece_manager.is_piece_wanted(i)
            {
                return Some(idx);
            }
        }
        None
    }

    /// Download a single piece from a web seed
    async fn download_piece(&self, seed: Arc<WebSeed>, piece_index: u32) {
        let result = self.do_download_piece(&seed, piece_index).await;

        match result {
            Ok(data) => {
                let bytes = data.len() as u64;
                seed.record_success(bytes);
                self.downloaded_bytes.fetch_add(bytes, Ordering::Relaxed);

                let _ = self
                    .event_tx
                    .send(WebSeedEvent::PieceComplete {
                        piece_index,
                        data,
                        source_url: seed.url.clone(),
                    })
                    .await;
            }
            Err(e) => {
                let retryable = e.is_retryable();
                seed.record_failure(
                    &e.to_string(),
                    self.config.max_failures,
                    self.config.initial_backoff,
                    self.config.max_backoff,
                );
                let _ = self
                    .event_tx
                    .send(WebSeedEvent::PieceFailed {
                        piece_index,
                        source_url: seed.url.clone(),
                        error: e.to_string(),
                        retryable,
                    })
                    .await;
            }
        }

        // Clear pending status
        self.webseed_pending.write().remove(&piece_index);
        seed.clear_downloading();
    }

    /// Perform the actual HTTP download for a piece
    async fn do_download_piece(&self, seed: &WebSeed, piece_index: u32) -> Result<Vec<u8>> {
        // Calculate byte range for this piece
        let (start, end) = self
            .metainfo
            .piece_range(piece_index as usize)
            .ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::InvalidTorrent,
                    format!("Invalid piece index: {}", piece_index),
                )
            })?;

        let piece_length = end - start;

        // For GetRight multi-file torrents, check if the piece spans multiple files.
        // If so, use the cross-file download path which makes separate HTTP requests per file.
        if seed.seed_type == WebSeedType::GetRight && !self.metainfo.info.is_single_file {
            let files = self.metainfo.files_for_piece(piece_index as usize);
            if files.len() > 1 {
                return self.download_multifile_piece(seed, piece_index).await;
            }
        }

        // Build URL based on seed type and torrent structure
        let url = self.build_piece_url(seed, piece_index, start, end)?;

        tracing::debug!(
            "WebSeed downloading piece {} from {} (bytes {}-{})",
            piece_index,
            seed.url,
            start,
            end - 1
        );

        // Make HTTP request with Range header
        let response = self
            .client
            .get(&url)
            .header("Accept-Encoding", "identity")
            .header("Range", format!("bytes={}-{}", start, end - 1))
            .send()
            .await
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::Other,
                    format!("WebSeed request failed: {}", e),
                )
            })?;

        // Validate response
        let status = response.status();
        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            // Expected response for Range request
        } else if status.is_success() {
            // Server returned full content - we'll extract what we need
            tracing::debug!(
                "WebSeed returned {} instead of 206, handling full response",
                status
            );
        } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(EngineError::network(
                NetworkErrorKind::Other,
                "WebSeed does not support range requests",
            ));
        } else {
            return Err(EngineError::network(
                NetworkErrorKind::HttpStatus(status.as_u16()),
                format!("WebSeed HTTP error: {}", status),
            ));
        }

        // Read response body
        let data = response.bytes().await.map_err(|e| {
            EngineError::network(
                NetworkErrorKind::ConnectionReset,
                format!("Failed to read response: {}", e),
            )
        })?;

        // If server sent full content, we need to extract our piece
        let piece_data = if data.len() as u64 == piece_length {
            data.to_vec()
        } else if data.len() as u64 == self.metainfo.info.total_size {
            // Server sent entire file, extract our piece
            data[start as usize..end as usize].to_vec()
        } else if data.len() as u64 > piece_length {
            // Server sent more than we asked for, take first piece_length bytes
            data[..piece_length as usize].to_vec()
        } else {
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidResponse,
                format!(
                    "WebSeed returned wrong size: expected {}, got {}",
                    piece_length,
                    data.len()
                ),
            ));
        };

        // Verify the piece hash before returning
        let expected_hash = self
            .metainfo
            .piece_hash(piece_index as usize)
            .ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::InvalidTorrent,
                    format!("No hash for piece {}", piece_index),
                )
            })?;

        let actual_hash = Self::sha1_hash(&piece_data);

        if actual_hash != *expected_hash {
            return Err(EngineError::protocol(
                ProtocolErrorKind::HashMismatch,
                format!(
                    "WebSeed piece {} hash mismatch: expected {:?}, got {:?}",
                    piece_index, expected_hash, actual_hash
                ),
            ));
        }

        Ok(piece_data)
    }

    /// Build the URL for downloading a specific piece
    fn build_piece_url(
        &self,
        seed: &WebSeed,
        piece_index: u32,
        _start: u64,
        _end: u64,
    ) -> Result<String> {
        match seed.seed_type {
            WebSeedType::GetRight => {
                // BEP 19: For single-file torrents, URL is the file
                // (with the name appended if the URL is a directory).
                // For multi-file torrents, URL is the directory root.
                if self.metainfo.info.is_single_file {
                    Ok(getright_single_file_url(
                        &seed.url,
                        &self.metainfo.info.name,
                    ))
                } else {
                    // For multi-file, we need to determine which file(s)
                    // this piece spans and construct appropriate URL
                    self.build_multifile_url(seed, piece_index)
                }
            }
            WebSeedType::Hoffman => {
                // BEP 17: Append parameters to URL
                let info_hash = self.metainfo.info_hash_urlencoded();
                Ok(hoffman_piece_url(&seed.url, &info_hash, piece_index))
            }
        }
    }

    /// Build URL for a single file in a multi-file torrent (BEP 19 GetRight style)
    fn build_file_url(&self, seed: &WebSeed, file: &FileInfo) -> String {
        getright_multi_file_url(&seed.url, &self.metainfo.info.name, &file.path)
    }

    /// Build URL for multi-file torrent piece (BEP 19)
    ///
    /// Returns the URL for a single-file piece. For cross-file pieces,
    /// use `download_multifile_piece` which makes separate requests per file.
    fn build_multifile_url(&self, seed: &WebSeed, piece_index: u32) -> Result<String> {
        // Get files that this piece spans
        let files = self.metainfo.files_for_piece(piece_index as usize);

        if files.is_empty() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                format!("No files for piece {}", piece_index),
            ));
        }

        if files.len() == 1 {
            // Piece is entirely within one file
            let (file_idx, _file_offset, _length) = files[0];
            let file = &self.metainfo.info.files[file_idx];
            Ok(self.build_file_url(seed, file))
        } else {
            // Cross-file pieces are handled by download_multifile_piece()
            Err(EngineError::protocol(
                ProtocolErrorKind::InvalidResponse,
                format!("Piece {} spans {} files", piece_index, files.len()),
            ))
        }
    }

    /// Download a piece that spans multiple files via separate HTTP requests (BEP 19)
    async fn download_multifile_piece(&self, seed: &WebSeed, piece_index: u32) -> Result<Vec<u8>> {
        let files = self.metainfo.files_for_piece(piece_index as usize);
        let mut piece_data = Vec::new();

        for (file_idx, file_offset, length) in &files {
            let file = &self.metainfo.info.files[*file_idx];
            let url = self.build_file_url(seed, file);
            let end_byte = file_offset + length - 1;

            tracing::debug!(
                "WebSeed cross-file: piece {} file {} bytes {}-{}",
                piece_index,
                file.path.display(),
                file_offset,
                end_byte
            );

            let response = self
                .client
                .get(&url)
                .header("Accept-Encoding", "identity")
                .header("Range", format!("bytes={}-{}", file_offset, end_byte))
                .send()
                .await
                .map_err(|e| {
                    EngineError::network(
                        NetworkErrorKind::Other,
                        format!("WebSeed cross-file request failed: {}", e),
                    )
                })?;

            let status = response.status();
            if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
                return Err(EngineError::network(
                    NetworkErrorKind::HttpStatus(status.as_u16()),
                    format!(
                        "WebSeed HTTP error for file {}: {}",
                        file.path.display(),
                        status
                    ),
                ));
            }

            let data = response.bytes().await.map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    format!("Failed to read cross-file response: {}", e),
                )
            })?;

            // A compliant server answers the Range request with 206 and only
            // the requested bytes. A server that ignores Range replies 200
            // with the whole file, so the requested window must be sliced
            // out starting at `file_offset`, not from the start of the body.
            let chunk = extract_file_range(
                &data,
                status == reqwest::StatusCode::PARTIAL_CONTENT,
                *file_offset,
                *length,
            )?;
            piece_data.extend_from_slice(chunk);
        }

        // Verify the piece hash
        let expected_hash = self
            .metainfo
            .piece_hash(piece_index as usize)
            .ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::InvalidTorrent,
                    format!("No hash for piece {}", piece_index),
                )
            })?;

        let actual_hash = Self::sha1_hash(&piece_data);
        if actual_hash != *expected_hash {
            return Err(EngineError::protocol(
                ProtocolErrorKind::HashMismatch,
                format!("WebSeed cross-file piece {} hash mismatch", piece_index),
            ));
        }

        Ok(piece_data)
    }

    /// Calculate SHA-1 hash
    fn sha1_hash(data: &[u8]) -> Sha1Hash {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(data);
        hasher.finalize().into()
    }

    /// Shutdown the web seed manager
    ///
    /// Idempotent and cheap: sets an atomic flag that the download loop
    /// checks, so it is safe to call multiple times (e.g. from `stop()`).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Get total bytes downloaded via webseeds
    pub fn downloaded_bytes(&self) -> u64 {
        self.downloaded_bytes.load(Ordering::Relaxed)
    }

    /// Get stats for all web seeds
    pub fn all_stats(&self) -> Vec<(String, WebSeedStats)> {
        self.seeds
            .iter()
            .map(|s| (s.url.clone(), s.stats()))
            .collect()
    }

    /// Check if there are any active web seeds
    pub fn has_seeds(&self) -> bool {
        !self.seeds.is_empty()
    }

    /// Get number of active (non-failed) web seeds
    pub fn active_seed_count(&self) -> usize {
        self.seeds
            .iter()
            .filter(|s| s.state() != WebSeedState::Failed)
            .count()
    }
}

/// Build the request URL for a single-file torrent (BEP 19 GetRight style)
///
/// Per BEP 19, a base URL ending in `/` is a directory and the torrent name
/// is appended; otherwise the URL already points at the file itself.
fn getright_single_file_url(base_url: &str, name: &str) -> String {
    if base_url.ends_with('/') {
        format!("{}{}", base_url, urlencoding::encode(name))
    } else {
        base_url.to_string()
    }
}

/// Build the request URL for one file of a multi-file torrent (BEP 19
/// GetRight style)
///
/// Per BEP 19, multi-file torrents append `{name}/{file path}` to the base
/// URL, where `name` is the torrent's top-level directory.
fn getright_multi_file_url(base_url: &str, name: &str, file_path: &Path) -> String {
    // URL encode the path components
    let encoded_path = file_path
        .to_string_lossy()
        .split(std::path::MAIN_SEPARATOR)
        .map(|p| urlencoding::encode(p).into_owned())
        .collect::<Vec<_>>()
        .join("/");

    let base = base_url.trim_end_matches('/');
    let encoded_name = urlencoding::encode(name);
    format!("{}/{}/{}", base, encoded_name, encoded_path)
}

/// Build the request URL for a piece from a seed server (BEP 17 Hoffman style)
fn hoffman_piece_url(base_url: &str, info_hash_urlencoded: &str, piece_index: u32) -> String {
    format!(
        "{}?info_hash={}&piece={}",
        base_url, info_hash_urlencoded, piece_index
    )
}

/// Extract the requested byte window from a per-file web seed response body
///
/// A 206 (Partial Content) body must be exactly the requested range. A 200
/// body is the whole file starting at offset 0, so the window starting at
/// `file_offset` is sliced out. Any other size is an error.
fn extract_file_range(
    data: &[u8],
    is_partial_content: bool,
    file_offset: u64,
    length: u64,
) -> Result<&[u8]> {
    if is_partial_content {
        if data.len() as u64 != length {
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidResponse,
                format!(
                    "WebSeed returned wrong range size: expected {}, got {}",
                    length,
                    data.len()
                ),
            ));
        }
        Ok(data)
    } else {
        let range_end = file_offset + length;
        if (data.len() as u64) < range_end {
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidResponse,
                format!(
                    "WebSeed full response too short: need {} bytes, got {}",
                    range_end,
                    data.len()
                ),
            ));
        }
        // range_end <= data.len() <= usize::MAX, so these casts are lossless
        Ok(&data[file_offset as usize..range_end as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webseed_state_transitions() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );

        // Initially idle
        assert_eq!(seed.state(), WebSeedState::Idle);
        assert!(seed.is_available());

        // Set downloading
        seed.set_downloading(0);
        assert_eq!(seed.state(), WebSeedState::Downloading);
        assert!(!seed.is_available());

        // Clear downloading
        seed.clear_downloading();
        assert_eq!(seed.state(), WebSeedState::Idle);
        assert!(seed.is_available());
    }

    #[test]
    fn test_webseed_backoff() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );

        // Record failure
        seed.record_failure(
            "test error",
            5,
            Duration::from_millis(100),
            Duration::from_secs(10),
        );

        assert_eq!(seed.state(), WebSeedState::Backoff);
        assert!(!seed.is_available());

        let stats = seed.stats();
        assert_eq!(stats.failures, 1);
        assert_eq!(stats.last_error, Some("test error".to_string()));
    }

    #[test]
    fn test_webseed_success_resets_failures() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );

        // Record some failures
        seed.record_failure(
            "error 1",
            5,
            Duration::from_millis(100),
            Duration::from_secs(10),
        );
        assert_eq!(seed.consecutive_failures.load(Ordering::Relaxed), 1);

        // Force state to idle for testing
        *seed.state.write() = WebSeedState::Idle;

        // Record success
        seed.record_success(1000);

        // Consecutive failures should be reset
        assert_eq!(seed.consecutive_failures.load(Ordering::Relaxed), 0);
        assert_eq!(seed.state(), WebSeedState::Idle);

        let stats = seed.stats();
        assert_eq!(stats.downloaded, 1000);
        assert_eq!(stats.pieces_completed, 1);
    }

    #[test]
    fn test_webseed_max_failures() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );

        // Record max failures
        for i in 0..5 {
            *seed.state.write() = WebSeedState::Idle; // Reset for testing
            seed.record_failure(
                &format!("error {}", i),
                5,
                Duration::from_millis(100),
                Duration::from_secs(10),
            );
        }

        // Should be permanently failed
        assert_eq!(seed.state(), WebSeedState::Failed);
        assert!(!seed.is_available());
    }

    // ========================================================================
    // WebSeed Fallback Behavior Tests
    // ========================================================================

    #[test]
    fn test_webseed_backoff_exponential() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );
        let initial_delay = Duration::from_millis(100);
        let max_delay = Duration::from_secs(10);

        // First failure
        seed.record_failure("error 1", 5, initial_delay, max_delay);
        let backoff1 = {
            let guard = seed.backoff_until.read();
            guard.unwrap()
        };

        // Reset state for next test
        *seed.state.write() = WebSeedState::Idle;
        *seed.backoff_until.write() = None;

        // Second failure - should have longer backoff
        seed.record_failure("error 2", 5, initial_delay, max_delay);
        let backoff2 = {
            let guard = seed.backoff_until.read();
            guard.unwrap()
        };

        // Backoff should increase (exponential)
        // With 2 consecutive failures, delay is min(100 * 2^1, 10000) = 200ms
        assert!(
            backoff2 > backoff1,
            "Backoff should increase with more failures"
        );
    }

    #[test]
    fn test_webseed_backoff_respects_max() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );
        let initial_delay = Duration::from_secs(1);
        let max_delay = Duration::from_secs(5);

        // Simulate many failures to hit max delay
        for _ in 0..10 {
            *seed.state.write() = WebSeedState::Idle;
            *seed.backoff_until.write() = None;
            seed.record_failure("error", 20, initial_delay, max_delay);
        }

        // Check that backoff doesn't exceed max
        let until_opt = {
            let guard = seed.backoff_until.read();
            *guard
        };
        if let Some(until) = until_opt {
            let now = Instant::now();
            let backoff_duration = until.saturating_duration_since(now);
            assert!(
                backoff_duration <= max_delay + Duration::from_millis(100), // Small tolerance
                "Backoff should not exceed max_delay"
            );
        }
    }

    #[test]
    fn test_webseed_types() {
        let getright = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );
        let hoffman = WebSeed::new("http://seed.example.com".to_string(), WebSeedType::Hoffman);

        assert_eq!(getright.seed_type, WebSeedType::GetRight);
        assert_eq!(hoffman.seed_type, WebSeedType::Hoffman);
    }

    #[test]
    fn test_webseed_failed_state_not_available() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );

        // Set to failed
        *seed.state.write() = WebSeedState::Failed;
        assert_eq!(seed.state(), WebSeedState::Failed);
        assert!(!seed.is_available(), "Failed seed should not be available");
    }

    #[test]
    fn test_webseed_recovery_from_backoff() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );

        // Set to backoff with expired time
        *seed.state.write() = WebSeedState::Backoff;
        *seed.backoff_until.write() = Some(Instant::now() - Duration::from_secs(1));

        // Should be available since backoff expired
        assert!(
            seed.is_available(),
            "Seed with expired backoff should be available"
        );
    }

    #[test]
    fn test_webseed_stats_accumulation() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );

        // Record multiple successes
        seed.record_success(1000);
        seed.record_success(2000);
        seed.record_success(3000);

        let stats = seed.stats();
        assert_eq!(stats.downloaded, 6000);
        assert_eq!(stats.pieces_completed, 3);
    }

    #[test]
    fn test_webseed_piece_tracking() {
        let seed = WebSeed::new(
            "http://example.com/file.iso".to_string(),
            WebSeedType::GetRight,
        );

        // Set current piece
        *seed.current_piece.write() = Some(42);
        assert_eq!(*seed.current_piece.read(), Some(42));

        // Clear current piece
        *seed.current_piece.write() = None;
        assert_eq!(*seed.current_piece.read(), None);
    }

    #[test]
    fn test_webseed_config_defaults() {
        let config = super::WebSeedConfig::default();
        assert_eq!(config.max_connections, 4);
        assert_eq!(config.request_timeout, Duration::from_secs(30));
        assert_eq!(config.max_failures, 5);
    }

    #[test]
    fn test_webseed_url_preservation() {
        let url = "http://example.com/path/to/torrent/file.iso";
        let seed = WebSeed::new(url.to_string(), WebSeedType::GetRight);
        assert_eq!(seed.url, url);
    }

    // ========================================================================
    // URL Construction Tests (BEP 19 / BEP 17)
    // ========================================================================

    use crate::torrent::metainfo::Info;
    use std::path::PathBuf;

    fn test_metainfo(url_list: Vec<String>, httpseeds: Vec<String>) -> Metainfo {
        Metainfo {
            info_hash: [0u8; 20],
            info: Info {
                name: "linux.iso".to_string(),
                piece_length: 32 * 1024,
                pieces: vec![[0u8; 20]],
                files: vec![FileInfo {
                    path: PathBuf::from("linux.iso"),
                    length: 100,
                    offset: 0,
                    md5sum: None,
                }],
                total_size: 100,
                is_single_file: true,
                private: false,
            },
            announce: None,
            announce_list: Vec::new(),
            creation_date: None,
            comment: None,
            created_by: None,
            encoding: None,
            url_list,
            httpseeds,
        }
    }

    #[test]
    fn test_bep19_single_file_url_with_trailing_slash() {
        // URL ends in '/': the torrent name is appended
        assert_eq!(
            getright_single_file_url("http://example.com/dl/", "linux.iso"),
            "http://example.com/dl/linux.iso"
        );
    }

    #[test]
    fn test_bep19_single_file_url_without_trailing_slash() {
        // URL is the complete file URL and is used as-is
        assert_eq!(
            getright_single_file_url("http://example.com/dl/linux.iso", "linux.iso"),
            "http://example.com/dl/linux.iso"
        );
    }

    #[test]
    fn test_bep19_single_file_url_encodes_name() {
        assert_eq!(
            getright_single_file_url("http://example.com/dl/", "my file.iso"),
            "http://example.com/dl/my%20file.iso"
        );
    }

    #[test]
    fn test_bep19_multi_file_url_includes_name_dir() {
        let path = PathBuf::from("sub").join("a.bin");
        assert_eq!(
            getright_multi_file_url("http://example.com/dl/", "My Torrent", &path),
            "http://example.com/dl/My%20Torrent/sub/a.bin"
        );
        // A base URL without a trailing slash is treated the same way
        assert_eq!(
            getright_multi_file_url("http://example.com/dl", "My Torrent", &path),
            "http://example.com/dl/My%20Torrent/sub/a.bin"
        );
    }

    #[test]
    fn test_bep17_hoffman_url() {
        assert_eq!(
            hoffman_piece_url("http://seed.example.com/seed.php", "%AA%BB", 7),
            "http://seed.example.com/seed.php?info_hash=%AA%BB&piece=7"
        );
    }

    #[test]
    fn test_seed_types_from_metainfo_lists() {
        // url-list entries are BEP 19 (GetRight), httpseeds are BEP 17 (Hoffman)
        let metainfo = test_metainfo(
            vec!["http://mirror.example.com/linux.iso".to_string()],
            vec!["http://seed.example.com/seed.php".to_string()],
        );
        let seeds = WebSeedManager::seeds_from_metainfo(&metainfo);
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].url, "http://mirror.example.com/linux.iso");
        assert_eq!(seeds[0].seed_type, WebSeedType::GetRight);
        assert_eq!(seeds[1].url, "http://seed.example.com/seed.php");
        assert_eq!(seeds[1].seed_type, WebSeedType::Hoffman);
    }

    #[test]
    fn test_seed_types_dedup_prefers_getright() {
        let url = "http://both.example.com/".to_string();
        let metainfo = test_metainfo(vec![url.clone()], vec![url]);
        let seeds = WebSeedManager::seeds_from_metainfo(&metainfo);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].seed_type, WebSeedType::GetRight);
    }

    // ========================================================================
    // Cross-File Response Handling Tests
    // ========================================================================

    #[test]
    fn test_extract_file_range_206_exact() {
        let data = vec![10u8, 11, 12, 13];
        let chunk = extract_file_range(&data, true, 100, 4).unwrap();
        assert_eq!(chunk, &data[..]);
    }

    #[test]
    fn test_extract_file_range_206_wrong_size_is_error() {
        // Too short
        let data = vec![10u8, 11, 12];
        assert!(extract_file_range(&data, true, 100, 4).is_err());
        // Too long
        let data = vec![0u8; 10];
        assert!(extract_file_range(&data, true, 100, 4).is_err());
    }

    #[test]
    fn test_extract_file_range_200_slices_from_file_offset() {
        // Server ignored the Range header and returned the whole file:
        // the requested window starts at file_offset, not at offset 0
        let data: Vec<u8> = (0u8..100).collect();
        let chunk = extract_file_range(&data, false, 60, 20).unwrap();
        assert_eq!(chunk, &data[60..80]);
    }

    #[test]
    fn test_extract_file_range_200_too_short_is_error() {
        let data = vec![0u8; 50];
        assert!(extract_file_range(&data, false, 60, 20).is_err());
    }
}
