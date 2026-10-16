//! Download Engine - Main coordinator
//!
//! The `DownloadEngine` is the primary entry point for the library.
//! It manages all downloads, coordinates between HTTP and BitTorrent
//! engines, handles persistence, and emits events.

use crate::config::EngineConfig;
use crate::error::{EngineError, Result};
#[cfg(feature = "http")]
use crate::http::{HttpDownloader, MirrorManager, SegmentedDownload};
use crate::priority_queue::{DownloadPriority, PriorityQueue};
use crate::scheduler::{BandwidthLimits, BandwidthScheduler};
#[cfg(feature = "storage")]
use crate::storage::SqliteStorage;
use crate::storage::{Segment, Storage};
#[cfg(feature = "torrent")]
use crate::torrent::{MagnetUri, Metainfo, TorrentConfig, TorrentDownloader};
use crate::types::{
    DownloadEvent, DownloadId, DownloadOptions, DownloadState, DownloadStatus, GlobalStats,
};
#[cfg(any(feature = "http", feature = "torrent"))]
use crate::types::{DownloadKind, DownloadMetadata, DownloadProgress};
#[cfg(feature = "torrent")]
use crate::types::{TorrentFile, TorrentStatusInfo};

#[cfg(all(feature = "http", feature = "recursive-http"))]
use crate::http::crawl;
#[cfg(any(feature = "http", feature = "torrent"))]
use chrono::Utc;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(all(feature = "http", feature = "recursive-http"))]
use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::broadcast;
#[cfg(feature = "http")]
use url::Url;
#[cfg(all(feature = "http", feature = "recursive-http"))]
use uuid::Uuid;

/// Maximum number of events to buffer
const EVENT_CHANNEL_CAPACITY: usize = 1024;
#[cfg(all(feature = "http", feature = "recursive-http"))]
const RECURSIVE_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Internal representation of a managed download
struct ManagedDownload {
    status: DownloadStatus,
    handle: Option<DownloadHandle>,
    /// The options this download was added with. Kept (and persisted) so
    /// resume — in-session or after a restart — keeps file selection,
    /// sequential mode, connection caps, and speed limits instead of
    /// rebuilding them from defaults.
    options: Option<DownloadOptions>,
    /// Metainfo retained for torrents added paused without persistent storage.
    #[cfg(feature = "torrent")]
    cached_torrent_data: Option<Vec<u8>>,
    /// Lifetime torrent transfer totals restored from storage, consumed when
    /// the torrent (re)starts.
    restored_totals: Option<(u64, u64)>,
    /// Cached HTTP segment state for in-memory pause/resume (no storage needed)
    #[cfg(feature = "http")]
    cached_segments: Option<Vec<Segment>>,
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    redirect_scope: Option<crawl::RedirectScope>,
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    recursive_group_id: Option<Uuid>,
}

#[cfg(all(feature = "http", feature = "recursive-http"))]
struct RecursiveGroup {
    child_ids: HashSet<DownloadId>,
    fail_fast: bool,
    failed: bool,
}

#[cfg(all(feature = "http", feature = "recursive-http"))]
struct RecursiveFailFastAbort {
    id: DownloadId,
    old_state: DownloadState,
    error_message: String,
    status: DownloadStatus,
    cancel_token: Option<tokio_util::sync::CancellationToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedDownloadRuntimeMetadata {
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    #[serde(default)]
    recursive_child: Option<PersistedRecursiveChildState>,
    /// The download's original options, persisted so crash recovery does not
    /// silently drop file selection, sequential mode, or speed limits.
    #[serde(default)]
    options: Option<DownloadOptions>,
    /// Lifetime (downloaded, uploaded) byte totals for torrents, so
    /// seed-ratio decisions survive restarts.
    #[serde(default)]
    torrent_totals: Option<(u64, u64)>,
}

#[cfg(all(feature = "http", feature = "recursive-http"))]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedRecursiveChildState {
    redirect_scope: crawl::PersistedRedirectScope,
    recursive_group_id: Option<Uuid>,
    fail_fast: bool,
}

/// Handle to control a running download
#[allow(dead_code)]
enum DownloadHandle {
    #[cfg(feature = "http")]
    Http(HttpDownloadHandle),
    #[cfg(feature = "torrent")]
    Torrent(TorrentDownloadHandle),
}

/// Handle for an HTTP download task
#[cfg(feature = "http")]
struct HttpDownloadHandle {
    cancel_token: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<Result<()>>,
    /// Reference to segmented download for persistence (if using segmented download).
    /// Wrapped in RwLock so it can be populated from inside the spawned task.
    segmented_download: Arc<RwLock<Option<Arc<SegmentedDownload>>>>,
}

/// Handle for a torrent download
#[cfg(feature = "torrent")]
struct TorrentDownloadHandle {
    downloader: Arc<TorrentDownloader>,
    task: tokio::task::JoinHandle<Result<()>>,
    progress_task: tokio::task::JoinHandle<()>,
}

/// Per-download outcomes of a batch operation such as
/// [`DownloadEngine::pause_all`] or [`DownloadEngine::resume_all`].
#[derive(Debug, Default)]
pub struct BatchResult {
    /// Downloads the operation was applied to successfully.
    pub succeeded: Vec<DownloadId>,
    /// Downloads skipped because their state changed between snapshot and
    /// action (e.g. a download completed while `pause_all` was running).
    pub skipped: Vec<DownloadId>,
    /// Downloads where the operation failed with a real error.
    pub failed: Vec<(DownloadId, EngineError)>,
}

/// The main download engine
pub struct DownloadEngine {
    /// Weak self-reference for spawning background tasks from `&self` methods
    self_ref: Weak<Self>,

    /// Configuration
    config: RwLock<EngineConfig>,

    /// All managed downloads
    downloads: RwLock<HashMap<DownloadId, ManagedDownload>>,
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    recursive_groups: RwLock<HashMap<Uuid, RecursiveGroup>>,
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    recursive_jobs: RwLock<HashMap<Uuid, crate::types::TrackedRecursiveJob>>,
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    recursive_job_membership: RwLock<HashMap<DownloadId, HashSet<Uuid>>>,

    /// HTTP downloader
    #[cfg(feature = "http")]
    http: Arc<HttpDownloader>,

    /// Event broadcaster
    event_tx: broadcast::Sender<DownloadEvent>,
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    recursive_job_event_tx: broadcast::Sender<crate::types::RecursiveJobEvent>,

    /// Priority queue for limiting and ordering concurrent downloads
    priority_queue: Arc<PriorityQueue>,

    /// Bandwidth scheduler for time-based limits
    scheduler: Arc<RwLock<BandwidthScheduler>>,

    /// Global download rate limiter shared by every transfer path
    global_download_limiter: Arc<crate::limiter::RateLimiter>,
    /// Global upload rate limiter shared by every transfer path
    global_upload_limiter: Arc<crate::limiter::RateLimiter>,

    /// Shutdown flag
    shutdown: tokio_util::sync::CancellationToken,

    /// Persistent storage for download state
    storage: Option<Arc<dyn Storage>>,
}

impl DownloadEngine {
    /// Obtain a strong `Arc<Self>` reference for spawning background tasks.
    fn arc(&self) -> Result<Arc<Self>> {
        self.self_ref.upgrade().ok_or(EngineError::Shutdown)
    }

    /// Create a new download engine with the given configuration
    ///
    /// When the `storage` feature is enabled and `config.database_path` is
    /// set, download state is persisted to the built-in SQLite storage. To
    /// use a custom [`Storage`] implementation instead, see
    /// [`with_storage`](Self::with_storage).
    pub async fn new(config: EngineConfig) -> Result<Arc<Self>> {
        // Validate configuration
        config.validate()?;

        // Initialize persistent storage
        #[cfg(feature = "storage")]
        let storage: Option<Arc<dyn Storage>> = if let Some(ref db_path) = config.database_path {
            // A configured database that fails to open is a hard error:
            // downgrading to a warning silently runs the engine with no
            // persistence at all, which callers have no way to detect.
            Some(Arc::new(SqliteStorage::new(db_path).await?))
        } else {
            None
        };
        #[cfg(not(feature = "storage"))]
        let storage: Option<Arc<dyn Storage>> = None;

        Self::build(config, storage).await
    }

    /// Create a new download engine that persists state to the given
    /// [`Storage`] implementation.
    ///
    /// This allows applications that maintain their own metadata store
    /// (custom database, sidecar files, ...) to get full pause/resume and
    /// crash-recovery support without the built-in SQLite storage or the
    /// `storage` feature. Previously persisted downloads are loaded from
    /// the given storage during construction.
    ///
    /// `config.database_path` is ignored when this constructor is used; the
    /// injected storage always wins.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use gosh_dl::{DownloadEngine, EngineConfig, MemoryStorage};
    ///
    /// # async fn example() -> gosh_dl::Result<()> {
    /// let storage = Arc::new(MemoryStorage::new());
    /// let engine = DownloadEngine::with_storage(EngineConfig::default(), storage).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn with_storage(
        config: EngineConfig,
        storage: Arc<dyn Storage>,
    ) -> Result<Arc<Self>> {
        config.validate()?;

        if config.database_path.is_some() {
            tracing::warn!(
                "Both an injected storage and config.database_path are set; \
                 using the injected storage and ignoring database_path"
            );
        }

        Self::build(config, Some(storage)).await
    }

    /// Shared construction path for [`new`](Self::new) and
    /// [`with_storage`](Self::with_storage). Expects a validated config.
    async fn build(config: EngineConfig, storage: Option<Arc<dyn Storage>>) -> Result<Arc<Self>> {
        // Create event channel
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        #[cfg(all(feature = "http", feature = "recursive-http"))]
        let (recursive_job_event_tx, _) = broadcast::channel(RECURSIVE_EVENT_CHANNEL_CAPACITY);

        // Create HTTP downloader
        #[cfg(feature = "http")]
        let http = Arc::new(HttpDownloader::new(&config)?);

        // One global limiter pair governs all traffic. With the http feature
        // it's the pool's pair (already seeded from config); otherwise the
        // engine owns a standalone pair for the torrent paths.
        #[cfg(feature = "http")]
        let (global_download_limiter, global_upload_limiter) = http.limiters();
        #[cfg(not(feature = "http"))]
        let (global_download_limiter, global_upload_limiter) = (
            Arc::new(crate::limiter::RateLimiter::new(
                config.global_download_limit,
            )),
            Arc::new(crate::limiter::RateLimiter::new(config.global_upload_limit)),
        );

        // Create priority queue for concurrent download limiting
        let priority_queue = PriorityQueue::new(config.max_concurrent_downloads);

        // Create bandwidth scheduler with configured rules
        let scheduler = Arc::new(RwLock::new(BandwidthScheduler::new(
            config.schedule_rules.clone(),
            BandwidthLimits {
                download: config.global_download_limit,
                upload: config.global_upload_limit,
            },
        )));

        let engine = Arc::new_cyclic(|weak| Self {
            self_ref: weak.clone(),
            config: RwLock::new(config),
            downloads: RwLock::new(HashMap::new()),
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            recursive_groups: RwLock::new(HashMap::new()),
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            recursive_jobs: RwLock::new(HashMap::new()),
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            recursive_job_membership: RwLock::new(HashMap::new()),
            #[cfg(feature = "http")]
            http,
            event_tx,
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            recursive_job_event_tx,
            priority_queue,
            scheduler,
            global_download_limiter,
            global_upload_limiter,
            shutdown: tokio_util::sync::CancellationToken::new(),
            storage,
        });

        // Load persisted downloads from database
        let restored_queued = engine.load_persisted_downloads().await?;
        #[cfg(all(feature = "http", feature = "recursive-http"))]
        engine.load_persisted_recursive_jobs().await?;

        // Start background persistence task
        Self::start_persistence_task(Arc::clone(&engine));

        // Start bandwidth scheduler update task
        Self::start_scheduler_task(Arc::clone(&engine));

        // Auto-start downloads that were persisted as Queued: they never got
        // to run before the crash, and nothing else would ever start them.
        for id in restored_queued {
            if let Err(e) = engine.resume(id).await {
                tracing::warn!(
                    "Failed to auto-start restored queued download {}: {}",
                    id,
                    e
                );
            }
        }

        Ok(engine)
    }

    /// Start background task that periodically persists active download states.
    ///
    /// This ensures that if the process crashes, downloads can be resumed
    /// from approximately where they left off.
    fn start_persistence_task(engine: Arc<Self>) {
        if engine.storage.is_none() {
            return; // No storage configured
        }

        let shutdown = engine.shutdown.clone();
        // Hold only a Weak reference: a strong Arc here would keep the engine
        // alive forever (Drop fires the shutdown token, but Drop can never run
        // while this task owns an Arc — a reference cycle).
        let weak = Arc::downgrade(&engine);
        drop(engine);
        tokio::spawn(async move {
            const PERSISTENCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
            let mut interval = tokio::time::interval(PERSISTENCE_INTERVAL);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let Some(engine) = weak.upgrade() else { break };
                        if let Err(e) = engine.persist_active_downloads().await {
                            tracing::warn!("Failed to persist active downloads: {}", e);
                        }
                    }
                    _ = shutdown.cancelled() => {
                        // Final persistence on shutdown
                        if let Some(engine) = weak.upgrade() {
                            if let Err(e) = engine.persist_active_downloads().await {
                                tracing::warn!("Failed to persist downloads on shutdown: {}", e);
                            }
                        }
                        break;
                    }
                }
            }
        });
    }

    /// Start background task that updates bandwidth limits based on schedule.
    ///
    /// This checks the schedule rules every minute and updates the current
    /// bandwidth limits if they have changed.
    fn start_scheduler_task(engine: Arc<Self>) {
        let shutdown = engine.shutdown.clone();
        // Weak for the same reason as the persistence task: a strong Arc in a
        // shutdown-gated loop is a reference cycle that makes Drop unreachable.
        let weak = Arc::downgrade(&engine);
        drop(engine);
        tokio::spawn(async move {
            const SCHEDULER_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
            let mut interval = tokio::time::interval(SCHEDULER_INTERVAL);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let Some(engine) = weak.upgrade() else { break };
                        if engine.scheduler.read().update() {
                            let limits = engine.scheduler.read().get_limits();
                            #[cfg(feature = "http")]
                            engine
                                .http
                                .set_bandwidth_limits(limits.download, limits.upload);
                        }
                    }
                    _ = shutdown.cancelled() => {
                        break;
                    }
                }
            }
        });
    }

    /// Persist all active (non-completed, non-error) downloads to storage.
    async fn persist_active_downloads(&self) -> Result<()> {
        let storage = match &self.storage {
            Some(s) => s,
            None => return Ok(()),
        };

        // Collect active downloads and their segment info
        type PersistEntry = (
            DownloadStatus,
            Option<Vec<crate::storage::Segment>>,
            Option<String>,
        );
        let active_downloads: Vec<PersistEntry> = {
            let downloads = self.downloads.read();
            downloads
                .values()
                .filter(|d| d.status.state.is_active())
                .map(|d| {
                    let segments = match &d.handle {
                        #[cfg(feature = "http")]
                        Some(DownloadHandle::Http(h)) => h
                            .segmented_download
                            .read()
                            .as_ref()
                            .map(|sd| sd.segments_with_progress()),
                        _ => None,
                    };
                    // Torrents refresh their runtime metadata each cycle so
                    // lifetime transfer totals survive a crash.
                    #[cfg(feature = "torrent")]
                    let runtime_json = match &d.handle {
                        Some(DownloadHandle::Torrent(h)) => {
                            let totals = h.downloader.transfer_totals();
                            d.options.as_ref().and_then(|opts| {
                                self.build_persisted_runtime_metadata(
                                    opts,
                                    Some(totals),
                                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                                    d.redirect_scope.as_ref(),
                                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                                    d.recursive_group_id,
                                )
                                .ok()
                            })
                        }
                        _ => None,
                    };
                    #[cfg(not(feature = "torrent"))]
                    let runtime_json: Option<String> = None;
                    (d.status.clone(), segments, runtime_json)
                })
                .collect()
        };

        // Save each active download and its segments
        for (status, segments_opt, runtime_json) in active_downloads {
            if let Err(e) = storage.save_download(&status).await {
                tracing::debug!("Failed to persist download {}: {}", status.id, e);
            }

            // Save segments if this is a segmented HTTP download
            if let Some(segments) = segments_opt {
                if let Err(e) = storage.save_segments(status.id, &segments).await {
                    tracing::debug!("Failed to persist segments for {}: {}", status.id, e);
                }
            }

            // Refresh runtime metadata (torrent transfer totals)
            if let Some(json) = runtime_json {
                if let Err(e) = storage.save_runtime_metadata(status.id, &json).await {
                    tracing::debug!(
                        "Failed to persist runtime metadata for {}: {}",
                        status.id,
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Load persisted downloads from database on startup.
    ///
    /// Returns the ids of downloads that were persisted in `Queued` state:
    /// they are restored as `Paused` (there is no task backing them) and the
    /// caller auto-resumes them, so a crash can't strand them in a state
    /// `resume()` refuses to act on.
    async fn load_persisted_downloads(&self) -> Result<Vec<DownloadId>> {
        let mut restored_queued = Vec::new();
        let storage = match &self.storage {
            Some(s) => s,
            None => return Ok(restored_queued), // No storage configured
        };

        let persisted = storage.load_all().await?;
        let runtime_metadata = storage.load_all_runtime_metadata().await?;
        #[cfg(all(feature = "http", feature = "recursive-http"))]
        let mut restored_groups = HashMap::new();

        for status in persisted {
            // For active/downloading states, mark as paused (crashed mid-download)
            let restored_state = match &status.state {
                DownloadState::Downloading | DownloadState::Connecting => DownloadState::Paused,
                DownloadState::Seeding => DownloadState::Paused, // Torrents that were seeding
                DownloadState::Queued => {
                    // No task backs a restored Queued download; restore as
                    // Paused and let the caller auto-resume it.
                    restored_queued.push(status.id);
                    DownloadState::Paused
                }
                other => other.clone(),
            };

            let mut restored_status = status.clone();
            restored_status.state = restored_state;
            // Reset speeds (they're stale)
            restored_status.progress.download_speed = 0;
            restored_status.progress.upload_speed = 0;
            restored_status.progress.connections = 0;

            let parsed_runtime =
                runtime_metadata.get(&status.id).and_then(|runtime_json| {
                    match self.parse_persisted_runtime_metadata(runtime_json) {
                        Ok(runtime) => Some(runtime),
                        Err(e) => {
                            tracing::warn!(
                                "Failed to parse runtime metadata for {}: {}",
                                status.id,
                                e
                            );
                            None
                        }
                    }
                });
            let restored_options = parsed_runtime.as_ref().and_then(|r| r.options.clone());
            let restored_totals = parsed_runtime.as_ref().and_then(|r| r.torrent_totals);

            #[cfg(all(feature = "http", feature = "recursive-http"))]
            let (redirect_scope, recursive_group_id, recursive_fail_fast) = match parsed_runtime
                .as_ref()
                .and_then(|r| r.recursive_child.clone())
            {
                Some(recursive_child) => (
                    Some(crawl::RedirectScope::from_persisted(
                        recursive_child.redirect_scope,
                    )?),
                    recursive_child.recursive_group_id,
                    recursive_child.fail_fast,
                ),
                None => (None, None, false),
            };

            #[cfg(all(feature = "http", feature = "recursive-http"))]
            if let Some(group_id) = recursive_group_id {
                if recursive_fail_fast
                    && !matches!(
                        restored_status.state,
                        DownloadState::Completed | DownloadState::Error { .. }
                    )
                {
                    restored_groups
                        .entry(group_id)
                        .or_insert_with(|| RecursiveGroup {
                            child_ids: HashSet::new(),
                            fail_fast: true,
                            failed: false,
                        })
                        .child_ids
                        .insert(status.id);
                }
            }

            #[cfg(feature = "http")]
            let cached_segments = if status.kind == DownloadKind::Http {
                let segments = storage.load_segments(status.id).await?;
                if segments.is_empty() {
                    None
                } else {
                    Some(segments)
                }
            } else {
                None
            };

            // Insert into downloads map
            self.downloads.write().insert(
                status.id,
                ManagedDownload {
                    status: restored_status,
                    handle: None,
                    options: restored_options,
                    #[cfg(feature = "torrent")]
                    cached_torrent_data: None,
                    restored_totals,
                    #[cfg(feature = "http")]
                    cached_segments,
                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                    redirect_scope,
                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                    recursive_group_id,
                },
            );

            tracing::info!(
                "Restored download {} ({}) in state {:?}",
                status.id,
                status.metadata.name,
                status.state
            );
        }

        #[cfg(all(feature = "http", feature = "recursive-http"))]
        {
            self.recursive_groups.write().extend(restored_groups);
        }

        Ok(restored_queued)
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    async fn load_persisted_recursive_jobs(&self) -> Result<()> {
        let storage = match &self.storage {
            Some(s) => s,
            None => return Ok(()),
        };

        let jobs = storage.load_recursive_jobs().await?;
        if jobs.is_empty() {
            return Ok(());
        }

        let mut recursive_jobs = self.recursive_jobs.write();
        for job in jobs {
            self.register_recursive_job_membership(&job);
            recursive_jobs.insert(job.id, job);
        }

        Ok(())
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    fn register_recursive_job_membership(&self, job: &crate::types::TrackedRecursiveJob) {
        let mut membership = self.recursive_job_membership.write();
        for child_id in &job.child_ids {
            membership.entry(*child_id).or_default().insert(job.id);
        }
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    fn unregister_recursive_job_membership(&self, job: &crate::types::TrackedRecursiveJob) {
        let mut membership = self.recursive_job_membership.write();
        for child_id in &job.child_ids {
            let should_remove = if let Some(job_ids) = membership.get_mut(child_id) {
                job_ids.remove(&job.id);
                job_ids.is_empty()
            } else {
                false
            };
            if should_remove {
                membership.remove(child_id);
            }
        }
    }

    #[cfg(feature = "torrent")]
    fn build_torrent_status_info(metainfo: &Metainfo) -> TorrentStatusInfo {
        TorrentStatusInfo {
            files: metainfo
                .info
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| TorrentFile {
                    index: i,
                    path: f.path.clone(),
                    size: f.length,
                    completed: 0,
                    selected: true,
                })
                .collect(),
            piece_length: metainfo.info.piece_length,
            pieces_count: metainfo.info.pieces.len(),
            private: metainfo.info.private,
        }
    }

    /// Add an HTTP/HTTPS download
    #[cfg(feature = "http")]
    pub async fn add_http(&self, url: &str, options: DownloadOptions) -> Result<DownloadId> {
        self.add_http_internal(
            url,
            options,
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            None,
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            None,
        )
        .await
    }

    #[cfg(feature = "http")]
    async fn add_http_internal(
        &self,
        url: &str,
        options: DownloadOptions,
        #[cfg(all(feature = "http", feature = "recursive-http"))] redirect_scope: Option<
            crawl::RedirectScope,
        >,
        #[cfg(all(feature = "http", feature = "recursive-http"))] recursive_group_id: Option<Uuid>,
    ) -> Result<DownloadId> {
        // Validate URL
        let parsed_url = Url::parse(url)
            .map_err(|e| EngineError::invalid_input("url", format!("Invalid URL: {}", e)))?;

        // Only allow http and https
        match parsed_url.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(EngineError::invalid_input(
                    "url",
                    format!("Unsupported scheme: {}", scheme),
                ));
            }
        }

        #[cfg(all(feature = "http", feature = "recursive-http"))]
        if let Some(group_id) = recursive_group_id {
            if !self.recursive_groups.read().contains_key(&group_id) {
                return Err(EngineError::Internal(format!(
                    "recursive group {} missing for child download",
                    group_id
                )));
            }
        }

        // Generate download ID
        let id = DownloadId::new();

        // Determine save directory
        let save_dir = options
            .save_dir
            .clone()
            .unwrap_or_else(|| self.config.read().download_dir.clone());

        // Extract filename from URL or options
        let filename = options.filename.clone().or_else(|| {
            parsed_url
                .path_segments()
                .and_then(|mut segments| segments.next_back())
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
        });

        let name = filename.clone().unwrap_or_else(|| "download".to_string());
        // Lifecycle operations use this path before a worker starts, including
        // cancel-with-delete. Reject unsafe names before storing the download.
        crate::http::validate_filename_components(&name)?;

        // Create download status
        let status = DownloadStatus {
            id,
            kind: DownloadKind::Http,
            state: if options.start_paused {
                DownloadState::Paused
            } else {
                DownloadState::Queued
            },
            priority: options.priority,
            progress: DownloadProgress::default(),
            metadata: DownloadMetadata {
                name,
                url: Some(url.to_string()),
                magnet_uri: None,
                info_hash: None,
                save_dir,
                filename,
                user_agent: options.user_agent.clone(),
                referer: options.referer.clone(),
                headers: options.headers.clone(),
                cookies: options.cookies.clone().unwrap_or_default(),
                checksum: options.checksum.clone(),
                mirrors: options.mirrors.clone(),
                etag: None,
                last_modified: None,
            },
            torrent_info: None,
            peers: None,
            created_at: Utc::now(),
            completed_at: None,
        };

        let runtime_metadata_json = self.build_persisted_runtime_metadata(
            &options,
            None,
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            redirect_scope.as_ref(),
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            recursive_group_id,
        )?;

        // Insert into downloads map
        {
            let mut downloads = self.downloads.write();
            downloads.insert(
                id,
                ManagedDownload {
                    status: status.clone(),
                    handle: None,
                    options: Some(options.clone()),
                    #[cfg(feature = "torrent")]
                    cached_torrent_data: None,
                    restored_totals: None,
                    #[cfg(feature = "http")]
                    cached_segments: None,
                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                    redirect_scope,
                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                    recursive_group_id,
                },
            );
        }

        #[cfg(all(feature = "http", feature = "recursive-http"))]
        if let Some(group_id) = recursive_group_id {
            if let Some(group) = self.recursive_groups.write().get_mut(&group_id) {
                group.child_ids.insert(id);
            }
        }

        // Persist to database
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_download(&status).await {
                tracing::warn!("Failed to persist new download {}: {}", id, e);
            }
            if let Err(e) = storage
                .save_runtime_metadata(id, &runtime_metadata_json)
                .await
            {
                tracing::warn!("Failed to persist runtime metadata for {}: {}", id, e);
            }
        }

        // Emit event
        let _ = self.event_tx.send(DownloadEvent::Added { id });

        // Start the download (no saved segments for new downloads)
        if !options.start_paused {
            self.start_download(id, url.to_string(), options, None)
                .await?;
        }

        Ok(id)
    }

    /// Discover files reachable from an HTTP/HTTPS directory-like root URL.
    ///
    /// This method is feature-gated behind `recursive-http` and currently
    /// validates recursive inputs before delegating to the in-progress crawler
    /// implementation.
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    pub async fn discover_http_recursive(
        &self,
        root_url: &str,
        options: &DownloadOptions,
        recursive: &crate::types::RecursiveOptions,
    ) -> Result<crate::types::RecursiveManifest> {
        crawl::discover(&self.http, root_url, options, recursive).await
    }

    /// Expand a recursive HTTP/HTTPS discovery root into child HTTP downloads.
    ///
    /// Each child is intended to become a normal HTTP download so the existing
    /// queueing, retry, and persistence paths remain unchanged.
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    pub async fn add_http_recursive(
        &self,
        root_url: &str,
        options: DownloadOptions,
        recursive: crate::types::RecursiveOptions,
    ) -> Result<crate::types::RecursiveJob> {
        let manifest = self
            .discover_http_recursive(root_url, &options, &recursive)
            .await?;
        self.enqueue_recursive_manifest(manifest, options, &recursive)
            .await
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    async fn enqueue_recursive_manifest(
        &self,
        manifest: crate::types::RecursiveManifest,
        options: DownloadOptions,
        recursive: &crate::types::RecursiveOptions,
    ) -> Result<crate::types::RecursiveJob> {
        let redirect_scope = crawl::RedirectScope::new(&manifest.root_url, recursive)?;
        let recursive_group_id = if recursive.fail_fast && !manifest.entries.is_empty() {
            let group_id = Uuid::new_v4();
            self.recursive_groups.write().insert(
                group_id,
                RecursiveGroup {
                    child_ids: HashSet::new(),
                    fail_fast: true,
                    failed: false,
                },
            );
            Some(group_id)
        } else {
            None
        };

        let base_save_dir = options
            .save_dir
            .clone()
            .unwrap_or_else(|| self.config.read().download_dir.clone());
        let mut child_ids = Vec::with_capacity(manifest.entries.len());

        for entry in &manifest.entries {
            let mut child_options = options.clone();
            child_options.save_dir = Some(match entry.relative_path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => base_save_dir.join(parent),
                _ => base_save_dir.clone(),
            });
            child_options.filename = entry
                .relative_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string());
            child_options.checksum = None;
            child_options.mirrors.clear();

            let child_id = match self
                .add_http_internal(
                    &entry.url,
                    child_options,
                    Some(redirect_scope.clone()),
                    recursive_group_id,
                )
                .await
            {
                Ok(child_id) => child_id,
                Err(err) => {
                    self.rollback_recursive_enqueue(&child_ids, recursive_group_id)
                        .await;
                    return Err(err);
                }
            };
            child_ids.push(child_id);
        }

        let tracked_job = crate::types::TrackedRecursiveJob {
            id: Uuid::new_v4(),
            root_url: manifest.root_url.clone(),
            child_ids: child_ids.clone(),
            created_at: Utc::now(),
        };

        self.register_recursive_job_membership(&tracked_job);
        self.recursive_jobs
            .write()
            .insert(tracked_job.id, tracked_job.clone());

        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_recursive_job(&tracked_job).await {
                tracing::warn!("Failed to persist recursive job {}: {}", tracked_job.id, e);
            }
        }

        let status = self.recursive_job_status(&tracked_job.as_job());
        let _ = self
            .recursive_job_event_tx
            .send(crate::types::RecursiveJobEvent::Added {
                job: tracked_job,
                status,
            });

        Ok(crate::types::RecursiveJob {
            root_url: manifest.root_url,
            child_ids,
        })
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    async fn rollback_recursive_enqueue(
        &self,
        child_ids: &[DownloadId],
        recursive_group_id: Option<Uuid>,
    ) {
        for child_id in child_ids {
            if self.status(*child_id).is_some() {
                let _ = self.cancel(*child_id, false).await;
            }
        }

        if let Some(group_id) = recursive_group_id {
            self.recursive_groups.write().remove(&group_id);
        }
    }

    /// List tracked recursive jobs restored from storage or created in-process.
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    pub fn list_recursive_jobs(&self) -> Vec<crate::types::TrackedRecursiveJob> {
        let mut jobs = self
            .recursive_jobs
            .read()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        jobs
    }

    /// Look up a tracked recursive job by ID.
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    pub fn recursive_job(&self, id: Uuid) -> Option<crate::types::TrackedRecursiveJob> {
        self.recursive_jobs.read().get(&id).cloned()
    }

    /// Subscribe to recursive parent job events.
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    pub fn subscribe_recursive_jobs(&self) -> broadcast::Receiver<crate::types::RecursiveJobEvent> {
        self.recursive_job_event_tx.subscribe()
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    fn emit_recursive_job_update(&self, id: Uuid) {
        if let Some(job) = self.recursive_job(id) {
            let status = self.recursive_job_status(&job.as_job());
            let _ = self
                .recursive_job_event_tx
                .send(crate::types::RecursiveJobEvent::Updated { job, status });
        }
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    fn emit_recursive_job_updates_for_child(&self, child_id: DownloadId) {
        let job_ids = self
            .recursive_job_membership
            .read()
            .get(&child_id)
            .cloned()
            .unwrap_or_default();
        for job_id in job_ids {
            self.emit_recursive_job_update(job_id);
        }
    }

    /// Cancel all currently present child downloads for a tracked recursive job.
    ///
    /// This leaves the tracked recursive job record intact so callers can still
    /// inspect aggregate state/history after the children have been removed.
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    pub async fn cancel_recursive_job(&self, id: Uuid, delete_files: bool) -> Result<()> {
        let job = self
            .recursive_job(id)
            .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

        for child_id in job.child_ids {
            if self.status(child_id).is_some() {
                self.cancel(child_id, delete_files).await?;
            }
        }

        Ok(())
    }

    /// Remove a tracked recursive job record and cancel any remaining children.
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    pub async fn remove_recursive_job(&self, id: Uuid, delete_files: bool) -> Result<()> {
        let job = self
            .recursive_job(id)
            .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

        for child_id in &job.child_ids {
            if self.status(*child_id).is_some() {
                self.cancel(*child_id, delete_files).await?;
            }
        }

        self.recursive_jobs.write().remove(&id);
        self.unregister_recursive_job_membership(&job);
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.delete_recursive_job(id).await {
                tracing::warn!("Failed to delete recursive job {}: {}", id, e);
            }
        }
        let _ = self
            .recursive_job_event_tx
            .send(crate::types::RecursiveJobEvent::Removed { id });

        Ok(())
    }

    /// Derive aggregate status for a recursive job from its child downloads.
    #[cfg(all(feature = "http", feature = "recursive-http"))]
    pub fn recursive_job_status(
        &self,
        job: &crate::types::RecursiveJob,
    ) -> crate::types::RecursiveJobStatus {
        let mut progress = crate::types::RecursiveJobProgress {
            total_children: job.child_ids.len(),
            ..Default::default()
        };
        let mut summed_total_size = 0u64;
        let mut all_total_sizes_known = true;

        for child_id in &job.child_ids {
            match self.status(*child_id) {
                Some(status) => {
                    progress.completed_size = progress
                        .completed_size
                        .saturating_add(status.progress.completed_size);

                    if let Some(total_size) = status.progress.total_size {
                        summed_total_size = summed_total_size.saturating_add(total_size);
                    } else {
                        all_total_sizes_known = false;
                    }

                    match status.state {
                        DownloadState::Queued => progress.queued_children += 1,
                        DownloadState::Connecting
                        | DownloadState::Downloading
                        | DownloadState::Seeding => progress.active_children += 1,
                        DownloadState::Paused => progress.paused_children += 1,
                        DownloadState::Completed => progress.completed_children += 1,
                        DownloadState::Error { .. } => progress.failed_children += 1,
                    }
                }
                None => {
                    progress.missing_children += 1;
                    all_total_sizes_known = false;
                }
            }
        }

        progress.total_size = if all_total_sizes_known {
            Some(summed_total_size)
        } else {
            None
        };

        let state = if progress.total_children == 0 {
            crate::types::RecursiveJobState::Empty
        } else if progress.completed_children == progress.total_children {
            crate::types::RecursiveJobState::Completed
        } else if progress.failed_children + progress.missing_children == progress.total_children {
            crate::types::RecursiveJobState::Failed
        } else if progress.failed_children > 0 || progress.missing_children > 0 {
            crate::types::RecursiveJobState::Partial
        } else if progress.active_children > 0 {
            crate::types::RecursiveJobState::Running
        } else if progress.paused_children > 0 {
            crate::types::RecursiveJobState::Paused
        } else if progress.queued_children > 0 {
            crate::types::RecursiveJobState::Queued
        } else {
            crate::types::RecursiveJobState::Partial
        };

        crate::types::RecursiveJobStatus {
            root_url: job.root_url.clone(),
            child_ids: job.child_ids.clone(),
            state,
            progress,
        }
    }

    fn build_persisted_runtime_metadata(
        &self,
        options: &DownloadOptions,
        torrent_totals: Option<(u64, u64)>,
        #[cfg(all(feature = "http", feature = "recursive-http"))] redirect_scope: Option<
            &crawl::RedirectScope,
        >,
        #[cfg(all(feature = "http", feature = "recursive-http"))] recursive_group_id: Option<Uuid>,
    ) -> Result<String> {
        let runtime = PersistedDownloadRuntimeMetadata {
            #[cfg(all(feature = "http", feature = "recursive-http"))]
            recursive_child: redirect_scope.map(|redirect_scope| PersistedRecursiveChildState {
                redirect_scope: redirect_scope.to_persisted(),
                recursive_group_id,
                fail_fast: recursive_group_id
                    .and_then(|group_id| {
                        self.recursive_groups
                            .read()
                            .get(&group_id)
                            .map(|g| g.fail_fast)
                    })
                    .unwrap_or(false),
            }),
            options: Some(options.clone()),
            torrent_totals,
        };

        serde_json::to_string(&runtime).map_err(|e| {
            EngineError::Internal(format!("Failed to serialize runtime metadata: {}", e))
        })
    }

    fn parse_persisted_runtime_metadata(
        &self,
        runtime_json: &str,
    ) -> Result<PersistedDownloadRuntimeMetadata> {
        serde_json::from_str(runtime_json).map_err(|e| {
            EngineError::Internal(format!("Failed to deserialize runtime metadata: {}", e))
        })
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    fn remove_recursive_group_member(&self, recursive_group_id: Option<Uuid>, id: DownloadId) {
        let Some(group_id) = recursive_group_id else {
            return;
        };

        let mut groups = self.recursive_groups.write();
        let should_remove_group = if let Some(group) = groups.get_mut(&group_id) {
            group.child_ids.remove(&id);
            group.child_ids.is_empty()
        } else {
            false
        };

        if should_remove_group {
            groups.remove(&group_id);
        }
    }

    #[cfg(all(feature = "http", feature = "recursive-http"))]
    async fn trigger_recursive_fail_fast(
        &self,
        failed_id: DownloadId,
        failed_message: &str,
    ) -> Result<()> {
        let recursive_group_id = {
            let downloads = self.downloads.read();
            downloads.get(&failed_id).and_then(|d| d.recursive_group_id)
        };
        let Some(group_id) = recursive_group_id else {
            return Ok(());
        };

        let sibling_ids = {
            let mut groups = self.recursive_groups.write();
            let Some(group) = groups.get_mut(&group_id) else {
                return Ok(());
            };

            if !group.fail_fast || group.failed {
                return Ok(());
            }

            group.failed = true;
            group
                .child_ids
                .iter()
                .copied()
                .filter(|id| *id != failed_id)
                .collect::<Vec<_>>()
        };

        let fail_fast_message = format!(
            "Aborted because recursive sibling download {} failed: {}",
            failed_id, failed_message
        );

        let impacted = {
            let mut downloads = self.downloads.write();
            let mut impacted = Vec::new();

            for sibling_id in sibling_ids {
                let Some(download) = downloads.get_mut(&sibling_id) else {
                    continue;
                };

                if !matches!(
                    download.status.state,
                    DownloadState::Queued | DownloadState::Connecting | DownloadState::Downloading
                ) {
                    continue;
                }

                let old_state = download.status.state.clone();
                download.status.state = DownloadState::Error {
                    kind: "RecursiveFailFast".to_string(),
                    message: fail_fast_message.clone(),
                    retryable: false,
                };

                impacted.push(RecursiveFailFastAbort {
                    id: sibling_id,
                    old_state,
                    error_message: fail_fast_message.clone(),
                    status: download.status.clone(),
                    cancel_token: match download.handle.as_ref() {
                        Some(DownloadHandle::Http(handle)) => Some(handle.cancel_token.clone()),
                        _ => None,
                    },
                });
            }

            impacted
        };

        for abort in &impacted {
            if let Some(cancel_token) = &abort.cancel_token {
                cancel_token.cancel();
            }
        }

        if let Some(ref storage) = self.storage {
            for abort in &impacted {
                if let Err(e) = storage.save_download(&abort.status).await {
                    tracing::debug!(
                        "Failed to persist recursive fail-fast state for {}: {}",
                        abort.id,
                        e
                    );
                }
            }
        }

        for abort in impacted {
            let new_state = DownloadState::Error {
                kind: "RecursiveFailFast".to_string(),
                message: abort.error_message.clone(),
                retryable: false,
            };

            let _ = self.event_tx.send(DownloadEvent::StateChanged {
                id: abort.id,
                old_state: abort.old_state,
                new_state,
            });
            let _ = self.event_tx.send(DownloadEvent::Failed {
                id: abort.id,
                error: abort.error_message.clone(),
                retryable: false,
            });
            self.emit_recursive_job_updates_for_child(abort.id);
            self.remove_recursive_group_member(Some(group_id), abort.id);
        }

        Ok(())
    }

    /// Add a BitTorrent download from torrent file data
    #[cfg(feature = "torrent")]
    pub async fn add_torrent(
        &self,
        torrent_data: &[u8],
        options: DownloadOptions,
    ) -> Result<DownloadId> {
        // Parse torrent file
        let metainfo = Metainfo::parse(torrent_data)?;

        // Generate download ID
        let id = DownloadId::new();

        // Determine save directory
        let save_dir = options
            .save_dir
            .clone()
            .unwrap_or_else(|| self.config.read().download_dir.clone());

        // Create download status
        let status = DownloadStatus {
            id,
            kind: DownloadKind::Torrent,
            state: if options.start_paused {
                DownloadState::Paused
            } else {
                DownloadState::Queued
            },
            priority: options.priority,
            progress: DownloadProgress::default(),
            metadata: DownloadMetadata {
                name: metainfo.info.name.clone(),
                url: None,
                magnet_uri: None,
                info_hash: Some(hex::encode(metainfo.info_hash)),
                save_dir: save_dir.clone(),
                filename: Some(metainfo.info.name.clone()),
                user_agent: options.user_agent.clone(),
                referer: None,
                headers: Vec::new(),
                cookies: Vec::new(),
                checksum: None,
                mirrors: Vec::new(),
                etag: None,
                last_modified: None,
            },
            torrent_info: Some(Self::build_torrent_status_info(&metainfo)),
            peers: Some(Vec::new()),
            created_at: Utc::now(),
            completed_at: None,
        };

        // Insert into downloads map
        {
            let mut downloads = self.downloads.write();
            downloads.insert(
                id,
                ManagedDownload {
                    status: status.clone(),
                    handle: None,
                    options: Some(options.clone()),
                    #[cfg(feature = "torrent")]
                    cached_torrent_data: options.start_paused.then(|| torrent_data.to_vec()),
                    restored_totals: None,
                    #[cfg(feature = "http")]
                    cached_segments: None,
                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                    redirect_scope: None,
                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                    recursive_group_id: None,
                },
            );
        }

        // Persist to database
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_download(&status).await {
                tracing::warn!("Failed to persist new torrent download {}: {}", id, e);
            }
            match self.build_persisted_runtime_metadata(
                &options,
                None,
                #[cfg(all(feature = "http", feature = "recursive-http"))]
                None,
                #[cfg(all(feature = "http", feature = "recursive-http"))]
                None,
            ) {
                Ok(runtime_json) => {
                    if let Err(e) = storage.save_runtime_metadata(id, &runtime_json).await {
                        tracing::warn!("Failed to persist runtime metadata for {}: {}", id, e);
                    }
                }
                Err(e) => tracing::warn!("Failed to serialize options for {}: {}", id, e),
            }
            // Save raw torrent data for crash recovery
            if let Err(e) = storage.save_torrent_data(id, torrent_data).await {
                tracing::warn!("Failed to persist torrent data for {}: {}", id, e);
            }
        }

        // Emit event
        let _ = self.event_tx.send(DownloadEvent::Added { id });

        // Start the torrent download
        if !options.start_paused {
            self.start_torrent(id, metainfo, save_dir, options).await?;
        }

        Ok(id)
    }

    /// Add a BitTorrent download from a magnet URI
    #[cfg(feature = "torrent")]
    pub async fn add_magnet(
        &self,
        magnet_uri: &str,
        options: DownloadOptions,
    ) -> Result<DownloadId> {
        // Parse magnet URI
        let magnet = MagnetUri::parse(magnet_uri)?;

        // Generate download ID
        let id = DownloadId::new();

        // Determine save directory
        let save_dir = options
            .save_dir
            .clone()
            .unwrap_or_else(|| self.config.read().download_dir.clone());

        // Create download status
        let status = DownloadStatus {
            id,
            kind: DownloadKind::Magnet,
            state: if options.start_paused {
                DownloadState::Paused
            } else {
                DownloadState::Queued
            },
            priority: options.priority,
            progress: DownloadProgress::default(),
            metadata: DownloadMetadata {
                name: magnet.name(),
                url: None,
                magnet_uri: Some(magnet_uri.to_string()),
                info_hash: Some(hex::encode(magnet.info_hash)),
                save_dir: save_dir.clone(),
                filename: magnet.display_name.clone(),
                user_agent: options.user_agent.clone(),
                referer: None,
                headers: Vec::new(),
                cookies: Vec::new(),
                checksum: None,
                mirrors: Vec::new(),
                etag: None,
                last_modified: None,
            },
            torrent_info: None, // Will be populated when metadata is received
            peers: Some(Vec::new()),
            created_at: Utc::now(),
            completed_at: None,
        };

        // Insert into downloads map
        {
            let mut downloads = self.downloads.write();
            downloads.insert(
                id,
                ManagedDownload {
                    status: status.clone(),
                    handle: None,
                    options: Some(options.clone()),
                    #[cfg(feature = "torrent")]
                    cached_torrent_data: None,
                    restored_totals: None,
                    #[cfg(feature = "http")]
                    cached_segments: None,
                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                    redirect_scope: None,
                    #[cfg(all(feature = "http", feature = "recursive-http"))]
                    recursive_group_id: None,
                },
            );
        }

        // Persist to database
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_download(&status).await {
                tracing::warn!("Failed to persist new magnet download {}: {}", id, e);
            }
            match self.build_persisted_runtime_metadata(
                &options,
                None,
                #[cfg(all(feature = "http", feature = "recursive-http"))]
                None,
                #[cfg(all(feature = "http", feature = "recursive-http"))]
                None,
            ) {
                Ok(runtime_json) => {
                    if let Err(e) = storage.save_runtime_metadata(id, &runtime_json).await {
                        tracing::warn!("Failed to persist runtime metadata for {}: {}", id, e);
                    }
                }
                Err(e) => tracing::warn!("Failed to serialize options for {}: {}", id, e),
            }
        }

        // Emit event
        let _ = self.event_tx.send(DownloadEvent::Added { id });

        // Start the magnet download
        if !options.start_paused {
            self.start_magnet(id, magnet, save_dir, options).await?;
        }

        Ok(id)
    }

    /// Start a torrent download task
    #[cfg(feature = "torrent")]
    fn build_torrent_runtime_config(&self, options: &DownloadOptions) -> TorrentConfig {
        let config = self.config.read();
        TorrentConfig {
            max_peers: config.max_peers,
            listen_port_range: config.torrent.listen_port_range,
            enable_dht: config.enable_dht,
            enable_pex: config.enable_pex,
            enable_lpd: config.enable_lpd,
            seed_ratio: options.seed_ratio.or(Some(config.seed_ratio)),
            max_upload_speed: options
                .max_upload_speed
                .or(config.global_upload_limit)
                .unwrap_or(0),
            max_download_speed: options
                .max_download_speed
                .or(config.global_download_limit)
                .unwrap_or(0),
            announce_interval: config.torrent.tracker_update_interval,
            request_timeout: Duration::from_secs(config.torrent.peer_timeout),
            keepalive_interval: Duration::from_secs(120),
            max_pending_requests: config.torrent.max_pending_requests,
            dht_bootstrap_nodes: config.torrent.dht_bootstrap_nodes.clone(),
            tick_interval_ms: config.torrent.tick_interval_ms,
            connect_interval_secs: config.torrent.connect_interval_secs,
            choking_interval_secs: config.torrent.choking_interval_secs,
            enable_utp: config.torrent.utp.enabled
                && config.torrent.utp.policy != crate::config::TransportPolicy::TcpOnly,
            enable_endgame: config.torrent.enable_endgame,
            allocation_mode: config.torrent.allocation_mode,
            utp_config: crate::torrent::utp::UtpConfig {
                enable_sack: config.torrent.utp.enable_sack,
                max_window_size: config.torrent.utp.max_window_size,
                recv_window: config.torrent.utp.recv_window,
                target_delay_us: config.torrent.utp.target_delay_us,
            },
        }
    }

    /// Start a torrent download task
    #[cfg(feature = "torrent")]
    async fn start_torrent(
        &self,
        id: DownloadId,
        metainfo: Metainfo,
        save_dir: std::path::PathBuf,
        options: DownloadOptions,
    ) -> Result<()> {
        let torrent_config = self.build_torrent_runtime_config(&options);
        let (webseed_config, encryption_config, transport_policy, tcp_fallback) = {
            let config = self.config.read();
            // Honor the configured policy as-is: the old special-case that
            // silently rewrote a default "Preferred" config to "Disabled"
            // meant the documented default was actually plaintext-only.
            let encryption = config.torrent.encryption.clone();
            (
                config.torrent.webseed.clone(),
                encryption,
                config.torrent.utp.policy,
                config.torrent.utp.tcp_fallback,
            )
        };

        let downloader = Arc::new(TorrentDownloader::from_torrent(
            id,
            metainfo,
            save_dir,
            torrent_config,
            self.event_tx.clone(),
        )?);
        downloader.set_webseed_config(webseed_config);
        downloader.set_mse_config(encryption_config);
        downloader.set_transport_policy(transport_policy, tcp_fallback);
        {
            // All torrent traffic draws from the engine's global limiters,
            // plus per-download limits when the options specify them.
            let mut down = vec![Arc::clone(&self.global_download_limiter)];
            if let Some(l) = options.max_download_speed.filter(|l| *l > 0) {
                down.push(Arc::new(crate::limiter::RateLimiter::new(Some(l))));
            }
            let mut up = vec![Arc::clone(&self.global_upload_limiter)];
            if let Some(l) = options.max_upload_speed.filter(|l| *l > 0) {
                up.push(Arc::new(crate::limiter::RateLimiter::new(Some(l))));
            }
            downloader.set_rate_limiters(down, up);
        }
        // Restore lifetime transfer totals so seed-ratio decisions survive
        // restarts.
        if let Some((downloaded, uploaded)) = {
            let mut downloads = self.downloads.write();
            downloads
                .get_mut(&id)
                .and_then(|d| d.restored_totals.take())
        } {
            downloader.set_transfer_totals(downloaded, uploaded);
        }

        // Apply selected files for partial download
        if let Some(ref selected) = options.selected_files {
            downloader.set_selected_files(Some(selected));
        }

        // Apply sequential download mode if requested
        if let Some(sequential) = options.sequential {
            downloader.set_sequential(sequential);
        }

        let downloader_clone = Arc::clone(&downloader);
        let engine = self.arc()?;
        let priority_queue = Arc::clone(&self.priority_queue);
        let priority = options.priority;

        // Update state
        self.update_state(id, DownloadState::Queued)?;

        let task = tokio::spawn(async move {
            // Torrents respect the same engine-wide concurrency limit and
            // priority ordering as HTTP downloads. The permit is held for the
            // life of the task (including seeding). If the task is aborted
            // while still waiting here, cancel()/shutdown() clear the
            // abandoned queue entry via priority_queue.remove(id).
            let _permit = priority_queue.acquire(id, priority).await;
            engine.update_state(id, DownloadState::Connecting)?;

            // Start the download (announces to trackers, verifies existing pieces)
            if let Err(e) = Arc::clone(&downloader_clone).start().await {
                let error_msg = e.to_string();
                engine.update_state(
                    id,
                    DownloadState::Error {
                        kind: format!("{:?}", e),
                        message: error_msg.clone(),
                        retryable: e.is_retryable(),
                    },
                )?;
                // Persist error state to storage
                if let Some(ref storage) = engine.storage {
                    let status = engine.downloads.read().get(&id).map(|d| d.status.clone());
                    if let Some(status) = status {
                        if let Err(e) = storage.save_download(&status).await {
                            tracing::debug!("Failed to persist error state for {}: {}", id, e);
                        }
                    }
                }
                let _ = engine.event_tx.send(DownloadEvent::Failed {
                    id,
                    error: error_msg,
                    retryable: e.is_retryable(),
                });
                return Ok(());
            }

            // Update state to downloading
            engine.update_state(id, DownloadState::Downloading)?;
            let _ = engine.event_tx.send(DownloadEvent::Started { id });

            // Run the peer connection loop
            let downloader_ref = Arc::clone(&downloader_clone);
            if let Err(e) = downloader_clone.run_peer_loop().await {
                let error_msg = e.to_string();
                engine.update_state(
                    id,
                    DownloadState::Error {
                        kind: format!("{:?}", e),
                        message: error_msg.clone(),
                        retryable: e.is_retryable(),
                    },
                )?;
                // Persist error state to storage
                if let Some(ref storage) = engine.storage {
                    let status = engine.downloads.read().get(&id).map(|d| d.status.clone());
                    if let Some(status) = status {
                        if let Err(e) = storage.save_download(&status).await {
                            tracing::debug!("Failed to persist error state for {}: {}", id, e);
                        }
                    }
                }
                let _ = engine.event_tx.send(DownloadEvent::Failed {
                    id,
                    error: error_msg,
                    retryable: e.is_retryable(),
                });
            } else if downloader_ref.is_complete() {
                // Torrent completed successfully
                let should_complete = {
                    let mut downloads = engine.downloads.write();
                    if let Some(download) = downloads.get_mut(&id) {
                        if download.status.state == DownloadState::Paused {
                            false
                        } else {
                            download.status.state = DownloadState::Completed;
                            download.status.completed_at = Some(Utc::now());
                            true
                        }
                    } else {
                        false
                    }
                };

                if should_complete {
                    // Persist completed state to storage
                    if let Some(ref storage) = engine.storage {
                        let status = engine.downloads.read().get(&id).map(|d| d.status.clone());
                        if let Some(status) = status {
                            if let Err(e) = storage.save_download(&status).await {
                                tracing::debug!(
                                    "Failed to persist completed state for {}: {}",
                                    id,
                                    e
                                );
                            }
                        }
                    }

                    let _ = engine.event_tx.send(DownloadEvent::Completed { id });
                }
            }

            Ok(())
        });

        let progress_task =
            Self::spawn_torrent_progress_task(self.arc()?, id, Arc::clone(&downloader));

        // Store the handle
        {
            let mut downloads = self.downloads.write();
            if let Some(download) = downloads.get_mut(&id) {
                download.handle = Some(DownloadHandle::Torrent(TorrentDownloadHandle {
                    downloader,
                    task,
                    progress_task,
                }));
            }
        }

        Ok(())
    }

    /// Start a magnet download task
    #[cfg(feature = "torrent")]
    async fn start_magnet(
        &self,
        id: DownloadId,
        magnet: MagnetUri,
        save_dir: std::path::PathBuf,
        options: DownloadOptions,
    ) -> Result<()> {
        let torrent_config = self.build_torrent_runtime_config(&options);
        let (webseed_config, encryption_config, transport_policy, tcp_fallback) = {
            let config = self.config.read();
            // Honor the configured policy as-is: the old special-case that
            // silently rewrote a default "Preferred" config to "Disabled"
            // meant the documented default was actually plaintext-only.
            let encryption = config.torrent.encryption.clone();
            (
                config.torrent.webseed.clone(),
                encryption,
                config.torrent.utp.policy,
                config.torrent.utp.tcp_fallback,
            )
        };

        let downloader = Arc::new(TorrentDownloader::from_magnet(
            id,
            magnet,
            save_dir,
            torrent_config,
            self.event_tx.clone(),
        )?);
        downloader.set_webseed_config(webseed_config);
        downloader.set_mse_config(encryption_config);
        downloader.set_transport_policy(transport_policy, tcp_fallback);
        {
            // All torrent traffic draws from the engine's global limiters,
            // plus per-download limits when the options specify them.
            let mut down = vec![Arc::clone(&self.global_download_limiter)];
            if let Some(l) = options.max_download_speed.filter(|l| *l > 0) {
                down.push(Arc::new(crate::limiter::RateLimiter::new(Some(l))));
            }
            let mut up = vec![Arc::clone(&self.global_upload_limiter)];
            if let Some(l) = options.max_upload_speed.filter(|l| *l > 0) {
                up.push(Arc::new(crate::limiter::RateLimiter::new(Some(l))));
            }
            downloader.set_rate_limiters(down, up);
        }
        // Restore lifetime transfer totals so seed-ratio decisions survive
        // restarts.
        if let Some((downloaded, uploaded)) = {
            let mut downloads = self.downloads.write();
            downloads
                .get_mut(&id)
                .and_then(|d| d.restored_totals.take())
        } {
            downloader.set_transfer_totals(downloaded, uploaded);
        }

        if let Some(ref selected) = options.selected_files {
            downloader.set_selected_files(Some(selected));
        }

        // Apply sequential download mode if requested
        if let Some(sequential) = options.sequential {
            downloader.set_sequential(sequential);
        }

        let downloader_clone = Arc::clone(&downloader);
        let engine = self.arc()?;
        let priority_queue = Arc::clone(&self.priority_queue);
        let priority = options.priority;

        // Update state
        self.update_state(id, DownloadState::Queued)?;

        let task = tokio::spawn(async move {
            // Torrents respect the same engine-wide concurrency limit and
            // priority ordering as HTTP downloads. The permit is held for the
            // life of the task (including seeding). If the task is aborted
            // while still waiting here, cancel()/shutdown() clear the
            // abandoned queue entry via priority_queue.remove(id).
            let _permit = priority_queue.acquire(id, priority).await;
            engine.update_state(id, DownloadState::Connecting)?;

            // Start the download (announces to trackers)
            if let Err(e) = Arc::clone(&downloader_clone).start().await {
                let error_msg = e.to_string();
                engine.update_state(
                    id,
                    DownloadState::Error {
                        kind: format!("{:?}", e),
                        message: error_msg.clone(),
                        retryable: e.is_retryable(),
                    },
                )?;
                // Persist error state to storage
                if let Some(ref storage) = engine.storage {
                    let status = engine.downloads.read().get(&id).map(|d| d.status.clone());
                    if let Some(status) = status {
                        if let Err(e) = storage.save_download(&status).await {
                            tracing::debug!("Failed to persist error state for {}: {}", id, e);
                        }
                    }
                }
                let _ = engine.event_tx.send(DownloadEvent::Failed {
                    id,
                    error: error_msg,
                    retryable: e.is_retryable(),
                });
                return Ok(());
            }

            // Update state - for magnets, we're initially fetching metadata
            engine.update_state(id, DownloadState::Downloading)?;
            let _ = engine.event_tx.send(DownloadEvent::Started { id });

            // Run the peer connection loop (handles both downloading and metadata fetching for magnets)
            let downloader_ref = Arc::clone(&downloader_clone);
            if let Err(e) = downloader_clone.run_peer_loop().await {
                let error_msg = e.to_string();
                engine.update_state(
                    id,
                    DownloadState::Error {
                        kind: format!("{:?}", e),
                        message: error_msg.clone(),
                        retryable: e.is_retryable(),
                    },
                )?;
                // Persist error state to storage
                if let Some(ref storage) = engine.storage {
                    let status = engine.downloads.read().get(&id).map(|d| d.status.clone());
                    if let Some(status) = status {
                        if let Err(e) = storage.save_download(&status).await {
                            tracing::debug!("Failed to persist error state for {}: {}", id, e);
                        }
                    }
                }
                let _ = engine.event_tx.send(DownloadEvent::Failed {
                    id,
                    error: error_msg,
                    retryable: e.is_retryable(),
                });
            } else if downloader_ref.is_complete() {
                // Magnet download completed successfully
                let should_complete = {
                    let mut downloads = engine.downloads.write();
                    if let Some(download) = downloads.get_mut(&id) {
                        if download.status.state == DownloadState::Paused {
                            false
                        } else {
                            download.status.state = DownloadState::Completed;
                            download.status.completed_at = Some(Utc::now());
                            true
                        }
                    } else {
                        false
                    }
                };

                if should_complete {
                    // Persist completed state to storage
                    if let Some(ref storage) = engine.storage {
                        let status = engine.downloads.read().get(&id).map(|d| d.status.clone());
                        if let Some(status) = status {
                            if let Err(e) = storage.save_download(&status).await {
                                tracing::debug!(
                                    "Failed to persist completed state for {}: {}",
                                    id,
                                    e
                                );
                            }
                        }
                    }

                    let _ = engine.event_tx.send(DownloadEvent::Completed { id });
                }
            }

            Ok(())
        });

        let progress_task =
            Self::spawn_torrent_progress_task(self.arc()?, id, Arc::clone(&downloader));

        // Store the handle
        {
            let mut downloads = self.downloads.write();
            if let Some(download) = downloads.get_mut(&id) {
                download.handle = Some(DownloadHandle::Torrent(TorrentDownloadHandle {
                    downloader,
                    task,
                    progress_task,
                }));
            }
        }

        Ok(())
    }

    #[cfg(feature = "torrent")]
    fn spawn_torrent_progress_task(
        engine: Arc<Self>,
        id: DownloadId,
        downloader: Arc<TorrentDownloader>,
    ) -> tokio::task::JoinHandle<()> {
        let shutdown = engine.shutdown.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => {}
                }

                let progress = downloader.progress();
                let metainfo = downloader.metainfo();
                let torrent_state = downloader.state();
                let mut state_change: Option<(DownloadState, DownloadState)> = None;
                let (send_progress, persist_torrent_data) = {
                    let mut downloads = engine.downloads.write();
                    let download = match downloads.get_mut(&id) {
                        Some(download) => download,
                        None => break,
                    };

                    if matches!(
                        download.status.state,
                        DownloadState::Error { .. } | DownloadState::Completed
                    ) {
                        break;
                    }

                    // Surface seeding at the engine level: without this the
                    // download reports "Downloading" for its entire seed life.
                    if torrent_state == crate::torrent::TorrentState::Seeding
                        && download.status.state == DownloadState::Downloading
                    {
                        download.status.state = DownloadState::Seeding;
                        state_change = Some((DownloadState::Downloading, DownloadState::Seeding));
                    }

                    let mut needs_persist = false;
                    if let Some(ref metainfo) = metainfo {
                        if download.status.torrent_info.is_none() {
                            download.status.torrent_info =
                                Some(Self::build_torrent_status_info(metainfo));
                            // Magnet metadata just arrived — persist torrent data
                            if download.status.kind == DownloadKind::Magnet {
                                needs_persist = true;
                            }
                        }
                        if download.status.metadata.name != metainfo.info.name {
                            download.status.metadata.name = metainfo.info.name.clone();
                        }
                        if download.status.metadata.filename.as_deref()
                            != Some(metainfo.info.name.as_str())
                        {
                            download.status.metadata.filename = Some(metainfo.info.name.clone());
                        }
                    }

                    download.status.progress = progress.clone();
                    (download.status.state.is_active(), needs_persist)
                };

                // Persist torrent data for magnet crash recovery (outside the lock)
                if persist_torrent_data {
                    if let Some(ref storage) = engine.storage {
                        if let Some(raw_data) = downloader.raw_torrent_data() {
                            if let Err(e) = storage.save_torrent_data(id, &raw_data).await {
                                tracing::warn!(
                                    "Failed to persist magnet torrent data for {}: {}",
                                    id,
                                    e
                                );
                            }
                        }
                    }
                }

                if let Some((old_state, new_state)) = state_change {
                    let _ = engine.event_tx.send(DownloadEvent::StateChanged {
                        id,
                        old_state,
                        new_state,
                    });
                }

                if send_progress {
                    let _ = engine
                        .event_tx
                        .send(DownloadEvent::Progress { id, progress });
                }
            }
        })
    }

    /// Start a download task
    #[cfg(feature = "http")]
    async fn start_download(
        &self,
        id: DownloadId,
        url: String,
        options: DownloadOptions,
        saved_segments: Option<Vec<crate::storage::Segment>>,
    ) -> Result<()> {
        let engine = self.arc()?;
        let http = Arc::clone(&self.http);
        let priority_queue = Arc::clone(&self.priority_queue);
        let priority = options.priority;
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let cancel_token_clone = cancel_token.clone();

        // Create shared reference for segmented download (populated by download_segmented)
        let segmented_ref: Arc<RwLock<Option<Arc<SegmentedDownload>>>> =
            Arc::new(RwLock::new(None));
        let segmented_ref_for_task = Arc::clone(&segmented_ref);

        // Update state to connecting
        self.update_state(id, DownloadState::Queued)?;

        let task = tokio::spawn(async move {
            // Acquire priority queue permit for concurrent limit, bailing out
            // promptly if the download is paused/cancelled while still queued.
            let _permit = tokio::select! {
                permit = priority_queue.acquire(id, priority) => permit,
                _ = cancel_token_clone.cancelled() => {
                    // The dropped acquire future leaves its entry in the
                    // waiting heap; remove it so it can't win a permit later.
                    priority_queue.remove(id);
                    return Ok(());
                }
            };

            // Check if cancelled before starting
            if cancel_token_clone.is_cancelled() {
                return Ok(());
            }

            // Update state to connecting then downloading
            engine.update_state(id, DownloadState::Connecting)?;
            engine.update_state(id, DownloadState::Downloading)?;
            let _ = engine.event_tx.send(DownloadEvent::Started { id });

            // Get save path and options
            #[cfg(feature = "recursive-http")]
            let (
                save_dir,
                filename,
                user_agent,
                referer,
                headers,
                cookies,
                checksum,
                mirrors,
                redirect_scope,
                recursive_group_id,
            ) = {
                let downloads = engine.downloads.read();
                let download = downloads
                    .get(&id)
                    .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
                (
                    download.status.metadata.save_dir.clone(),
                    download.status.metadata.filename.clone(),
                    download.status.metadata.user_agent.clone(),
                    download.status.metadata.referer.clone(),
                    download.status.metadata.headers.clone(),
                    download.status.metadata.cookies.clone(),
                    download.status.metadata.checksum.clone(),
                    download.status.metadata.mirrors.clone(),
                    download.redirect_scope.clone(),
                    download.recursive_group_id,
                )
            };
            #[cfg(not(feature = "recursive-http"))]
            let (save_dir, filename, user_agent, referer, headers, cookies, checksum, mirrors) = {
                let downloads = engine.downloads.read();
                let download = downloads
                    .get(&id)
                    .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
                (
                    download.status.metadata.save_dir.clone(),
                    download.status.metadata.filename.clone(),
                    download.status.metadata.user_agent.clone(),
                    download.status.metadata.referer.clone(),
                    download.status.metadata.headers.clone(),
                    download.status.metadata.cookies.clone(),
                    download.status.metadata.checksum.clone(),
                    download.status.metadata.mirrors.clone(),
                )
            };

            // Seed the resume-validator cell with the values persisted by a
            // previous session; the HTTP layer validates the partial file
            // against them and replaces them with the server's current ones.
            let validators_cell: Arc<RwLock<crate::http::ResumeValidators>> = {
                let downloads = engine.downloads.read();
                let saved = downloads
                    .get(&id)
                    .map(|d| crate::http::ResumeValidators {
                        etag: d.status.metadata.etag.clone(),
                        last_modified: d.status.metadata.last_modified.clone(),
                    })
                    .unwrap_or_default();
                Arc::new(RwLock::new(saved))
            };

            // Create progress callback
            let engine_clone = Arc::clone(&engine);
            let validators_for_progress = Arc::clone(&validators_cell);
            let progress_callback = Arc::new(move |progress: DownloadProgress| {
                // Update progress in download status
                {
                    let mut downloads = engine_clone.downloads.write();
                    if let Some(download) = downloads.get_mut(&id) {
                        download.status.progress = progress.clone();
                        // Keep the persisted metadata in sync with the
                        // validators observed by the HTTP layer so the next
                        // session can validate its resume.
                        let v = validators_for_progress.read().clone();
                        download.status.metadata.etag = v.etag;
                        download.status.metadata.last_modified = v.last_modified;
                    }
                }
                // Emit progress event
                let _ = engine_clone
                    .event_tx
                    .send(DownloadEvent::Progress { id, progress });
                #[cfg(feature = "recursive-http")]
                engine_clone.emit_recursive_job_updates_for_child(id);
            });

            // Get config for segmented downloads
            let (max_connections, min_segment_size) = {
                let config = engine.config.read();
                (
                    options
                        .max_connections
                        .unwrap_or(config.max_connections_per_download),
                    config.min_segment_size,
                )
            };

            // Per-download speed limit, layered on top of the global limiter
            let per_download_limiter = options
                .max_download_speed
                .filter(|l| *l > 0)
                .map(|l| Arc::new(crate::limiter::RateLimiter::new(Some(l))));

            // Perform the download (uses segmented if server supports it)
            let cookies_opt = if cookies.is_empty() {
                None
            } else {
                Some(cookies.as_slice())
            };
            let mirror_manager = Arc::new(MirrorManager::new(url.clone(), mirrors));
            let mut active_url = mirror_manager.current_url().to_string();
            let mut saved_segments = saved_segments;
            let result = loop {
                let attempt_url = active_url.clone();
                let attempt_result = http
                    .download_segmented_with_scope(
                        &attempt_url,
                        &save_dir,
                        filename.as_deref(),
                        user_agent.as_deref(),
                        referer.as_deref(),
                        &headers,
                        cookies_opt,
                        checksum.as_ref(),
                        #[cfg(feature = "recursive-http")]
                        redirect_scope.clone(),
                        max_connections,
                        min_segment_size,
                        cancel_token_clone.clone(),
                        saved_segments.take(),
                        Some(Arc::clone(&validators_cell)),
                        per_download_limiter.clone(),
                        Some(Arc::clone(&mirror_manager)),
                        {
                            let progress_callback = Arc::clone(&progress_callback);
                            move |progress| progress_callback(progress)
                        },
                        Some(Arc::clone(&segmented_ref_for_task)),
                    )
                    .await;

                match attempt_result {
                    Ok(result) => break Ok(result),
                    Err(err) => {
                        if let Some(next_url) = mirror_manager.failover_from(&attempt_url) {
                            if next_url != attempt_url {
                                tracing::warn!(
                                    "Download {} failed on {} ({}). Failing over to {}",
                                    id,
                                    attempt_url,
                                    err,
                                    next_url
                                );
                                active_url = next_url.to_string();
                                saved_segments = None;
                                continue;
                            }
                        }
                        break Err(err);
                    }
                }
            };

            match result {
                Ok((final_path, _segmented_download)) => {
                    // Update status to completed (but not if paused - race condition)
                    let should_complete = {
                        let mut downloads = engine.downloads.write();
                        if let Some(download) = downloads.get_mut(&id) {
                            // Don't overwrite Paused state - user paused before completion
                            if download.status.state == DownloadState::Paused {
                                false
                            } else {
                                download.status.state = DownloadState::Completed;
                                download.status.completed_at = Some(Utc::now());
                                download.status.metadata.filename = final_path
                                    .strip_prefix(&download.status.metadata.save_dir)
                                    .ok()
                                    .map(|s| s.to_string_lossy().to_string());
                                true
                            }
                        } else {
                            false
                        }
                    };

                    if should_complete {
                        // Persist completed state to storage
                        if let Some(ref storage) = engine.storage {
                            let status = engine.downloads.read().get(&id).map(|d| d.status.clone());
                            if let Some(status) = status {
                                if let Err(e) = storage.save_download(&status).await {
                                    tracing::debug!(
                                        "Failed to persist completed state for {}: {}",
                                        id,
                                        e
                                    );
                                }
                            }
                        }

                        // Clean up saved segments from storage
                        if let Some(ref storage) = engine.storage {
                            if let Err(e) = storage.delete_segments(id).await {
                                tracing::debug!("Failed to clean up segments for {}: {}", id, e);
                            }
                        }

                        let _ = engine.event_tx.send(DownloadEvent::Completed { id });
                    }

                    #[cfg(feature = "recursive-http")]
                    engine.emit_recursive_job_updates_for_child(id);
                    #[cfg(feature = "recursive-http")]
                    engine.remove_recursive_group_member(recursive_group_id, id);
                }
                Err(e) if cancel_token_clone.is_cancelled() => {
                    // Cancelled, already handled
                    let _ = e;
                }
                Err(e) => {
                    let retryable = e.is_retryable();
                    let error_msg = e.to_string();

                    // Save segment progress so retries can resume from where we left off
                    #[cfg(feature = "http")]
                    {
                        let segments: Option<Vec<Segment>> = segmented_ref_for_task
                            .read()
                            .as_ref()
                            .map(|sd| sd.segments_with_progress());

                        if let Some(ref segs) = segments {
                            // Cache in memory for storage-less resume
                            {
                                let mut downloads = engine.downloads.write();
                                if let Some(download) = downloads.get_mut(&id) {
                                    download.cached_segments = Some(segs.clone());
                                }
                            }
                            // Persist to database
                            if let Some(ref storage) = engine.storage {
                                if let Err(e) = storage.save_segments(id, segs).await {
                                    tracing::debug!(
                                        "Failed to persist segments on error for {}: {}",
                                        id,
                                        e
                                    );
                                }
                            }
                        }
                    }

                    // Update status to error
                    engine.update_state(
                        id,
                        DownloadState::Error {
                            kind: format!("{:?}", e),
                            message: error_msg.clone(),
                            retryable,
                        },
                    )?;

                    // Persist error state to storage
                    if let Some(ref storage) = engine.storage {
                        let status = engine.downloads.read().get(&id).map(|d| d.status.clone());
                        if let Some(status) = status {
                            if let Err(e) = storage.save_download(&status).await {
                                tracing::debug!("Failed to persist error state for {}: {}", id, e);
                            }
                        }
                    }

                    let _ = engine.event_tx.send(DownloadEvent::Failed {
                        id,
                        error: error_msg.clone(),
                        retryable,
                    });

                    #[cfg(feature = "recursive-http")]
                    {
                        engine.emit_recursive_job_updates_for_child(id);
                        engine.trigger_recursive_fail_fast(id, &error_msg).await?;
                        engine.remove_recursive_group_member(recursive_group_id, id);
                    }
                }
            }

            Ok(())
        });

        // Store the handle
        {
            let mut downloads = self.downloads.write();
            if let Some(download) = downloads.get_mut(&id) {
                download.handle = Some(DownloadHandle::Http(HttpDownloadHandle {
                    cancel_token,
                    task,
                    segmented_download: segmented_ref,
                }));
            }
        }

        Ok(())
    }

    /// Pause a download
    pub async fn pause(&self, id: DownloadId) -> Result<()> {
        #[cfg(feature = "torrent")]
        let mut torrent_to_stop = None;
        let (status_to_save, segments_to_save) = {
            let mut downloads = self.downloads.write();
            let download = downloads
                .get_mut(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

            // Check if can be paused. Queued downloads are pausable too:
            // their task is parked waiting for a permit and bails out when
            // the cancel token fires.
            if !download.status.state.is_active() && download.status.state != DownloadState::Queued
            {
                return Err(EngineError::InvalidState {
                    action: "pause",
                    current_state: format!("{:?}", download.status.state),
                });
            }

            // Extract segments before taking the handle (for HTTP resume)
            let segments: Option<Vec<Segment>> = match &download.handle {
                #[cfg(feature = "http")]
                Some(DownloadHandle::Http(h)) => h
                    .segmented_download
                    .read()
                    .as_ref()
                    .map(|sd| sd.segments_with_progress()),
                _ => None,
            };

            // Cancel the task
            if let Some(handle) = download.handle.take() {
                match handle {
                    #[cfg(feature = "http")]
                    DownloadHandle::Http(h) => {
                        h.cancel_token.cancel();
                        // Don't await the task here to avoid blocking
                    }
                    #[cfg(feature = "torrent")]
                    DownloadHandle::Torrent(h) => {
                        h.downloader.pause();
                        // Release the concurrency slot (or queued acquisition)
                        // and prevent a queued task from starting after pause.
                        h.task.abort();
                        h.progress_task.abort();
                        torrent_to_stop = Some(Arc::clone(&h.downloader));
                        download.handle = Some(DownloadHandle::Torrent(h));
                        // Don't await the task
                    }
                }
            }

            // Update state
            let old_state = download.status.state.clone();
            download.status.state = DownloadState::Paused;

            // Emit events
            let _ = self.event_tx.send(DownloadEvent::StateChanged {
                id,
                old_state,
                new_state: DownloadState::Paused,
            });
            let _ = self.event_tx.send(DownloadEvent::Paused { id });

            // Cache segments in memory for storage-less pause/resume
            #[cfg(feature = "http")]
            {
                download.cached_segments = segments.clone();
            }

            (download.status.clone(), segments)
        };

        #[cfg(feature = "torrent")]
        if let Some(downloader) = torrent_to_stop {
            let _ = tokio::time::timeout(Duration::from_secs(5), downloader.stop()).await;
            self.priority_queue.remove(id);
        }

        // Persist to database
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.save_download(&status_to_save).await {
                tracing::warn!("Failed to persist paused download {}: {}", id, e);
            }
            // Save HTTP segments for resume
            if let Some(segments) = segments_to_save {
                if let Err(e) = storage.save_segments(id, &segments).await {
                    tracing::warn!(
                        "Failed to persist segments for paused download {}: {}",
                        id,
                        e
                    );
                }
            }
        }

        Ok(())
    }

    /// Resume a paused download
    pub async fn resume(&self, id: DownloadId) -> Result<()> {
        // Get download info and determine type
        #[allow(unused_variables)]
        let (kind, url, options, has_torrent_handle) = {
            let downloads = self.downloads.read();
            let download = downloads
                .get(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

            // Check if can be resumed
            if download.status.state != DownloadState::Paused {
                return Err(EngineError::InvalidState {
                    action: "resume",
                    current_state: format!("{:?}", download.status.state),
                });
            }

            #[cfg(feature = "torrent")]
            let has_torrent_handle = matches!(download.handle, Some(DownloadHandle::Torrent(_)));
            #[cfg(not(feature = "torrent"))]
            let has_torrent_handle = false;

            // Start from the download's original options (kept in memory and
            // persisted across restarts) so resume keeps file selection,
            // sequential mode, connection caps, and speed limits — then let
            // the current metadata override the fields the engine mutates
            // during a download's lifetime.
            let options = DownloadOptions {
                priority: download.status.priority,
                save_dir: Some(download.status.metadata.save_dir.clone()),
                filename: download.status.metadata.filename.clone(),
                user_agent: download.status.metadata.user_agent.clone(),
                referer: download.status.metadata.referer.clone(),
                headers: download.status.metadata.headers.clone(),
                cookies: if download.status.metadata.cookies.is_empty() {
                    None
                } else {
                    Some(download.status.metadata.cookies.clone())
                },
                checksum: download.status.metadata.checksum.clone(),
                mirrors: download.status.metadata.mirrors.clone(),
                ..download.options.clone().unwrap_or_default()
            };

            (
                download.status.kind,
                download.status.metadata.url.clone(),
                options,
                has_torrent_handle,
            )
        };

        #[allow(unreachable_code)]
        {
            match kind {
                #[cfg(feature = "http")]
                DownloadKind::Http => {
                    // HTTP: restart download with saved segments
                    let url = url.ok_or_else(|| {
                        EngineError::Internal("HTTP download missing URL".to_string())
                    })?;

                    // Load saved segments from storage if available
                    let mut saved_segments = if let Some(ref storage) = self.storage {
                        match storage.load_segments(id).await {
                            Ok(segments) if !segments.is_empty() => {
                                tracing::debug!(
                                    "Loaded {} saved segments for download {}",
                                    segments.len(),
                                    id
                                );
                                Some(segments)
                            }
                            Ok(_) => None,
                            Err(e) => {
                                tracing::debug!("Failed to load segments for {}: {}", id, e);
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Fall back to in-memory cached segments (for storage-less pause/resume)
                    if saved_segments.is_none() {
                        let mut downloads = self.downloads.write();
                        if let Some(download) = downloads.get_mut(&id) {
                            saved_segments = download.cached_segments.take();
                            if saved_segments.is_some() {
                                tracing::debug!("Using cached segments for download {}", id);
                            }
                        }
                    }

                    self.start_download(id, url, options, saved_segments)
                        .await?;
                }
                #[cfg(feature = "torrent")]
                DownloadKind::Torrent | DownloadKind::Magnet => {
                    if has_torrent_handle {
                        // A paused task has released its queue slot. Rebuild
                        // from its metainfo and reacquire through the queue;
                        // flipping downloader state alone cannot restart a task.
                        let (handle, magnet_uri) = {
                            let mut downloads = self.downloads.write();
                            let download = downloads
                                .get_mut(&id)
                                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
                            let handle = match download.handle.take() {
                                Some(DownloadHandle::Torrent(h)) => h,
                                _ => {
                                    return Err(EngineError::Internal(
                                        "torrent handle disappeared".into(),
                                    ))
                                }
                            };
                            download.restored_totals = Some(handle.downloader.transfer_totals());
                            (handle, download.status.metadata.magnet_uri.clone())
                        };
                        let metainfo = handle.downloader.metainfo();
                        handle.task.abort();
                        handle.progress_task.abort();
                        let _ =
                            tokio::time::timeout(Duration::from_secs(5), handle.downloader.stop())
                                .await;
                        // Await abort completion before a replacement uses the
                        // same queue id, so the old permit cannot remove it.
                        let _ = handle.task.await;
                        self.priority_queue.remove(id);
                        let save_dir = options
                            .save_dir
                            .clone()
                            .unwrap_or_else(|| self.config.read().download_dir.clone());
                        if let Some(metainfo) = metainfo {
                            self.start_torrent(id, (*metainfo).clone(), save_dir, options)
                                .await?;
                        } else if let Some(uri) = magnet_uri {
                            self.start_magnet(id, MagnetUri::parse(&uri)?, save_dir, options)
                                .await?;
                        } else {
                            return Err(EngineError::Internal(
                                "torrent has no metainfo for resume".into(),
                            ));
                        }
                    } else {
                        // No live handle (crash recovery) — try reconstructing from stored data
                        let torrent_data = self.load_torrent_data(id).await?;

                        if let Some(data) = torrent_data {
                            let metainfo = Metainfo::parse(&data)?;
                            let save_dir = {
                                let downloads = self.downloads.read();
                                downloads
                                    .get(&id)
                                    .map(|d| d.status.metadata.save_dir.clone())
                                    .unwrap_or_else(|| self.config.read().download_dir.clone())
                            };
                            self.start_torrent(id, metainfo, save_dir, options).await?;
                        } else if let Some(ref magnet_uri) = {
                            let downloads = self.downloads.read();
                            downloads
                                .get(&id)
                                .and_then(|d| d.status.metadata.magnet_uri.clone())
                        } {
                            // Fall back to magnet URI (will re-fetch metadata from peers)
                            let magnet = MagnetUri::parse(magnet_uri)?;
                            let save_dir = {
                                let downloads = self.downloads.read();
                                downloads
                                    .get(&id)
                                    .map(|d| d.status.metadata.save_dir.clone())
                                    .unwrap_or_else(|| self.config.read().download_dir.clone())
                            };
                            self.start_magnet(id, magnet, save_dir, options).await?;
                        } else {
                            return Err(EngineError::Internal(
                                "Torrent download has no handle and no stored data for recovery"
                                    .to_string(),
                            ));
                        }
                    }
                }
                #[allow(unreachable_patterns)]
                _ => {
                    return Err(EngineError::Internal(format!(
                        "Feature not enabled for download kind {:?}",
                        kind
                    )));
                }
            }

            let _ = self.event_tx.send(DownloadEvent::Resumed { id });
        }

        Ok(())
    }

    /// Cancel a download and optionally delete files
    pub async fn cancel(&self, id: DownloadId, delete_files: bool) -> Result<()> {
        let (handle, save_path, recursive_group_id) = {
            let mut downloads = self.downloads.write();
            let download = downloads
                .remove(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

            let save_path = if delete_files {
                Some(
                    download.status.metadata.save_dir.join(
                        download
                            .status
                            .metadata
                            .filename
                            .as_deref()
                            .unwrap_or("download"),
                    ),
                )
            } else {
                None
            };

            (
                download.handle,
                save_path,
                #[cfg(all(feature = "http", feature = "recursive-http"))]
                download.recursive_group_id,
                #[cfg(not(all(feature = "http", feature = "recursive-http")))]
                None::<uuid::Uuid>,
            )
        };

        // Cancel the task if running
        if let Some(handle) = handle {
            match handle {
                #[cfg(feature = "http")]
                DownloadHandle::Http(h) => {
                    h.cancel_token.cancel();
                }
                #[cfg(feature = "torrent")]
                DownloadHandle::Torrent(h) => {
                    // Await stop() (bounded): it sets the shutdown flag,
                    // aborts discovery tasks, and announces "stopped" —
                    // dropping the future un-polled would run none of that.
                    if tokio::time::timeout(std::time::Duration::from_secs(5), h.downloader.stop())
                        .await
                        .is_err()
                    {
                        tracing::warn!("Torrent stop timed out during cancel");
                    }
                    h.progress_task.abort();
                    h.task.abort();
                }
            }
        }

        // If the task was aborted while still waiting for a concurrency slot,
        // its queue entry would otherwise sit at the head forever.
        self.priority_queue.remove(id);

        // Delete files and segments if requested
        if let Some(path) = save_path {
            if path.exists() {
                if path.is_dir() {
                    // Multi-file torrent: remove entire directory
                    tokio::fs::remove_dir_all(&path).await.ok();
                } else {
                    // Single file: remove the file
                    tokio::fs::remove_file(&path).await.ok();
                }
            }
            // Also try to remove the partial file, using the same naming
            // scheme as the HTTP layer (`file.zip` -> `file.zip.part`);
            // `with_extension` would target an unrelated `file.part`.
            #[cfg(feature = "http")]
            {
                let partial_path = crate::http::partial_path_for(&path);
                if partial_path.exists() {
                    tokio::fs::remove_file(&partial_path).await.ok();
                }
            }
        }

        // Clean up saved segments and download record from storage
        if let Some(ref storage) = self.storage {
            if let Err(e) = storage.delete_segments(id).await {
                tracing::debug!(
                    "Failed to clean up segments for cancelled download {}: {}",
                    id,
                    e
                );
            }
            if let Err(e) = storage.delete_download(id).await {
                tracing::debug!("Failed to delete download record for {}: {}", id, e);
            }
        }

        let _ = self.event_tx.send(DownloadEvent::Removed { id });

        #[cfg(all(feature = "http", feature = "recursive-http"))]
        self.emit_recursive_job_updates_for_child(id);
        #[cfg(all(feature = "http", feature = "recursive-http"))]
        self.remove_recursive_group_member(recursive_group_id, id);

        Ok(())
    }

    /// Pause every active or queued download.
    ///
    /// Queued downloads are paused as well so that freed slots do not
    /// immediately promote them back into running state. Downloads that
    /// change state while the batch is in flight are reported as skipped.
    /// Per-download `StateChanged`/`Paused` events are emitted as usual;
    /// there is no separate batch event.
    pub async fn pause_all(&self) -> BatchResult {
        let ids: Vec<DownloadId> = {
            let downloads = self.downloads.read();
            downloads
                .iter()
                .filter(|(_, d)| {
                    d.status.state.is_active() || d.status.state == DownloadState::Queued
                })
                .map(|(id, _)| *id)
                .collect()
        };

        let mut result = BatchResult::default();
        for id in ids {
            match self.pause(id).await {
                Ok(()) => result.succeeded.push(id),
                Err(EngineError::InvalidState { .. }) | Err(EngineError::NotFound(_)) => {
                    result.skipped.push(id);
                }
                Err(e) => result.failed.push((id, e)),
            }
        }
        result
    }

    /// Resume every paused download.
    ///
    /// Downloads that change state while the batch is in flight are reported
    /// as skipped. Per-download `Resumed` events are emitted as usual.
    pub async fn resume_all(&self) -> BatchResult {
        let ids: Vec<DownloadId> = {
            let downloads = self.downloads.read();
            downloads
                .iter()
                .filter(|(_, d)| d.status.state == DownloadState::Paused)
                .map(|(id, _)| *id)
                .collect()
        };

        let mut result = BatchResult::default();
        for id in ids {
            match self.resume(id).await {
                Ok(()) => result.succeeded.push(id),
                Err(EngineError::InvalidState { .. }) | Err(EngineError::NotFound(_)) => {
                    result.skipped.push(id);
                }
                Err(e) => result.failed.push((id, e)),
            }
        }
        result
    }

    /// Cancel every download, optionally deleting downloaded files.
    ///
    /// Tracked recursive job records are removed as well, since all of their
    /// children are gone after this call. Per-download `Removed` events are
    /// emitted as usual.
    pub async fn cancel_all(&self, delete_files: bool) -> BatchResult {
        let ids: Vec<DownloadId> = self.downloads.read().keys().copied().collect();

        let mut result = BatchResult::default();
        for id in ids {
            match self.cancel(id, delete_files).await {
                Ok(()) => result.succeeded.push(id),
                Err(EngineError::NotFound(_)) => result.skipped.push(id),
                Err(e) => result.failed.push((id, e)),
            }
        }

        // All children are gone; drop the now-empty recursive job records.
        #[cfg(all(feature = "http", feature = "recursive-http"))]
        {
            let jobs: Vec<crate::types::TrackedRecursiveJob> = {
                let mut recursive_jobs = self.recursive_jobs.write();
                recursive_jobs.drain().map(|(_, job)| job).collect()
            };
            for job in jobs {
                self.unregister_recursive_job_membership(&job);
                if let Some(ref storage) = self.storage {
                    if let Err(e) = storage.delete_recursive_job(job.id).await {
                        tracing::warn!("Failed to delete recursive job {}: {}", job.id, e);
                    }
                }
                let _ = self
                    .recursive_job_event_tx
                    .send(crate::types::RecursiveJobEvent::Removed { id: job.id });
            }
        }

        result
    }

    /// Get the status of a download
    pub fn status(&self, id: DownloadId) -> Option<DownloadStatus> {
        self.downloads.read().get(&id).map(|d| d.status.clone())
    }

    /// List all downloads
    pub fn list(&self) -> Vec<DownloadStatus> {
        self.downloads
            .read()
            .values()
            .map(|d| d.status.clone())
            .collect()
    }

    /// Get active downloads
    pub fn active(&self) -> Vec<DownloadStatus> {
        self.downloads
            .read()
            .values()
            .filter(|d| d.status.state.is_active())
            .map(|d| d.status.clone())
            .collect()
    }

    /// Get waiting/queued downloads
    pub fn waiting(&self) -> Vec<DownloadStatus> {
        self.downloads
            .read()
            .values()
            .filter(|d| matches!(d.status.state, DownloadState::Queued))
            .map(|d| d.status.clone())
            .collect()
    }

    /// Get stopped downloads (paused, completed, error)
    pub fn stopped(&self) -> Vec<DownloadStatus> {
        self.downloads
            .read()
            .values()
            .filter(|d| {
                matches!(
                    d.status.state,
                    DownloadState::Paused | DownloadState::Completed | DownloadState::Error { .. }
                )
            })
            .map(|d| d.status.clone())
            .collect()
    }

    /// Get global statistics
    pub fn global_stats(&self) -> GlobalStats {
        let downloads = self.downloads.read();
        let mut stats = GlobalStats::default();

        for download in downloads.values() {
            match &download.status.state {
                DownloadState::Downloading | DownloadState::Seeding | DownloadState::Connecting => {
                    stats.num_active += 1;
                    stats.download_speed += download.status.progress.download_speed;
                    stats.upload_speed += download.status.progress.upload_speed;
                }
                DownloadState::Queued => {
                    stats.num_waiting += 1;
                }
                DownloadState::Paused | DownloadState::Completed | DownloadState::Error { .. } => {
                    stats.num_stopped += 1;
                }
            }
        }

        stats
    }

    /// Subscribe to download events
    pub fn subscribe(&self) -> broadcast::Receiver<DownloadEvent> {
        self.event_tx.subscribe()
    }

    #[cfg(feature = "torrent")]
    async fn torrent_piece_manager(
        &self,
        id: DownloadId,
    ) -> Result<Arc<crate::torrent::PieceManager>> {
        let save_dir = {
            let downloads = self.downloads.read();
            let download = downloads
                .get(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
            if let Some(DownloadHandle::Torrent(h)) = &download.handle {
                if let Some(pm) = h.downloader.piece_manager_handle() {
                    return Ok(pm);
                }
            }
            download.status.metadata.save_dir.clone()
        };
        let data = self
            .load_torrent_data(id)
            .await?
            .ok_or_else(|| EngineError::InvalidState {
                action: "read torrent data",
                current_state: "torrent metadata is not available".into(),
            })?;
        Ok(Arc::new(crate::torrent::PieceManager::new(
            Arc::new(Metainfo::parse(&data)?),
            save_dir,
        )))
    }

    #[cfg(feature = "torrent")]
    async fn load_torrent_data(&self, id: DownloadId) -> Result<Option<Vec<u8>>> {
        let cached = self
            .downloads
            .read()
            .get(&id)
            .and_then(|d| d.cached_torrent_data.clone());
        if cached.is_some() {
            return Ok(cached);
        }
        match &self.storage {
            Some(storage) => storage.load_torrent_data(id).await,
            None => Ok(None),
        }
    }

    /// Re-verify a download's on-disk data.
    ///
    /// - **HTTP**: recomputes the configured checksum over the completed
    ///   file (or, with no checksum configured, checks presence and size).
    /// - **Torrents**: re-hashes every piece on disk and rebuilds the piece
    ///   bitmap, so corrupted or truncated pieces become wanted again.
    ///
    /// Queued, connecting, and downloading states are rejected; pause those
    /// downloads first. Persisted torrent metainfo allows verification without
    /// a live handle. Size-only HTTP checks do not detect same-size corruption.
    /// Use [`repair`](Self::repair) to act on a failed verification.
    pub async fn verify(&self, id: DownloadId) -> Result<VerifyReport> {
        let (kind, state, path, checksum, total_size) = {
            let downloads = self.downloads.read();
            let download = downloads
                .get(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
            (
                download.status.kind,
                download.status.state.clone(),
                download.status.metadata.save_dir.join(
                    download
                        .status
                        .metadata
                        .filename
                        .as_deref()
                        .unwrap_or(&download.status.metadata.name),
                ),
                download.status.metadata.checksum.clone(),
                download.status.progress.total_size,
            )
        };

        if matches!(
            state,
            DownloadState::Queued | DownloadState::Downloading | DownloadState::Connecting
        ) {
            return Err(EngineError::InvalidState {
                action: "verify",
                current_state: format!("{:?}", state),
            });
        }

        match kind {
            #[cfg(feature = "http")]
            DownloadKind::Http => {
                let _ = total_size;
                if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    return Ok(VerifyReport {
                        valid: false,
                        bad_pieces: 0,
                        detail: format!("file missing: {}", path.display()),
                    });
                }
                match checksum {
                    Some(expected) => {
                        let valid = crate::http::verify_checksum(&path, &expected).await?;
                        Ok(VerifyReport {
                            valid,
                            bad_pieces: 0,
                            detail: if valid {
                                format!("{} checksum matches", expected.algorithm)
                            } else {
                                format!("{} checksum mismatch", expected.algorithm)
                            },
                        })
                    }
                    None => {
                        let on_disk = tokio::fs::metadata(&path)
                            .await
                            .ok()
                            .filter(|m| m.is_file())
                            .map(|m| m.len());
                        let valid = match (on_disk, total_size) {
                            (Some(actual), Some(expected)) => actual == expected,
                            (Some(_), None) => true,
                            _ => false,
                        };
                        Ok(VerifyReport {
                            valid,
                            bad_pieces: 0,
                            detail: format!(
                                "no checksum configured; size check: {:?} vs expected {:?}",
                                on_disk, total_size
                            ),
                        })
                    }
                }
            }
            #[cfg(feature = "torrent")]
            DownloadKind::Torrent | DownloadKind::Magnet => {
                let pm = self.torrent_piece_manager(id).await?;
                let valid = pm.verify_existing().await?;
                let total = pm.progress().total_pieces;
                Ok(VerifyReport {
                    valid: pm.is_complete(),
                    bad_pieces: total.saturating_sub(valid),
                    detail: format!("{}/{} pieces verified", valid, total),
                })
            }
            #[allow(unreachable_patterns)]
            _ => Err(EngineError::Internal(
                "verify: unsupported download kind for this build".to_string(),
            )),
        }
    }

    /// Verify a download and, when the data is bad, put it back on the road
    /// to health:
    ///
    /// - **HTTP**: the corrupt file and any partial state are removed and
    ///   the download restarts from zero.
    /// - **Torrents**: reconstructs a worker and re-enters the priority queue.
    ///   Startup re-checks existing pieces so missing or corrupt data can be
    ///   fetched again, including after a finished task or engine restart.
    ///
    /// Returns the pre-repair [`VerifyReport`], not a completion result. Observe
    /// download events or status for the resulting transfer's completion.
    pub async fn repair(&self, id: DownloadId) -> Result<VerifyReport> {
        let report = self.verify(id).await?;
        if report.valid {
            return Ok(report);
        }

        let (kind, state) = {
            let downloads = self.downloads.read();
            let download = downloads
                .get(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
            (download.status.kind, download.status.state.clone())
        };

        match kind {
            #[cfg(feature = "http")]
            DownloadKind::Http => {
                // Remove the bad file and all partial state, reset progress,
                // then resume from zero.
                let path = {
                    let mut downloads = self.downloads.write();
                    let download = downloads
                        .get_mut(&id)
                        .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
                    download.status.progress.completed_size = 0;
                    download.status.state = DownloadState::Paused;
                    download.status.completed_at = None;
                    download.cached_segments = None;
                    download.status.metadata.save_dir.join(
                        download
                            .status
                            .metadata
                            .filename
                            .as_deref()
                            .unwrap_or(&download.status.metadata.name),
                    )
                };
                tokio::fs::remove_file(&path).await.ok();
                tokio::fs::remove_file(crate::http::partial_path_for(&path))
                    .await
                    .ok();
                if let Some(ref storage) = self.storage {
                    storage.delete_segments(id).await.ok();
                }
                self.resume(id).await?;
            }
            #[cfg(feature = "torrent")]
            DownloadKind::Torrent | DownloadKind::Magnet => {
                // Completed/error tasks have exited; seeding tasks retain
                // seeding state even when verification finds a bad piece.
                // Restart through resume in every case to re-fetch bad data.
                let _ = state;
                self.update_state(id, DownloadState::Paused)?;
                {
                    let mut downloads = self.downloads.write();
                    if let Some(download) = downloads.get_mut(&id) {
                        download.status.completed_at = None;
                    }
                }
                self.resume(id).await?;
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }

        Ok(report)
    }

    /// Open a streaming reader over an in-progress (or completed) download.
    ///
    /// The reader yields bytes from `offset` onward as they become available
    /// on disk, waiting (not erroring) for data that hasn't arrived yet:
    ///
    /// - **HTTP**: bytes are served from the contiguous downloaded prefix
    ///   (segmented downloads expose exactly the gap-free run from the file
    ///   start; single-connection downloads are linear anyway).
    /// - **Torrents**: bytes are served from verified pieces, and the read
    ///   position is fed back into piece selection so the swarm fetches the
    ///   pieces the reader needs next — streaming playback without manually
    ///   enabling sequential mode.
    ///
    /// The stream ends (EOF) when the download completes and the last byte
    /// has been read — or when the download fails, is cancelled, or is
    /// removed, so a reader consuming a broken download sees a short read
    /// rather than hanging. Compare bytes read against
    /// [`DownloadReader::total_size`] to distinguish the two.
    pub fn open_reader(&self, id: DownloadId, offset: u64) -> Result<DownloadReader> {
        let engine = self.arc()?;
        let total_size = {
            let downloads = self.downloads.read();
            let download = downloads
                .get(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
            download.status.progress.total_size
        };

        const CHUNK: u64 = 64 * 1024;
        let (reader_half, mut writer_half) = tokio::io::duplex(256 * 1024);

        let task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

            let mut pos = offset;
            #[cfg(feature = "torrent")]
            let mut restored_pm: Option<Arc<crate::torrent::PieceManager>> = None;
            'pump: loop {
                // Snapshot the source under the lock, then do I/O outside it.
                enum Source {
                    #[cfg(feature = "http")]
                    HttpFile {
                        path: std::path::PathBuf,
                        available_end: u64,
                    },
                    #[cfg(feature = "torrent")]
                    Torrent {
                        pm: Arc<crate::torrent::PieceManager>,
                        available_end: u64,
                    },
                    #[cfg(feature = "torrent")]
                    LoadTorrent,
                    Wait,
                    Finished,
                }

                let source = {
                    let downloads = engine.downloads.read();
                    let Some(download) = downloads.get(&id) else {
                        break 'pump; // removed
                    };
                    match &download.status.state {
                        DownloadState::Error { .. } => Source::Finished,
                        s => {
                            let completed = matches!(s, DownloadState::Completed);
                            match download.status.kind {
                                #[cfg(feature = "http")]
                                DownloadKind::Http => {
                                    // Prefer the live segmented handle for the
                                    // contiguous prefix; fall back to linear
                                    // progress (single-connection) or the
                                    // final file once completed.
                                    let segmented = match &download.handle {
                                        Some(DownloadHandle::Http(h)) => {
                                            h.segmented_download.read().clone()
                                        }
                                        _ => None,
                                    };
                                    let final_path = download.status.metadata.save_dir.join(
                                        download
                                            .status
                                            .metadata
                                            .filename
                                            .as_deref()
                                            .unwrap_or("download"),
                                    );
                                    if completed {
                                        let end = download
                                            .status
                                            .progress
                                            .total_size
                                            .unwrap_or(download.status.progress.completed_size);
                                        Source::HttpFile {
                                            path: final_path,
                                            available_end: end,
                                        }
                                    } else if let Some(sd) = segmented {
                                        Source::HttpFile {
                                            path: crate::http::partial_path_for(sd.save_path()),
                                            available_end: sd.contiguous_available(),
                                        }
                                    } else {
                                        // Paused/restored segments can have holes;
                                        // aggregate progress is not a readable prefix.
                                        let available_end = download
                                            .cached_segments
                                            .as_ref()
                                            .map(|segments| {
                                                let mut ordered: Vec<_> = segments.iter().collect();
                                                ordered.sort_by_key(|s| s.start);
                                                let mut end: u64 = 0;
                                                for segment in ordered {
                                                    if segment.start != end {
                                                        break;
                                                    }
                                                    let size = segment
                                                        .end
                                                        .saturating_sub(segment.start)
                                                        .saturating_add(1);
                                                    end = end.saturating_add(
                                                        segment.downloaded.min(size),
                                                    );
                                                    if segment.downloaded < size {
                                                        break;
                                                    }
                                                }
                                                end
                                            })
                                            .unwrap_or(download.status.progress.completed_size);
                                        Source::HttpFile {
                                            path: crate::http::partial_path_for(&final_path),
                                            available_end,
                                        }
                                    }
                                }
                                #[cfg(feature = "torrent")]
                                DownloadKind::Torrent | DownloadKind::Magnet => {
                                    let pm = match &download.handle {
                                        Some(DownloadHandle::Torrent(h)) => {
                                            h.downloader.piece_manager_handle()
                                        }
                                        _ => restored_pm.clone(),
                                    };
                                    match pm {
                                        Some(pm) => {
                                            let available = pm.contiguous_available_from(pos);
                                            Source::Torrent {
                                                available_end: pos + available,
                                                pm,
                                            }
                                        }
                                        // Rebuild a verified bitmap from persisted
                                        // metainfo after an engine restart.
                                        None if download.handle.is_none() => Source::LoadTorrent,
                                        // Magnet metadata not fetched yet, or
                                        // paused with no live handle.
                                        None => Source::Wait,
                                    }
                                }
                                #[allow(unreachable_patterns)]
                                _ => Source::Finished,
                            }
                        }
                    }
                };

                match source {
                    Source::Finished => break 'pump,
                    #[cfg(feature = "torrent")]
                    Source::LoadTorrent => match engine.torrent_piece_manager(id).await {
                        Ok(pm) => {
                            if pm.verify_existing().await.is_err() {
                                break 'pump;
                            }
                            restored_pm = Some(pm);
                        }
                        Err(_) => break 'pump,
                    },
                    Source::Wait => {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        continue 'pump;
                    }
                    #[cfg(feature = "http")]
                    Source::HttpFile {
                        path,
                        available_end,
                    } => {
                        if available_end <= pos {
                            // Nothing new. Completed + fully drained = EOF.
                            let done = {
                                let downloads = engine.downloads.read();
                                match downloads.get(&id) {
                                    Some(d) => {
                                        matches!(d.status.state, DownloadState::Completed)
                                    }
                                    None => true,
                                }
                            };
                            if done {
                                break 'pump;
                            }
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            continue 'pump;
                        }
                        let len = (available_end - pos).min(CHUNK) as usize;
                        let mut buf = vec![0u8; len];
                        // The .part may be renamed to the final name between
                        // the snapshot and this read; the next iteration
                        // re-resolves the path, so treat errors as transient.
                        let read = async {
                            let mut file = tokio::fs::File::open(&path).await?;
                            file.seek(std::io::SeekFrom::Start(pos)).await?;
                            file.read_exact(&mut buf).await?;
                            std::io::Result::Ok(())
                        }
                        .await;
                        match read {
                            Ok(()) => {
                                if writer_half.write_all(&buf).await.is_err() {
                                    break 'pump; // reader dropped
                                }
                                pos += len as u64;
                            }
                            Err(e) => {
                                // A rename can race an in-flight download,
                                // but a missing/truncated final file cannot heal.
                                if engine
                                    .status(id)
                                    .map(|d| d.state == DownloadState::Completed)
                                    .unwrap_or(true)
                                {
                                    break 'pump;
                                }
                                tracing::debug!(
                                    "Streaming read of {:?} at {} failed ({}); retrying",
                                    path,
                                    pos,
                                    e
                                );
                                tokio::time::sleep(Duration::from_millis(150)).await;
                            }
                        }
                    }
                    #[cfg(feature = "torrent")]
                    Source::Torrent { pm, available_end } => {
                        let piece_len = pm.nominal_piece_length();
                        // Keep piece selection pointed at the read head.
                        if let Some(piece) = pos.checked_div(piece_len) {
                            pm.set_stream_position(Some(piece as u32));
                        }
                        if available_end <= pos {
                            if pos >= pm.total_size() {
                                pm.set_stream_position(None);
                                break 'pump; // fully drained
                            }
                            if engine
                                .status(id)
                                .map(|d| d.state == DownloadState::Completed)
                                .unwrap_or(true)
                            {
                                pm.set_stream_position(None);
                                break 'pump;
                            }
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            continue 'pump;
                        }
                        let piece = (pos / piece_len) as u32;
                        let begin = (pos % piece_len) as u32;
                        let piece_end = ((piece as u64 + 1) * piece_len).min(pm.total_size());
                        let len = (available_end.min(piece_end) - pos).min(CHUNK) as u32;
                        match pm.read_block(piece, begin, len).await {
                            Ok(data) => {
                                if writer_half.write_all(&data).await.is_err() {
                                    pm.set_stream_position(None);
                                    break 'pump; // reader dropped
                                }
                                pos += data.len() as u64;
                                if pos >= pm.total_size() {
                                    pm.set_stream_position(None);
                                    break 'pump; // EOF
                                }
                            }
                            Err(e) => {
                                tracing::debug!(
                                    "Streaming piece read {}@{} failed ({}); retrying",
                                    piece,
                                    begin,
                                    e
                                );
                                tokio::time::sleep(Duration::from_millis(150)).await;
                            }
                        }
                    }
                }
            }
            // Dropping writer_half closes the stream: the reader sees EOF.
        });

        Ok(DownloadReader {
            inner: reader_half,
            total_size,
            task,
        })
    }

    /// Update engine configuration
    pub fn set_config(&self, config: EngineConfig) -> Result<()> {
        config.validate()?;

        self.priority_queue
            .set_max_concurrent(config.max_concurrent_downloads);

        let mut scheduler = self.scheduler.write();
        scheduler.set_defaults(BandwidthLimits {
            download: config.global_download_limit,
            upload: config.global_upload_limit,
        });
        scheduler.set_rules(config.schedule_rules.clone());
        let limits = scheduler.get_limits();
        drop(scheduler);

        #[cfg(feature = "http")]
        self.http
            .set_bandwidth_limits(limits.download, limits.upload);

        *self.config.write() = config;
        Ok(())
    }

    /// Get current configuration
    pub fn get_config(&self) -> EngineConfig {
        self.config.read().clone()
    }

    /// Set the priority of a download
    ///
    /// This affects the order in which downloads acquire slots when queued.
    /// If the download is already active, the priority is updated but
    /// won't affect scheduling until the download is paused and resumed.
    ///
    /// The priority change is persisted to storage immediately (non-blocking).
    pub fn set_priority(&self, id: DownloadId, priority: DownloadPriority) -> Result<()> {
        // Update in downloads map and get status for persistence
        let status_to_save = {
            let mut downloads = self.downloads.write();
            let download = downloads
                .get_mut(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;
            download.status.priority = priority;
            download.status.clone()
        };

        // Update in priority queue (affects scheduling if waiting)
        self.priority_queue.set_priority(id, priority);

        // Persist in background (fire-and-forget)
        if let Some(storage) = self.storage.as_ref().map(Arc::clone) {
            tokio::spawn(async move {
                if let Err(e) = storage.save_download(&status_to_save).await {
                    tracing::debug!("Failed to persist priority change for {}: {}", id, e);
                }
            });
        }

        Ok(())
    }

    /// Get the current priority of a download
    pub fn get_priority(&self, id: DownloadId) -> Option<DownloadPriority> {
        self.downloads.read().get(&id).map(|d| d.status.priority)
    }

    /// Get current bandwidth limits (accounting for schedule rules)
    pub fn get_bandwidth_limits(&self) -> BandwidthLimits {
        self.scheduler.read().get_limits()
    }

    /// Update the bandwidth schedule rules
    ///
    /// The new rules take effect immediately after evaluation.
    pub fn set_schedule_rules(&self, rules: Vec<crate::scheduler::ScheduleRule>) {
        self.config.write().schedule_rules = rules.clone();
        let limits = {
            let mut scheduler = self.scheduler.write();
            scheduler.set_rules(rules);
            scheduler.get_limits()
        };
        #[cfg(feature = "http")]
        self.http
            .set_bandwidth_limits(limits.download, limits.upload);
    }

    /// Get the current schedule rules
    pub fn get_schedule_rules(&self) -> Vec<crate::scheduler::ScheduleRule> {
        self.scheduler.read().rules().to_vec()
    }

    /// Graceful shutdown
    pub async fn shutdown(&self) -> Result<()> {
        // Persist a final snapshot while download handles still exist:
        // segment progress is only reachable through them, and the background
        // persistence task's own shutdown save is not awaited by anyone.
        if let Err(e) = self.persist_active_downloads().await {
            tracing::warn!("Final persistence on shutdown failed: {}", e);
        }

        // Signal shutdown
        self.shutdown.cancel();

        // Cancel all active downloads
        let handles: Vec<_> = {
            let mut downloads = self.downloads.write();
            downloads
                .values_mut()
                .filter_map(|d| d.handle.take())
                .collect()
        };

        for handle in handles {
            match handle {
                #[cfg(feature = "http")]
                DownloadHandle::Http(h) => {
                    h.cancel_token.cancel();
                    // Wait for task to finish (with timeout)
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h.task).await;
                }
                #[cfg(feature = "torrent")]
                DownloadHandle::Torrent(h) => {
                    // Await stop() (bounded): it sets the shutdown flag,
                    // aborts discovery tasks, and announces "stopped" —
                    // dropping the future un-polled would run none of that.
                    if tokio::time::timeout(std::time::Duration::from_secs(5), h.downloader.stop())
                        .await
                        .is_err()
                    {
                        tracing::warn!("Torrent stop timed out during shutdown");
                    }
                    h.progress_task.abort();
                    // Wait for task to finish (with timeout)
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h.task).await;
                }
            }
        }

        Ok(())
    }

    /// Helper to update download state
    fn update_state(&self, id: DownloadId, new_state: DownloadState) -> Result<()> {
        let old_state = {
            let mut downloads = self.downloads.write();
            let download = downloads
                .get_mut(&id)
                .ok_or_else(|| EngineError::NotFound(id.to_string()))?;

            let old_state = download.status.state.clone();
            download.status.state = new_state.clone();
            old_state
        };

        let _ = self.event_tx.send(DownloadEvent::StateChanged {
            id,
            old_state,
            new_state,
        });

        #[cfg(all(feature = "http", feature = "recursive-http"))]
        self.emit_recursive_job_updates_for_child(id);

        Ok(())
    }
}

impl Drop for DownloadEngine {
    fn drop(&mut self) {
        // Signal shutdown on drop
        self.shutdown.cancel();
    }
}

/// Result of [`DownloadEngine::verify`].
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// Whether the on-disk data checked out completely.
    pub valid: bool,
    /// For torrents: pieces that failed re-verification (0 for HTTP).
    pub bad_pieces: usize,
    /// Human-readable description of what was checked.
    pub detail: String,
}

/// Streaming reader over a download; see [`DownloadEngine::open_reader`].
///
/// Implements [`tokio::io::AsyncRead`]. Bytes arrive as the download makes
/// them available; the stream ends when the download's data is fully read or
/// the download stops existing (failed, cancelled, removed).
pub struct DownloadReader {
    inner: tokio::io::DuplexStream,
    total_size: Option<u64>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for DownloadReader {
    fn drop(&mut self) {
        // A pump waiting for unavailable bytes never writes to the duplex
        // stream and cannot otherwise notice that its reader disappeared.
        self.task.abort();
    }
}

impl DownloadReader {
    /// Total size of the underlying download, when known at open time.
    /// Compare against bytes read to detect a stream cut short by a failed
    /// or cancelled download.
    pub fn total_size(&self) -> Option<u64> {
        self.total_size
    }
}

impl tokio::io::AsyncRead for DownloadReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

#[cfg(all(test, feature = "http", feature = "recursive-http"))]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[tokio::test]
    async fn recursive_enqueue_rolls_back_partial_children_on_error() {
        let temp_dir = TempDir::new().expect("temp dir should be created");
        let engine = DownloadEngine::new(EngineConfig {
            download_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .expect("engine should be created");

        let manifest = crate::types::RecursiveManifest {
            root_url: "https://example.com/pub/".to_string(),
            entries: vec![
                crate::types::RecursiveEntry {
                    url: "https://example.com/pub/ok.txt".to_string(),
                    relative_path: PathBuf::from("ok.txt"),
                    size_hint: None,
                },
                crate::types::RecursiveEntry {
                    url: "ftp://example.com/pub/bad.txt".to_string(),
                    relative_path: PathBuf::from("bad.txt"),
                    size_hint: None,
                },
            ],
        };

        let err = engine
            .enqueue_recursive_manifest(
                manifest,
                DownloadOptions::default(),
                &crate::types::RecursiveOptions {
                    fail_fast: true,
                    ..Default::default()
                },
            )
            .await
            .expect_err("invalid child URL should fail recursive enqueue");

        assert!(matches!(
            err,
            EngineError::InvalidInput { field: "url", .. }
        ));
        assert!(
            engine.list().is_empty(),
            "partial child downloads should be rolled back"
        );
        assert!(
            engine.list_recursive_jobs().is_empty(),
            "tracked parent jobs should not be created on enqueue failure"
        );
        assert!(
            engine.recursive_groups.read().is_empty(),
            "fail-fast groups should not leak after rollback"
        );

        engine.shutdown().await.ok();
    }
}
