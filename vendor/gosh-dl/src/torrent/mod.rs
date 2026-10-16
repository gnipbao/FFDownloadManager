//! BitTorrent Module
//!
//! This module handles BitTorrent protocol downloads including:
//! - Torrent file parsing (metainfo)
//! - Magnet URI handling
//! - Tracker communication (HTTP/UDP)
//! - Peer wire protocol
//! - Piece management with SHA-1 verification
//! - DHT peer discovery (BEP 5)
//! - Peer Exchange (BEP 11)
//! - Local Peer Discovery (BEP 14)
//! - Choking algorithm

pub(crate) mod bencode;
pub(crate) mod choking;
pub(crate) mod dht;
pub(crate) mod lpd;
pub(crate) mod magnet;
pub(crate) mod metadata;
pub(crate) mod metainfo;
pub(crate) mod mse;
pub(crate) mod peer;
pub(crate) mod pex;
pub(crate) mod piece;
pub(crate) mod tracker;
pub(crate) mod transport;
pub(crate) mod utp;
#[cfg(feature = "http")]
pub(crate) mod webseed;

// Re-export commonly used types
pub use bencode::BencodeValue;
pub use choking::{ChokingConfig, ChokingDecision, ChokingManager, PeerStats};
pub use dht::{DhtClient, DhtManager};
pub use lpd::{LocalPeer, LpdManager, LpdService};
pub use magnet::MagnetUri;
pub use metadata::{
    MetadataFetcher, MetadataMessage, MetadataMessageType, METADATA_EXTENSION_NAME,
    OUR_METADATA_EXTENSION_ID,
};
pub use metainfo::{FileInfo, Info, Metainfo, Sha1Hash};
pub use mse::{connect_with_mse, EncryptedStream, EncryptionPolicy, MseConfig, PeerStream};
pub use peer::{ConnectionState, PeerConnection, PeerMessage, BLOCK_SIZE, OUR_PEX_EXTENSION_ID};
pub use pex::{ExtensionHandshake, PexMessage, PexState, PEX_EXTENSION_NAME};
pub use piece::{BlockRequest, PendingPiece, PieceManager, PieceProgress};
pub use tracker::{
    AnnounceEvent, AnnounceRequest, AnnounceResponse, PeerAddr, ScrapeInfo, ScrapeRequest,
    ScrapeResponse, TrackerClient,
};
pub use transport::{PeerTransport, TcpTransport, TransportType, UtpTransport};
pub use utp::{UtpConfig, UtpMux, UtpSocket};
#[cfg(feature = "http")]
pub use webseed::{
    WebSeed, WebSeedConfig, WebSeedEvent, WebSeedManager, WebSeedState, WebSeedType,
};

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tokio::sync::{broadcast, Semaphore};

use crate::config::{
    EncryptionConfig as EngineEncryptionConfig, EncryptionPolicy as EngineEncryptionPolicy,
    TransportPolicy, WebSeedConfig as EngineWebSeedConfig,
};
use crate::error::{EngineError, NetworkErrorKind, Result};
use crate::types::{DownloadEvent, DownloadId, DownloadProgress};
use pex::parse_extension_handshake;

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

/// Configuration for torrent downloads
#[derive(Debug, Clone)]
pub struct TorrentConfig {
    /// Maximum number of peers per torrent
    pub max_peers: usize,
    /// Port range for incoming connections
    pub listen_port_range: (u16, u16),
    /// Enable DHT (Phase 4)
    pub enable_dht: bool,
    /// Enable Peer Exchange (Phase 4)
    pub enable_pex: bool,
    /// Enable Local Peer Discovery (Phase 4)
    pub enable_lpd: bool,
    /// Seed ratio limit (stop seeding after this ratio)
    pub seed_ratio: Option<f64>,
    /// Maximum upload speed (bytes/sec, 0 = unlimited)
    pub max_upload_speed: u64,
    /// Maximum download speed (bytes/sec, 0 = unlimited)
    pub max_download_speed: u64,
    /// Announce interval override (0 = use tracker's)
    pub announce_interval: u64,
    /// Request timeout for blocks
    pub request_timeout: Duration,
    /// Keep-alive interval
    pub keepalive_interval: Duration,
    /// Maximum outstanding piece requests per peer
    pub max_pending_requests: usize,
    /// DHT bootstrap nodes
    pub dht_bootstrap_nodes: Vec<String>,
    /// Peer loop tick interval in milliseconds.
    /// Controls how frequently the peer loop checks for state changes and cleanup.
    pub tick_interval_ms: u64,
    /// Peer connection attempt interval in seconds.
    /// Controls how frequently we attempt to connect to new peers.
    pub connect_interval_secs: u64,
    /// Choking algorithm update interval in seconds.
    /// Controls how frequently we recalculate which peers to unchoke.
    pub choking_interval_secs: u64,
    /// Enable uTP (Micro Transport Protocol) transport for peer connections.
    /// When enabled, peers are tried via uTP first, falling back to TCP.
    pub enable_utp: bool,
    /// Enable endgame mode (request remaining blocks from several peers)
    pub enable_endgame: bool,
    /// uTP tuning applied when binding the multiplexer
    pub utp_config: UtpConfig,
    /// File preallocation strategy applied before the download starts
    pub allocation_mode: crate::config::AllocationMode,
}

impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            max_peers: 50,
            listen_port_range: (6881, 6889),
            enable_dht: true,
            enable_pex: true,
            enable_lpd: true,
            seed_ratio: None,
            max_upload_speed: 0,
            max_download_speed: 0,
            announce_interval: 0,
            request_timeout: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(120),
            max_pending_requests: 64,
            dht_bootstrap_nodes: vec![
                "router.bittorrent.com:6881".to_string(),
                "router.utorrent.com:6881".to_string(),
                "dht.transmissionbt.com:6881".to_string(),
                "dht.aelitis.com:6881".to_string(),
            ],
            tick_interval_ms: 100,
            connect_interval_secs: 5,
            choking_interval_secs: 10,
            enable_utp: false,
            enable_endgame: true,
            allocation_mode: crate::config::AllocationMode::None,
            utp_config: UtpConfig::default(),
        }
    }
}

/// State of a torrent download
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    /// Checking existing files
    Checking,
    /// Downloading metadata (for magnet links)
    Metadata,
    /// Downloading pieces
    Downloading,
    /// Seeding (complete)
    Seeding,
    /// Paused
    Paused,
    /// Stopped
    Stopped,
    /// Error
    Error,
}

/// Torrent download coordinator
#[allow(dead_code)]
pub struct TorrentDownloader {
    /// Download ID
    id: DownloadId,
    /// Metainfo (None for magnet links until metadata received)
    metainfo: RwLock<Option<Arc<Metainfo>>>,
    /// Magnet URI (if started from magnet)
    magnet: Option<MagnetUri>,
    /// Info hash
    info_hash: Sha1Hash,
    /// Save directory
    save_dir: PathBuf,
    /// Configuration
    config: TorrentConfig,
    /// Piece manager
    piece_manager: RwLock<Option<Arc<PieceManager>>>,
    /// Tracker client
    tracker_client: TrackerClient,
    /// Current state
    state: RwLock<TorrentState>,
    /// Connected peers
    peers: RwLock<HashMap<SocketAddr, PeerInfo>>,
    /// Known peer addresses (from trackers, DHT, etc.)
    known_peers: RwLock<HashSet<SocketAddr>>,
    /// Event sender
    event_tx: broadcast::Sender<DownloadEvent>,
    /// Shutdown flag
    shutdown: AtomicBool,
    /// Statistics
    stats: TorrentStats,
    /// Peer connection semaphore
    peer_semaphore: Semaphore,
    /// Metadata fetcher for magnet links (BEP 9)
    metadata_fetcher: Option<Arc<MetadataFetcher>>,
    /// Background discovery task handles (DHT, LPD, etc.)
    discovery_tasks: RwLock<Vec<tokio::task::JoinHandle<()>>>,
    /// Choking manager for peer unchoke decisions
    choking_manager: RwLock<ChokingManager>,
    /// Shared peer stats for choking algorithm (addr -> stats)
    shared_peer_stats: Arc<RwLock<HashMap<SocketAddr, PeerStats>>>,
    /// Choking decisions (addr -> should_be_unchoked)
    choking_decisions: Arc<RwLock<HashMap<SocketAddr, bool>>>,
    /// WebSeed manager (initialized when metainfo available and webseeds exist)
    #[cfg(feature = "http")]
    webseed_manager: RwLock<Option<Arc<WebSeedManager>>>,
    /// WebSeed event receiver task handle
    #[cfg(feature = "http")]
    webseed_task: RwLock<Option<tokio::task::JoinHandle<()>>>,
    /// Raw bencoded torrent data for crash recovery persistence.
    /// Set at construction for .torrent files, or when metadata is
    /// fetched for magnet links.
    raw_torrent_data: RwLock<Option<Vec<u8>>>,
    /// Piece-selection preferences that may arrive before metadata is available.
    pending_selected_files: RwLock<Option<Vec<usize>>>,
    pending_sequential: RwLock<Option<bool>>,
    /// Per-download transport and protocol settings derived from EngineConfig.
    webseed_config: RwLock<EngineWebSeedConfig>,
    mse_config: RwLock<MseConfig>,
    transport_policy: RwLock<TransportPolicy>,
    tcp_fallback: AtomicBool,
    /// uTP multiplexer for UDP-based peer connections (BEP 29).
    /// Initialized in `start()` when `config.enable_utp` is true.
    utp_mux: RwLock<Option<Arc<UtpMux>>>,
    /// Rate limiters drawn from for received piece payloads (global engine
    /// limiter plus optional per-download limiter)
    download_limiters: RwLock<Vec<Arc<crate::limiter::RateLimiter>>>,
    /// Rate limiters drawn from for sent piece payloads
    upload_limiters: RwLock<Vec<Arc<crate::limiter::RateLimiter>>>,
    /// Failed-peer backoff: consecutive failures and last attempt time.
    /// Without it the same dead candidates are re-dialed every connect tick,
    /// starving untried peers.
    peer_backoff: RwLock<HashMap<SocketAddr, (u32, Instant)>>,
    /// The TCP port we actually listen on for inbound peers (0 = not bound).
    /// This — not the configured range start — is what gets advertised.
    listen_port: std::sync::atomic::AtomicU16,
    /// Tasks driving inbound peer connections, aborted on stop().
    inbound_tasks: RwLock<Vec<tokio::task::JoinHandle<()>>>,
}

/// Information about a connected peer
#[derive(Debug)]
#[allow(dead_code)]
struct PeerInfo {
    /// Socket address
    addr: SocketAddr,
    /// Peer ID
    peer_id: Option<[u8; 20]>,
    /// Client name
    client: Option<String>,
    /// Connection established time
    connected_at: Instant,
    /// Download speed (bytes/sec)
    download_speed: u64,
    /// Upload speed (bytes/sec)
    upload_speed: u64,
    /// Total downloaded
    downloaded: u64,
    /// Total uploaded
    uploaded: u64,
    /// Is choking us
    choking: bool,
    /// Is interested in us
    interested: bool,
}

/// Torrent statistics
#[allow(dead_code)]
struct TorrentStats {
    downloaded: AtomicU64,
    uploaded: AtomicU64,
    download_speed: AtomicU64,
    upload_speed: AtomicU64,
    peers_connected: AtomicU64,
    seeders: AtomicU64,
    leechers: AtomicU64,
}

impl TorrentStats {
    fn new() -> Self {
        Self {
            downloaded: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            download_speed: AtomicU64::new(0),
            upload_speed: AtomicU64::new(0),
            peers_connected: AtomicU64::new(0),
            seeders: AtomicU64::new(0),
            leechers: AtomicU64::new(0),
        }
    }
}

impl TorrentDownloader {
    /// Create a new torrent downloader from a .torrent file
    pub fn from_torrent(
        id: DownloadId,
        metainfo: Metainfo,
        save_dir: PathBuf,
        config: TorrentConfig,
        event_tx: broadcast::Sender<DownloadEvent>,
    ) -> Result<Self> {
        let info_hash = metainfo.info_hash;
        let metainfo = Arc::new(metainfo);
        let piece_manager = Arc::new(PieceManager::new(metainfo.clone(), save_dir.clone()));

        Ok(Self {
            id,
            metainfo: RwLock::new(Some(metainfo)),
            magnet: None,
            info_hash,
            save_dir,
            config: config.clone(),
            piece_manager: RwLock::new(Some(piece_manager)),
            tracker_client: TrackerClient::new()?,
            state: RwLock::new(TorrentState::Checking),
            peers: RwLock::new(HashMap::new()),
            known_peers: RwLock::new(HashSet::new()),
            event_tx,
            shutdown: AtomicBool::new(false),
            stats: TorrentStats::new(),
            peer_semaphore: Semaphore::new(config.max_peers),
            metadata_fetcher: None, // Not needed for .torrent files
            discovery_tasks: RwLock::new(Vec::new()),
            choking_manager: RwLock::new(ChokingManager::new(ChokingConfig {
                recalculate_interval: Duration::from_secs(config.choking_interval_secs.max(1)),
                ..ChokingConfig::default()
            })),
            shared_peer_stats: Arc::new(RwLock::new(HashMap::new())),
            choking_decisions: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(feature = "http")]
            webseed_manager: RwLock::new(None),
            #[cfg(feature = "http")]
            webseed_task: RwLock::new(None),
            raw_torrent_data: RwLock::new(None),
            pending_selected_files: RwLock::new(None),
            pending_sequential: RwLock::new(None),
            webseed_config: RwLock::new(EngineWebSeedConfig::default()),
            mse_config: RwLock::new(MseConfig {
                policy: EncryptionPolicy::Disabled,
                ..MseConfig::default()
            }),
            transport_policy: RwLock::new(TransportPolicy::PreferTcp),
            tcp_fallback: AtomicBool::new(true),
            utp_mux: RwLock::new(None),
            download_limiters: RwLock::new(Vec::new()),
            upload_limiters: RwLock::new(Vec::new()),
            peer_backoff: RwLock::new(HashMap::new()),
            listen_port: std::sync::atomic::AtomicU16::new(0),
            inbound_tasks: RwLock::new(Vec::new()),
        })
    }

    /// Create a new torrent downloader from a magnet URI
    pub fn from_magnet(
        id: DownloadId,
        magnet: MagnetUri,
        save_dir: PathBuf,
        config: TorrentConfig,
        event_tx: broadcast::Sender<DownloadEvent>,
    ) -> Result<Self> {
        let info_hash = magnet.info_hash;

        Ok(Self {
            id,
            metainfo: RwLock::new(None),
            magnet: Some(magnet.clone()),
            info_hash,
            save_dir,
            config: config.clone(),
            piece_manager: RwLock::new(None),
            tracker_client: TrackerClient::new()?,
            state: RwLock::new(TorrentState::Metadata),
            peers: RwLock::new(HashMap::new()),
            known_peers: RwLock::new(HashSet::new()),
            event_tx,
            shutdown: AtomicBool::new(false),
            stats: TorrentStats::new(),
            peer_semaphore: Semaphore::new(config.max_peers),
            metadata_fetcher: Some(Arc::new(MetadataFetcher::new(info_hash))),
            discovery_tasks: RwLock::new(Vec::new()),
            choking_manager: RwLock::new(ChokingManager::new(ChokingConfig {
                recalculate_interval: Duration::from_secs(config.choking_interval_secs.max(1)),
                ..ChokingConfig::default()
            })),
            shared_peer_stats: Arc::new(RwLock::new(HashMap::new())),
            choking_decisions: Arc::new(RwLock::new(HashMap::new())),
            #[cfg(feature = "http")]
            webseed_manager: RwLock::new(None),
            #[cfg(feature = "http")]
            webseed_task: RwLock::new(None),
            raw_torrent_data: RwLock::new(None),
            pending_selected_files: RwLock::new(None),
            pending_sequential: RwLock::new(None),
            webseed_config: RwLock::new(EngineWebSeedConfig::default()),
            mse_config: RwLock::new(MseConfig {
                policy: EncryptionPolicy::Disabled,
                ..MseConfig::default()
            }),
            transport_policy: RwLock::new(TransportPolicy::PreferTcp),
            tcp_fallback: AtomicBool::new(true),
            utp_mux: RwLock::new(None),
            download_limiters: RwLock::new(Vec::new()),
            upload_limiters: RwLock::new(Vec::new()),
            peer_backoff: RwLock::new(HashMap::new()),
            listen_port: std::sync::atomic::AtomicU16::new(0),
            inbound_tasks: RwLock::new(Vec::new()),
        })
    }

    /// Get the download ID
    pub fn id(&self) -> DownloadId {
        self.id
    }

    /// Set selected files for partial download.
    ///
    /// Only pieces that contain data from the selected files will be downloaded.
    /// If `file_indices` is empty or None, all files will be downloaded.
    pub fn set_selected_files(&self, file_indices: Option<&[usize]>) {
        *self.pending_selected_files.write() = file_indices.map(|indices| indices.to_vec());
        if let Some(ref pm) = *self.piece_manager.read() {
            pm.set_selected_files(file_indices);
        }
    }

    /// Enable or disable sequential download mode.
    /// When enabled, pieces are downloaded in order for streaming support.
    pub fn set_sequential(&self, sequential: bool) {
        *self.pending_sequential.write() = Some(sequential);
        if let Some(ref pm) = *self.piece_manager.read() {
            pm.set_sequential(sequential);
        }
    }

    /// Override the webseed configuration for this download.
    pub fn set_webseed_config(&self, config: EngineWebSeedConfig) {
        *self.webseed_config.write() = config;
    }

    /// Override the MSE configuration for this download.
    pub fn set_mse_config(&self, config: EngineEncryptionConfig) {
        *self.mse_config.write() = MseConfig {
            policy: match config.policy {
                EngineEncryptionPolicy::Disabled => EncryptionPolicy::Disabled,
                EngineEncryptionPolicy::Allowed => EncryptionPolicy::Allowed,
                EngineEncryptionPolicy::Preferred => EncryptionPolicy::Preferred,
                EngineEncryptionPolicy::Required => EncryptionPolicy::Required,
            },
            allow_plaintext: config.allow_plaintext,
            allow_rc4: config.allow_rc4,
            min_padding: config.min_padding,
            max_padding: config.max_padding,
        };
    }

    /// Override the transport policy for this download.
    pub fn set_transport_policy(&self, policy: TransportPolicy, tcp_fallback: bool) {
        *self.transport_policy.write() = policy;
        self.tcp_fallback.store(tcp_fallback, Ordering::Relaxed);
    }

    fn apply_piece_manager_preferences(&self, piece_manager: &PieceManager) {
        let selected_files = self.pending_selected_files.read().clone();
        if let Some(selected_files) = selected_files.as_deref() {
            piece_manager.set_selected_files(Some(selected_files));
        }

        if let Some(sequential) = *self.pending_sequential.read() {
            piece_manager.set_sequential(sequential);
        }
    }

    /// Get the info hash
    pub fn info_hash(&self) -> &Sha1Hash {
        &self.info_hash
    }

    /// Get the info hash as hex string
    pub fn info_hash_hex(&self) -> String {
        self.info_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    /// Get the current state
    pub fn state(&self) -> TorrentState {
        *self.state.read()
    }

    /// Get the name (from metainfo or magnet)
    pub fn name(&self) -> String {
        if let Some(ref metainfo) = *self.metainfo.read() {
            metainfo.info.name.clone()
        } else if let Some(ref magnet) = self.magnet {
            magnet.name()
        } else {
            self.info_hash_hex()
        }
    }

    /// Get metainfo if available.
    pub fn metainfo(&self) -> Option<Arc<Metainfo>> {
        self.metainfo.read().clone()
    }

    /// Get raw bencoded torrent data for persistence.
    /// Returns `None` for magnet links that haven't received metadata yet.
    pub fn raw_torrent_data(&self) -> Option<Vec<u8>> {
        self.raw_torrent_data.read().clone()
    }

    /// Get progress information
    pub fn progress(&self) -> DownloadProgress {
        let (download_speed, upload_speed) = self.aggregate_transfer_rates();
        self.stats
            .download_speed
            .store(download_speed, Ordering::Relaxed);
        self.stats
            .upload_speed
            .store(upload_speed, Ordering::Relaxed);

        let pm_guard = self.piece_manager.read();

        let (completed_size, total_size) = if let Some(ref pm) = *pm_guard {
            let progress = pm.progress();
            (progress.verified_bytes, progress.total_size)
        } else {
            (0, 0)
        };

        let mut connections = self.stats.peers_connected.load(Ordering::Relaxed) as u32;

        // Include active WebSeed connections in the count
        #[cfg(feature = "http")]
        {
            if let Some(ref ws) = *self.webseed_manager.read() {
                connections += ws.active_seed_count() as u32;
            }
        }

        let progress = DownloadProgress {
            total_size: if total_size > 0 {
                Some(total_size)
            } else {
                None
            },
            completed_size,
            download_speed: self.stats.download_speed.load(Ordering::Relaxed),
            upload_speed: self.stats.upload_speed.load(Ordering::Relaxed),
            connections,
            seeders: self.stats.seeders.load(Ordering::Relaxed) as u32,
            peers: self.stats.leechers.load(Ordering::Relaxed) as u32,
            eta_seconds: self.calculate_eta(),
        };
        log_progress_invariant("torrent download", &progress);
        progress
    }

    fn aggregate_transfer_rates(&self) -> (u64, u64) {
        let stats = self.shared_peer_stats.read();
        let mut download_speed = 0u64;
        let mut upload_speed = 0u64;

        for peer_stats in stats.values() {
            download_speed = download_speed.saturating_add(peer_stats.download_rate);
            upload_speed = upload_speed.saturating_add(peer_stats.upload_rate);
        }

        (download_speed, upload_speed)
    }

    /// Calculate ETA in seconds
    fn calculate_eta(&self) -> Option<u64> {
        let pm_guard = self.piece_manager.read();
        let pm = pm_guard.as_ref()?;

        let progress = pm.progress();
        let remaining = progress.bytes_remaining();

        if remaining == 0 {
            return Some(0);
        }

        let speed = self.stats.download_speed.load(Ordering::Relaxed);
        if speed == 0 {
            return None;
        }

        Some(remaining / speed)
    }

    /// Start the download
    pub async fn start(self: Arc<Self>) -> Result<()> {
        // Verify existing files if we have metainfo
        // Clone the Arc to avoid holding the lock across await
        let pm_clone = self.piece_manager.read().clone();
        if let Some(pm) = pm_clone {
            *self.state.write() = TorrentState::Checking;

            // Preallocate output files per the configured strategy
            if let Err(e) = pm.preallocate_files(self.config.allocation_mode).await {
                tracing::warn!(
                    "File preallocation failed for {}: {}",
                    self.info_hash_hex(),
                    e
                );
            }

            let valid = pm.verify_existing().await?;
            tracing::info!(
                "Verified {} existing pieces for torrent {}",
                valid,
                self.info_hash_hex()
            );

            if pm.is_complete() {
                *self.state.write() = TorrentState::Seeding;
            } else {
                *self.state.write() = TorrentState::Downloading;
            }
        }

        // Initialize uTP multiplexer if enabled
        if self.config.enable_utp {
            let utp_addr: SocketAddr = format!("0.0.0.0:{}", self.config.listen_port_range.0)
                .parse()
                .unwrap();
            match UtpMux::bind_with_config(utp_addr, self.config.utp_config.clone()).await {
                Ok(mux) => {
                    tracing::info!("uTP multiplexer bound to {}", mux.local_addr());
                    let mux = Arc::new(mux);
                    *self.utp_mux.write() = Some(Arc::clone(&mux));

                    // Accept inbound uTP peers through the same connection
                    // loop as TCP peers.
                    let dl = Arc::clone(&self);
                    let handle = tokio::spawn(async move {
                        dl.run_utp_inbound(mux).await;
                    });
                    self.discovery_tasks.write().push(handle);
                }
                Err(e) => {
                    tracing::warn!("Failed to bind uTP multiplexer: {}", e);
                }
            }
        }

        // Bind the inbound peer listener on the configured port range,
        // walking the range if ports are taken. Without a listener the
        // advertised port accepts nothing and this client can only reach
        // peers it dialed itself. Bound BEFORE the first announce so the
        // advertised port is the real one.
        {
            let (start_port, end_port) = self.config.listen_port_range;
            let mut bound = None;
            for port in start_port..=end_port {
                match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
                    Ok(listener) => {
                        // Use the OS-reported port: with port 0 the kernel
                        // picks one, and that's what must be advertised.
                        let actual = listener.local_addr().map(|a| a.port()).unwrap_or(port);
                        bound = Some((listener, actual));
                        break;
                    }
                    Err(e) => {
                        tracing::debug!("Port {} unavailable for inbound peers: {}", port, e);
                    }
                }
            }
            match bound {
                Some((listener, port)) => {
                    self.listen_port.store(port, Ordering::Relaxed);
                    tracing::info!("Listening for inbound peers on TCP port {}", port);
                    let dl = Arc::clone(&self);
                    let handle = tokio::spawn(async move {
                        dl.run_inbound_listener(listener).await;
                    });
                    self.discovery_tasks.write().push(handle);
                }
                None => {
                    tracing::warn!(
                        "No free port in {:?} for inbound peers; running outbound-only",
                        self.config.listen_port_range
                    );
                }
            }
        }

        // Announce to trackers
        self.announce_to_trackers(AnnounceEvent::Started).await?;

        // Spawn periodic tracker re-announce: without it the tracker drops
        // this client from the swarm after one interval.
        {
            let dl = Arc::clone(&self);
            let handle = tokio::spawn(async move {
                if let Err(e) = dl.run_tracker_reannounce().await {
                    tracing::debug!("Tracker re-announce loop ended with error: {}", e);
                }
            });
            self.discovery_tasks.write().push(handle);
        }

        // Spawn DHT discovery loop
        if self.dht_enabled() {
            let dl = Arc::clone(&self);
            let handle = tokio::spawn(async move {
                if let Err(e) = dl.run_dht_discovery().await {
                    tracing::warn!("DHT discovery error: {}", e);
                }
            });
            self.discovery_tasks.write().push(handle);
        }

        // Spawn LPD discovery loop
        if self.lpd_enabled() {
            let dl = Arc::clone(&self);
            let handle = tokio::spawn(async move {
                if let Err(e) = dl.run_lpd_discovery().await {
                    tracing::warn!("LPD discovery error: {}", e);
                }
            });
            self.discovery_tasks.write().push(handle);
        }

        // Start webseed downloads if available
        #[cfg(feature = "http")]
        self.start_webseeds().await;

        Ok(())
    }

    /// Initialize and start webseed downloads if metainfo has webseeds
    #[cfg(feature = "http")]
    async fn start_webseeds(&self) {
        // Get metainfo and piece manager (both needed for webseeds)
        let metainfo = self.metainfo.read().clone();
        let piece_manager = self.piece_manager.read().clone();

        let (metainfo, piece_manager) = match (metainfo, piece_manager) {
            (Some(m), Some(p)) => (m, p),
            _ => return, // No metainfo or piece manager yet
        };

        // Check if torrent has webseeds
        if !metainfo.has_webseeds() {
            tracing::debug!("No webseeds for torrent {}", self.info_hash_hex());
            return;
        }

        let engine_webseed_config = self.webseed_config.read().clone();
        if !engine_webseed_config.enabled {
            tracing::debug!("Webseeds disabled for torrent {}", self.info_hash_hex());
            return;
        }

        tracing::info!(
            "Starting webseed downloads for torrent {} with {} seeds",
            self.info_hash_hex(),
            metainfo.all_webseeds().len()
        );

        // Create webseed manager
        let config = WebSeedConfig {
            max_connections: engine_webseed_config.max_connections,
            request_timeout: Duration::from_secs(engine_webseed_config.timeout_seconds),
            max_failures: engine_webseed_config.max_failures,
            ..WebSeedConfig::default()
        };
        let (manager, mut event_rx) =
            match WebSeedManager::new(metainfo.clone(), piece_manager.clone(), config) {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        "Failed to create WebSeedManager for torrent {}: {}",
                        self.info_hash_hex(),
                        e
                    );
                    return;
                }
            };
        let manager = Arc::new(manager);

        // Store manager reference
        *self.webseed_manager.write() = Some(Arc::clone(&manager));

        // Spawn webseed download task
        let manager_clone = Arc::clone(&manager);
        let webseed_handle = tokio::spawn(async move {
            if let Err(e) = manager_clone.run().await {
                tracing::warn!("WebSeed manager error: {}", e);
            }
        });

        // Spawn event handler task
        let piece_manager_clone = piece_manager.clone();
        let _event_tx = self.event_tx.clone(); // For future progress events
        let _info_hash_hex = self.info_hash_hex();
        let download_limiters = self.download_limiters.read().clone();
        let event_handle = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    WebSeedEvent::PieceComplete {
                        piece_index,
                        data,
                        source_url,
                    } => {
                        // Webseed traffic draws from the same download budget
                        // as peer traffic.
                        for limiter in download_limiters.iter() {
                            limiter.acquire(data.len() as u64).await;
                        }
                        // Write piece to disk (already verified in webseed manager)
                        match piece_manager_clone
                            .write_piece_from_webseed(piece_index, &data)
                            .await
                        {
                            Ok(()) => {
                                tracing::debug!(
                                    "WebSeed piece {} saved from {}",
                                    piece_index,
                                    source_url
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to save webseed piece {}: {}",
                                    piece_index,
                                    e
                                );
                            }
                        }
                    }
                    WebSeedEvent::PieceFailed {
                        piece_index,
                        source_url,
                        error,
                        ..
                    } => {
                        tracing::debug!(
                            "WebSeed piece {} failed from {}: {}",
                            piece_index,
                            source_url,
                            error
                        );
                    }
                    WebSeedEvent::SpeedUpdate { source_url, speed } => {
                        tracing::trace!("WebSeed {} speed: {} bytes/sec", source_url, speed);
                    }
                }
            }
        });

        // Store the task handles
        *self.webseed_task.write() = Some(webseed_handle);
        self.discovery_tasks.write().push(event_handle);
    }

    /// Announce to all known trackers
    async fn announce_to_trackers(&self, event: AnnounceEvent) -> Result<()> {
        let trackers = self.get_tracker_urls();

        if trackers.is_empty() {
            tracing::warn!("No trackers available for torrent {}", self.info_hash_hex());
            return Ok(());
        }

        // Get progress data in a block so the lock is dropped before await
        let (downloaded, left) = {
            let pm_guard = self.piece_manager.read();
            if let Some(ref pm) = *pm_guard {
                let progress = pm.progress();
                (progress.verified_bytes, progress.bytes_remaining())
            } else if let Some(ref magnet) = self.magnet {
                (0, magnet.exact_length.unwrap_or(1))
            } else {
                (0, 0)
            }
        };

        let request = AnnounceRequest {
            info_hash: self.info_hash,
            peer_id: *self.tracker_client.peer_id(),
            port: self.advertised_port(),
            uploaded: self.stats.uploaded.load(Ordering::Relaxed),
            downloaded,
            left,
            event,
            compact: true,
            numwant: Some(self.config.max_peers as u32),
            key: None,
            tracker_id: None,
        };

        for tracker_url in trackers {
            match self.tracker_client.announce(&tracker_url, &request).await {
                Ok(response) => {
                    tracing::info!(
                        "Announced to {}: {} peers, interval {}s",
                        tracker_url,
                        response.peers.len(),
                        response.interval
                    );

                    // Update stats
                    if let Some(complete) = response.complete {
                        self.stats.seeders.store(complete as u64, Ordering::Relaxed);
                    }
                    if let Some(incomplete) = response.incomplete {
                        self.stats
                            .leechers
                            .store(incomplete as u64, Ordering::Relaxed);
                    }

                    // Add peers to known list
                    let mut known = self.known_peers.write();
                    for peer in response.peers {
                        if let Some(addr) = peer.to_socket_addr() {
                            known.insert(addr);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to announce to {}: {}", tracker_url, e);
                }
            }
        }

        Ok(())
    }

    /// Get tracker URLs
    fn get_tracker_urls(&self) -> Vec<String> {
        if let Some(ref metainfo) = *self.metainfo.read() {
            metainfo.all_trackers()
        } else if let Some(ref magnet) = self.magnet {
            magnet.trackers.clone()
        } else {
            Vec::new()
        }
    }

    /// Pause the download
    pub fn pause(&self) {
        *self.state.write() = TorrentState::Paused;
        // Disconnect all peers and stop requesting
    }

    /// Accept inbound uTP peer connections.
    async fn run_utp_inbound(self: Arc<Self>, mux: Arc<UtpMux>) {
        let max_pending = self.config.max_pending_requests;
        loop {
            let socket = match mux.accept().await {
                Ok(socket) => socket,
                Err(e) => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    tracing::debug!("uTP accept error: {}", e);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
            };

            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let state = *self.state.read();
            if state == TorrentState::Paused || state == TorrentState::Stopped {
                continue;
            }
            // uTP carries no MSE; with encryption required, refuse inbound.
            if self.mse_config.read().policy == EncryptionPolicy::Required {
                continue;
            }
            if self.stats.peers_connected.load(Ordering::Relaxed) as usize >= self.config.max_peers
            {
                continue;
            }
            let num_pieces = match self.metainfo.read().as_ref() {
                Some(metainfo) => metainfo.info.pieces.len(),
                None => continue,
            };

            let downloader = Arc::clone(&self);
            let shared_stats = Arc::clone(&self.shared_peer_stats);
            let choking_decisions = Arc::clone(&self.choking_decisions);
            let peer_id = *self.tracker_client.peer_id();
            let info_hash = self.info_hash;
            let handle = tokio::spawn(async move {
                let addr = socket.peer_addr().ok();
                match PeerConnection::connect_utp(socket, info_hash, peer_id, num_pieces).await {
                    Ok(conn) => {
                        let addr = conn.addr();
                        tracing::info!("Accepted inbound uTP peer {}", addr);
                        if let Err(e) = Self::run_connection_loop(
                            downloader,
                            conn,
                            addr,
                            max_pending,
                            shared_stats,
                            choking_decisions,
                            false,
                        )
                        .await
                        {
                            tracing::debug!("Inbound uTP peer {} ended with error: {}", addr, e);
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Inbound uTP handshake with {:?} failed: {}", addr, e);
                    }
                }
            });

            let mut tasks = self.inbound_tasks.write();
            tasks.retain(|h| !h.is_finished());
            tasks.push(handle);
        }
    }

    /// Accept inbound peer connections and drive each through the same
    /// connection loop as outbound peers.
    async fn run_inbound_listener(self: Arc<Self>, listener: tokio::net::TcpListener) {
        let max_pending = self.config.max_pending_requests;
        loop {
            let (stream, addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    if self.shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    tracing::debug!("Inbound accept error: {}", e);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
            };

            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let state = *self.state.read();
            if state == TorrentState::Paused || state == TorrentState::Stopped {
                continue;
            }
            // No MSE responder role yet: with encryption required we must not
            // accept a plaintext inbound connection.
            if self.mse_config.read().policy == EncryptionPolicy::Required {
                tracing::debug!(
                    "Rejecting inbound peer {} (encryption required, no MSE responder yet)",
                    addr
                );
                continue;
            }
            if self.stats.peers_connected.load(Ordering::Relaxed) as usize >= self.config.max_peers
            {
                tracing::debug!("Rejecting inbound peer {} (at max_peers)", addr);
                continue;
            }
            // The handshake needs the piece count; without metadata (fresh
            // magnet) we can't serve inbound peers yet.
            let num_pieces = match self.metainfo.read().as_ref() {
                Some(metainfo) => metainfo.info.pieces.len(),
                None => {
                    tracing::debug!("Rejecting inbound peer {} (no metadata yet)", addr);
                    continue;
                }
            };

            let downloader = Arc::clone(&self);
            let shared_stats = Arc::clone(&self.shared_peer_stats);
            let choking_decisions = Arc::clone(&self.choking_decisions);
            let peer_id = *self.tracker_client.peer_id();
            let info_hash = self.info_hash;
            let handle = tokio::spawn(async move {
                match PeerConnection::accept(stream, addr, info_hash, peer_id, num_pieces).await {
                    Ok(conn) => {
                        tracing::info!("Accepted inbound peer {}", addr);
                        if let Err(e) = Self::run_connection_loop(
                            downloader,
                            conn,
                            addr,
                            max_pending,
                            shared_stats,
                            choking_decisions,
                            false,
                        )
                        .await
                        {
                            tracing::debug!("Inbound peer {} ended with error: {}", addr, e);
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Inbound handshake with {} failed: {}", addr, e);
                    }
                }
            });

            let mut tasks = self.inbound_tasks.write();
            tasks.retain(|h| !h.is_finished());
            tasks.push(handle);
        }
    }

    /// The UDP port the uTP multiplexer is bound to, if enabled.
    pub fn utp_listen_port(&self) -> Option<u16> {
        self.utp_mux.read().as_ref().map(|m| m.local_addr().port())
    }

    /// The TCP port actually bound for inbound peer connections, if any.
    pub fn bound_listen_port(&self) -> Option<u16> {
        match self.listen_port.load(Ordering::Relaxed) {
            0 => None,
            port => Some(port),
        }
    }

    /// The port to advertise to trackers/DHT/LPD: the actually-bound inbound
    /// listener port when available, else the configured range start.
    fn advertised_port(&self) -> u16 {
        let bound = self.listen_port.load(Ordering::Relaxed);
        if bound != 0 {
            bound
        } else {
            self.config.listen_port_range.0
        }
    }

    /// The piece manager, once metadata is available (immediately for
    /// .torrent downloads; after metadata fetch for magnets).
    pub fn piece_manager_handle(&self) -> Option<Arc<PieceManager>> {
        self.piece_manager.read().clone()
    }

    /// Lifetime transfer totals (downloaded, uploaded) in bytes.
    pub fn transfer_totals(&self) -> (u64, u64) {
        (
            self.stats.downloaded.load(Ordering::Relaxed),
            self.stats.uploaded.load(Ordering::Relaxed),
        )
    }

    /// Seed the transfer counters from persisted lifetime totals, so
    /// seed-ratio decisions survive restarts instead of restarting from a
    /// zero denominator.
    pub fn set_transfer_totals(&self, downloaded: u64, uploaded: u64) {
        self.stats.downloaded.store(downloaded, Ordering::Relaxed);
        self.stats.uploaded.store(uploaded, Ordering::Relaxed);
    }

    /// Install the rate limiters this torrent's transfers draw from.
    ///
    /// `download` / `upload` each usually contain the engine's global limiter
    /// plus an optional per-download one; every received or sent piece
    /// payload is booked against every limiter in the list.
    pub fn set_rate_limiters(
        &self,
        download: Vec<Arc<crate::limiter::RateLimiter>>,
        upload: Vec<Arc<crate::limiter::RateLimiter>>,
    ) {
        *self.download_limiters.write() = download;
        *self.upload_limiters.write() = upload;
    }

    /// Resume the download
    pub fn resume(&self) {
        let current = *self.state.read();
        if current == TorrentState::Paused {
            // Determine new state based on progress
            let pm_guard = self.piece_manager.read();
            if let Some(ref pm) = *pm_guard {
                if pm.is_complete() {
                    *self.state.write() = TorrentState::Seeding;
                } else {
                    *self.state.write() = TorrentState::Downloading;
                }
            }
        }
    }

    /// Stop the download
    pub async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        *self.state.write() = TorrentState::Stopped;

        // Abort all discovery tasks (DHT, LPD, etc.)
        {
            let mut tasks = self.discovery_tasks.write();
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }

        // Shut down webseed downloads
        #[cfg(feature = "http")]
        {
            if let Some(ref ws) = *self.webseed_manager.read() {
                ws.shutdown();
            }
            if let Some(handle) = self.webseed_task.write().take() {
                handle.abort();
            }
        }

        // Abort inbound peer connection tasks
        for handle in self.inbound_tasks.write().drain(..) {
            handle.abort();
        }

        // Announce stopped
        self.announce_to_trackers(AnnounceEvent::Stopped).await?;

        Ok(())
    }

    /// Check if download is complete
    pub fn is_complete(&self) -> bool {
        let pm_guard = self.piece_manager.read();
        pm_guard
            .as_ref()
            .map(|pm| pm.is_complete())
            .unwrap_or(false)
    }

    /// Get number of connected peers
    pub fn peer_count(&self) -> usize {
        self.peers.read().len()
    }

    /// Get list of known peer addresses
    pub fn known_peer_addresses(&self) -> Vec<SocketAddr> {
        self.known_peers.read().iter().cloned().collect()
    }

    /// Check if this is a private torrent.
    ///
    /// Private torrents should not use DHT, PEX, or LPD (BEP 27).
    pub fn is_private(&self) -> bool {
        self.metainfo
            .read()
            .as_ref()
            .map(|m| m.info.private)
            .unwrap_or(false)
    }

    /// Add discovered peers to the known peers list.
    ///
    /// This is used by DHT, PEX, and LPD to add discovered peers.
    pub fn add_known_peers(&self, peers: impl IntoIterator<Item = SocketAddr>) {
        let mut known = self.known_peers.write();
        for peer in peers {
            known.insert(peer);
        }
    }

    /// Get the configuration.
    pub fn config(&self) -> &TorrentConfig {
        &self.config
    }

    /// Check if DHT is enabled for this torrent.
    pub fn dht_enabled(&self) -> bool {
        self.config.enable_dht && !self.is_private()
    }

    /// Check if PEX is enabled for this torrent.
    pub fn pex_enabled(&self) -> bool {
        self.config.enable_pex && !self.is_private()
    }

    /// Check if LPD is enabled for this torrent.
    pub fn lpd_enabled(&self) -> bool {
        self.config.enable_lpd && !self.is_private()
    }

    /// Run the main peer connection loop.
    /// This spawns tasks to connect to peers and download pieces.
    pub async fn run_peer_loop(self: Arc<Self>) -> Result<()> {
        // Use configurable intervals from TorrentConfig
        let tick_duration = Duration::from_millis(self.config.tick_interval_ms);
        let connect_duration = Duration::from_secs(self.config.connect_interval_secs);
        let choking_duration = Duration::from_secs(self.config.choking_interval_secs);
        let max_pending_per_peer = self.config.max_pending_requests;

        let mut tick_interval = tokio::time::interval(tick_duration);
        let mut connect_interval = tokio::time::interval(connect_duration);
        let mut choking_interval = tokio::time::interval(choking_duration);

        // Active peer connections (addr -> connection task handle)
        let active_connections: Arc<RwLock<HashMap<SocketAddr, tokio::task::JoinHandle<()>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        loop {
            tokio::select! {
                _ = tick_interval.tick() => {
                    // Check for shutdown
                    if self.shutdown.load(Ordering::SeqCst) {
                        break;
                    }

                    // Check if state allows downloading
                    let state = *self.state.read();
                    if state == TorrentState::Paused || state == TorrentState::Stopped {
                        continue;
                    }

                    // Clean up finished connection tasks and their stats
                    {
                        let mut conns = active_connections.write();
                        let disconnected: Vec<SocketAddr> = conns
                            .iter()
                            .filter(|(_, handle)| handle.is_finished())
                            .map(|(addr, _)| *addr)
                            .collect();

                        for addr in &disconnected {
                            conns.remove(addr);
                            // Clean up peer stats and choking decisions
                            self.shared_peer_stats.write().remove(addr);
                            self.choking_decisions.write().remove(addr);
                            self.choking_manager.write().peer_disconnected(addr);
                        }
                    }

                    // Cancel stale pending pieces that haven't made progress
                    // This allows pieces to be re-selected when peers disconnect mid-download
                    {
                        let pm_guard = self.piece_manager.read();
                        if let Some(ref pm) = *pm_guard {
                            pm.cancel_stale_pieces(self.config.request_timeout);
                        }
                    }
                }

                _ = connect_interval.tick() => {
                    // Don't dial new peers while paused or stopped — a paused
                    // torrent churning through handshakes wastes everyone's
                    // sockets.
                    let state = *self.state.read();
                    if state == TorrentState::Paused || state == TorrentState::Stopped {
                        continue;
                    }

                    // Try to connect to more peers if below max
                    let current_count = active_connections.read().len();
                    if current_count < self.config.max_peers {
                        self.connect_to_new_peers(
                            Arc::clone(&active_connections),
                            max_pending_per_peer,
                        ).await;
                    }
                }

                _ = choking_interval.tick() => {
                    // Run choking algorithm to decide which peers to unchoke
                    self.run_choking_algorithm();
                }
            }

            // Check if download is complete
            let is_complete = {
                let pm_guard = self.piece_manager.read();
                pm_guard
                    .as_ref()
                    .map(|pm| pm.is_complete())
                    .unwrap_or(false)
            };

            if is_complete {
                let current_state = *self.state.read();
                if current_state != TorrentState::Seeding {
                    *self.state.write() = TorrentState::Seeding;
                    // Announce completion
                    let _ = self.announce_to_trackers(AnnounceEvent::Completed).await;
                    tracing::info!("Download complete for {}", self.name());
                }

                // Check seed ratio - stop seeding if reached
                if let Some(target_ratio) = self.config.seed_ratio {
                    if target_ratio > 0.0 {
                        let uploaded = self.stats.uploaded.load(Ordering::Relaxed);
                        let downloaded = self.stats.downloaded.load(Ordering::Relaxed);

                        // A resumed already-complete torrent has no session
                        // downloads; use the torrent size as the denominator
                        // so the ratio doesn't jump to infinity after the
                        // first uploaded block and kill seeding instantly.
                        let denominator = if downloaded > 0 {
                            downloaded
                        } else {
                            self.metainfo
                                .read()
                                .as_ref()
                                .map(|m| m.info.total_size)
                                .unwrap_or(0)
                        };

                        // Calculate current ratio (avoid division by zero)
                        let current_ratio = if denominator > 0 {
                            uploaded as f64 / denominator as f64
                        } else if uploaded > 0 {
                            f64::INFINITY
                        } else {
                            0.0
                        };

                        if current_ratio >= target_ratio {
                            tracing::info!(
                                "Seed ratio reached ({:.2} >= {:.2}), stopping torrent {}",
                                current_ratio,
                                target_ratio,
                                self.name()
                            );
                            *self.state.write() = TorrentState::Stopped;
                            // Announce stopped
                            let _ = self.announce_to_trackers(AnnounceEvent::Stopped).await;
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup: abort all connection tasks
        for (_, handle) in active_connections.write().drain() {
            handle.abort();
        }

        Ok(())
    }

    /// Run the choking algorithm to decide which peers to unchoke.
    fn run_choking_algorithm(&self) {
        let is_seeding = *self.state.read() == TorrentState::Seeding;
        let peer_stats = self.shared_peer_stats.read().clone();

        if peer_stats.is_empty() {
            return;
        }

        let decisions = self
            .choking_manager
            .write()
            .recalculate(&peer_stats, is_seeding);

        if decisions.is_empty() {
            return;
        }

        // Update choking decisions map
        let mut choking_map = self.choking_decisions.write();
        for decision in decisions {
            match decision {
                ChokingDecision::Unchoke(addr) => {
                    tracing::debug!("Choking algorithm: unchoking {}", addr);
                    choking_map.insert(addr, true);
                }
                ChokingDecision::Choke(addr) => {
                    tracing::debug!("Choking algorithm: choking {}", addr);
                    choking_map.insert(addr, false);
                }
            }
        }
    }

    /// Connect to new peers from the known_peers list
    async fn connect_to_new_peers(
        self: &Arc<Self>,
        active_connections: Arc<RwLock<HashMap<SocketAddr, tokio::task::JoinHandle<()>>>>,
        max_pending_per_peer: usize,
    ) {
        const MAX_CONNECT_PER_ROUND: usize = 5;

        // Get peers we're not connected to, skipping ones in failure backoff
        let now = Instant::now();
        let candidates: Vec<SocketAddr> = {
            let known = self.known_peers.read();
            let active = active_connections.read();
            let backoff = self.peer_backoff.read();
            known
                .iter()
                .filter(|addr| !active.contains_key(*addr))
                .filter(|addr| match backoff.get(*addr) {
                    Some((failures, last_attempt)) => {
                        // 60s doubling per consecutive failure, capped at 15min
                        let delay = Duration::from_secs(
                            60u64.saturating_mul(1 << (*failures).min(4).saturating_sub(1)),
                        )
                        .min(Duration::from_secs(900));
                        now.duration_since(*last_attempt) >= delay
                    }
                    None => true,
                })
                .take(MAX_CONNECT_PER_ROUND)
                .cloned()
                .collect()
        };

        let num_pieces = match self.metainfo.read().as_ref() {
            Some(metainfo) => metainfo.info.pieces.len(),
            None => {
                if self.metadata_fetcher.is_none() {
                    tracing::debug!("No metainfo available, skipping peer connections");
                    return;
                }
                0
            }
        };
        let peer_id = *self.tracker_client.peer_id();
        let info_hash = self.info_hash;

        for addr in candidates {
            // Check if we're at the connection limit
            let current_connections = active_connections.read().len();
            if current_connections >= self.config.max_peers {
                break;
            }

            let downloader = Arc::clone(self);
            let active_conns = Arc::clone(&active_connections);
            let shared_stats = Arc::clone(&self.shared_peer_stats);
            let choking_decisions = Arc::clone(&self.choking_decisions);

            let task = tokio::spawn(async move {
                match Self::run_single_peer_connection(
                    Arc::clone(&downloader),
                    addr,
                    info_hash,
                    peer_id,
                    num_pieces,
                    max_pending_per_peer,
                    shared_stats,
                    choking_decisions,
                )
                .await
                {
                    Ok(()) => {
                        tracing::debug!("Peer connection {} ended normally", addr);
                        downloader.peer_backoff.write().remove(&addr);
                    }
                    Err(e) => {
                        tracing::debug!("Peer connection {} failed: {}", addr, e);
                        let mut backoff = downloader.peer_backoff.write();
                        let entry = backoff.entry(addr).or_insert((0, Instant::now()));
                        entry.0 = entry.0.saturating_add(1);
                        entry.1 = Instant::now();
                    }
                }

                // Remove from active connections
                active_conns.write().remove(&addr);
            });

            active_connections.write().insert(addr, task);
        }
    }

    /// Run a connection to a single peer
    #[allow(clippy::too_many_arguments)]
    async fn run_single_peer_connection(
        downloader: Arc<Self>,
        addr: SocketAddr,
        info_hash: Sha1Hash,
        peer_id: [u8; 20],
        num_pieces: usize,
        max_pending: usize,
        shared_stats: Arc<RwLock<HashMap<SocketAddr, PeerStats>>>,
        choking_decisions: Arc<RwLock<HashMap<SocketAddr, bool>>>,
    ) -> Result<()> {
        // Connect to peer using the configured transport policy.
        let metadata_only = downloader.metadata_fetcher.is_some() && num_pieces == 0;
        let utp_mux = downloader.utp_mux.read().clone();
        let transport_policy = *downloader.transport_policy.read();
        let tcp_fallback = downloader.tcp_fallback.load(Ordering::Relaxed);
        let mse_config = downloader.mse_config.read().clone();
        let conn = match transport_policy {
            TransportPolicy::TcpOnly => {
                PeerConnection::connect_with_encryption(
                    addr,
                    info_hash,
                    peer_id,
                    num_pieces,
                    Some(&mse_config),
                )
                .await?
            }
            TransportPolicy::PreferTcp => {
                match PeerConnection::connect_with_encryption(
                    addr,
                    info_hash,
                    peer_id,
                    num_pieces,
                    Some(&mse_config),
                )
                .await
                {
                    Ok(connection) => connection,
                    Err(error) => {
                        if let Some(ref mux) = utp_mux {
                            if tcp_fallback {
                                tracing::debug!(
                                    "TCP connect to {} failed: {}, falling back to uTP",
                                    addr,
                                    error
                                );
                                let socket = mux.connect(addr).await?;
                                PeerConnection::connect_utp(socket, info_hash, peer_id, num_pieces)
                                    .await?
                            } else {
                                return Err(error);
                            }
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            TransportPolicy::UtpOnly | TransportPolicy::PreferUtp => {
                if let Some(ref mux) = utp_mux {
                    match mux.connect(addr).await {
                        Ok(socket) => {
                            match PeerConnection::connect_utp(
                                socket, info_hash, peer_id, num_pieces,
                            )
                            .await
                            {
                                Ok(c) => {
                                    tracing::info!("Connected to peer {} via uTP", addr);
                                    c
                                }
                                Err(e) => {
                                    if tcp_fallback && transport_policy != TransportPolicy::UtpOnly
                                    {
                                        tracing::debug!(
                                            "uTP handshake with {} failed: {}, falling back to TCP",
                                            addr,
                                            e
                                        );
                                        PeerConnection::connect_with_encryption(
                                            addr,
                                            info_hash,
                                            peer_id,
                                            num_pieces,
                                            Some(&mse_config),
                                        )
                                        .await?
                                    } else {
                                        return Err(e);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            if tcp_fallback && transport_policy != TransportPolicy::UtpOnly {
                                tracing::debug!(
                                    "uTP connect to {} failed: {}, falling back to TCP",
                                    addr,
                                    e
                                );
                                PeerConnection::connect_with_encryption(
                                    addr,
                                    info_hash,
                                    peer_id,
                                    num_pieces,
                                    Some(&mse_config),
                                )
                                .await?
                            } else {
                                return Err(crate::error::EngineError::network(
                                    crate::error::NetworkErrorKind::Other,
                                    format!("uTP connect failed: {}", e),
                                ));
                            }
                        }
                    }
                } else if transport_policy == TransportPolicy::UtpOnly {
                    return Err(crate::error::EngineError::network(
                        crate::error::NetworkErrorKind::Other,
                        format!(
                            "uTP transport required for {} but no multiplexer is available",
                            addr
                        ),
                    ));
                } else {
                    PeerConnection::connect_with_encryption(
                        addr,
                        info_hash,
                        peer_id,
                        num_pieces,
                        Some(&mse_config),
                    )
                    .await?
                }
            }
        };
        tracing::info!("Connected to peer {}", addr);

        Self::run_connection_loop(
            downloader,
            conn,
            addr,
            max_pending,
            shared_stats,
            choking_decisions,
            metadata_only,
        )
        .await
    }

    /// Drive an established peer connection (outbound or inbound): extension
    /// handshake, bitfield exchange, and the message loop, until shutdown,
    /// pause/stop, or a connection error.
    #[allow(clippy::too_many_arguments)]
    async fn run_connection_loop(
        downloader: Arc<Self>,
        mut conn: PeerConnection,
        addr: SocketAddr,
        max_pending: usize,
        shared_stats: Arc<RwLock<HashMap<SocketAddr, PeerStats>>>,
        choking_decisions: Arc<RwLock<HashMap<SocketAddr, bool>>>,
        metadata_only: bool,
    ) -> Result<()> {
        downloader
            .stats
            .peers_connected
            .fetch_add(1, Ordering::Relaxed);

        // Send extension handshake if supported
        if conn.supports_extensions() {
            let metadata_id = downloader
                .metadata_fetcher
                .as_ref()
                .map(|_| OUR_METADATA_EXTENSION_ID);
            conn.send_extension_handshake(metadata_id, None).await.ok();
        }

        // Initialize PEX state for this connection
        let mut pex_state = if downloader.pex_enabled() {
            Some(PexState::new(OUR_PEX_EXTENSION_ID))
        } else {
            None
        };

        // Send our bitfield
        let bitfield_opt = {
            let pm_guard = downloader.piece_manager.read();
            pm_guard.as_ref().map(|pm| pm.bitfield())
        };
        if let Some(bitfield) = bitfield_opt {
            conn.send_bitfield(&bitfield).await.ok();
        }

        // Send interested
        conn.interested().await?;

        // Track pending requests
        let mut pending_requests: HashSet<(u32, u32, u32)> = HashSet::new();
        let mut last_stats_update = Instant::now();
        let mut last_downloaded: u64 = 0;
        let mut last_uploaded: u64 = 0;

        // Track peer's ut_metadata extension ID for requesting more pieces
        let mut peer_metadata_ext_id: Option<u8> = None;

        // Initialize peer stats entry
        {
            let mut stats = shared_stats.write();
            stats.insert(
                addr,
                PeerStats {
                    addr,
                    download_rate: 0,
                    upload_rate: 0,
                    peer_interested: conn.peer_interested(),
                    am_interested: conn.am_interested(),
                    is_unchoked: !conn.am_choking(),
                    is_seeder: false,
                },
            );
        }

        loop {
            // Check shutdown
            if downloader.shutdown.load(Ordering::SeqCst) {
                break;
            }

            // Check state
            let state = *downloader.state.read();
            if state == TorrentState::Paused || state == TorrentState::Stopped {
                break;
            }

            // Receive message - DO NOT wrap in timeout as it corrupts the stream
            // The recv() has its own internal 30s timeout
            match conn.recv().await {
                Ok(msg) => {
                    match msg {
                        PeerMessage::KeepAlive => {}

                        PeerMessage::Choke => {
                            // Peer choked us, clear pending
                            tracing::debug!("[{}] Peer choked us", addr);
                            pending_requests.clear();
                        }

                        PeerMessage::Unchoke => {
                            // Can request pieces now
                            tracing::debug!("[{}] Peer unchoked us", addr);
                        }

                        PeerMessage::Have { piece_index } => {
                            // Conn tracks the peer's pieces internally; feed
                            // the availability counter so rarest-first works.
                            if let Some(ref pm) = *downloader.piece_manager.read() {
                                pm.update_piece_availability(piece_index, true);
                            }
                        }

                        PeerMessage::Bitfield { bitfield } => {
                            // Already handled internally by conn
                            let has_count = bitfield.iter().map(|b| b.count_ones()).sum::<u32>();
                            tracing::debug!(
                                "[{}] Received bitfield: peer has {} pieces",
                                addr,
                                has_count
                            );
                            if let Some(ref pm) = *downloader.piece_manager.read() {
                                pm.update_availability(conn.peer_pieces(), true);
                            }
                        }

                        PeerMessage::HaveAll => {
                            // BEP 6: Peer has all pieces - handled internally by conn
                            tracing::debug!("[{}] Peer has all pieces (HaveAll)", addr);
                            if let Some(ref pm) = *downloader.piece_manager.read() {
                                pm.update_availability(conn.peer_pieces(), true);
                            }
                        }

                        PeerMessage::HaveNone => {
                            // BEP 6: Peer has no pieces - handled internally by conn
                            tracing::debug!("[{}] Peer has no pieces (HaveNone)", addr);
                        }

                        PeerMessage::Piece {
                            index,
                            begin,
                            block,
                        } => {
                            // Remove from pending
                            pending_requests.remove(&(index, begin, block.len() as u32));

                            // Book the payload against the download budget
                            // (delays further requests once over the limit).
                            let limiters = downloader.download_limiters.read().clone();
                            for limiter in limiters {
                                limiter.acquire(block.len() as u64).await;
                            }

                            // Add block to piece manager
                            let add_result = {
                                let pm_guard = downloader.piece_manager.read();
                                if let Some(ref pm) = *pm_guard {
                                    Some((
                                        pm.add_block(index, begin, block.clone()),
                                        Arc::clone(pm),
                                    ))
                                } else {
                                    None
                                }
                            };

                            if let Some((result, pm)) = add_result {
                                match result {
                                    Ok(complete) => {
                                        // Update stats
                                        downloader
                                            .stats
                                            .downloaded
                                            .fetch_add(block.len() as u64, Ordering::Relaxed);

                                        if complete {
                                            // Verify and save the piece (now pm is owned, not borrowed)
                                            match pm.verify_and_save(index).await {
                                                Ok(true) => {
                                                    tracing::debug!(
                                                        "Piece {} verified and saved",
                                                        index
                                                    );
                                                }
                                                Ok(false) => {
                                                    tracing::warn!(
                                                        "Piece {} failed verification",
                                                        index
                                                    );
                                                }
                                                Err(e) => {
                                                    tracing::error!(
                                                        "Error saving piece {}: {}",
                                                        index,
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // This can happen if a stale piece was cancelled - not a real error
                                        tracing::debug!("Block for cancelled piece: {}", e);
                                    }
                                }
                            }
                        }

                        PeerMessage::Request {
                            index,
                            begin,
                            length,
                        } => {
                            // Peer is requesting a block from us (for seeding)

                            // Check if we're choking this peer - ignore request if so
                            if conn.am_choking() {
                                tracing::trace!(
                                    "Ignoring request from {} - peer is choked (piece={}, offset={}, len={})",
                                    addr, index, begin, length
                                );
                                continue;
                            }

                            // Read the block from disk
                            // Clone the Arc to avoid holding the RwLock guard across await
                            let pm_opt = downloader.piece_manager.read().clone();
                            let block_result = match pm_opt {
                                Some(ref pm) => Some(pm.read_block(index, begin, length).await),
                                None => None,
                            };

                            match block_result {
                                Some(Ok(block)) => {
                                    // Book the payload against the upload
                                    // budget before sending.
                                    let limiters = downloader.upload_limiters.read().clone();
                                    for limiter in limiters {
                                        limiter.acquire(block.len() as u64).await;
                                    }

                                    // Send the piece to the peer
                                    match conn.send_piece(index, begin, block.clone()).await {
                                        Ok(()) => {
                                            // Update upload statistics
                                            downloader
                                                .stats
                                                .uploaded
                                                .fetch_add(block.len() as u64, Ordering::Relaxed);
                                            tracing::trace!(
                                                "Sent block to {}: piece={}, offset={}, len={}",
                                                addr,
                                                index,
                                                begin,
                                                length
                                            );
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Failed to send piece to {}: {}",
                                                addr,
                                                e
                                            );
                                        }
                                    }
                                }
                                Some(Err(e)) => {
                                    // We don't have the piece or invalid request
                                    tracing::debug!(
                                        "Cannot serve request from {}: {} (piece={}, offset={}, len={})",
                                        addr, e, index, begin, length
                                    );
                                    // Per BitTorrent protocol, just ignore invalid requests
                                }
                                None => {
                                    tracing::debug!(
                                        "No piece manager available to serve request from {}",
                                        addr
                                    );
                                }
                            }
                        }

                        PeerMessage::Extended { id, payload } => {
                            // Handle extension messages
                            if id == 0 {
                                // Extension handshake
                                if let Ok(handshake) = parse_extension_handshake(&payload) {
                                    if let Some(pex_id) =
                                        handshake.extensions.get(PEX_EXTENSION_NAME)
                                    {
                                        tracing::debug!(
                                            "Peer {} supports PEX (id={})",
                                            addr,
                                            pex_id
                                        );
                                        // Update PEX state with peer's extension ID
                                        if let Some(ref mut state) = pex_state {
                                            state.set_peer_extension_id(*pex_id);
                                        }
                                    }
                                    // Check for ut_metadata support
                                    if let Some(metadata_id) =
                                        handshake.extensions.get(METADATA_EXTENSION_NAME)
                                    {
                                        tracing::debug!(
                                            "Peer {} supports ut_metadata (id={})",
                                            addr,
                                            metadata_id
                                        );
                                        // Store peer's metadata extension ID for later requests
                                        peer_metadata_ext_id = Some(*metadata_id);
                                        // If we need metadata, request it
                                        if let Some(ref fetcher) = downloader.metadata_fetcher {
                                            if !fetcher.is_complete().await {
                                                let needed = fetcher.get_needed_pieces().await;
                                                for piece in needed.into_iter().take(2) {
                                                    let msg = MetadataMessage::request(piece);
                                                    if conn
                                                        .send_extension_message(
                                                            *metadata_id,
                                                            msg.encode(),
                                                        )
                                                        .await
                                                        .is_ok()
                                                    {
                                                        fetcher.mark_requested(piece).await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else if id == OUR_PEX_EXTENSION_ID {
                                // PEX message
                                downloader.process_pex_message(&payload);
                            } else if id == OUR_METADATA_EXTENSION_ID {
                                // ut_metadata message
                                if let Some(ref fetcher) = downloader.metadata_fetcher {
                                    if let Ok(msg) = MetadataMessage::parse(&payload) {
                                        if let Ok(complete) = fetcher.process_message(msg).await {
                                            if complete {
                                                // Metadata complete! Initialize piece manager
                                                if let Ok(Some(metainfo)) =
                                                    fetcher.parse_metainfo().await
                                                {
                                                    tracing::info!(
                                                        "Metadata received for {}",
                                                        downloader.name()
                                                    );
                                                    let metainfo = Arc::new(metainfo);
                                                    let pm = Arc::new(PieceManager::new(
                                                        metainfo.clone(),
                                                        downloader.save_dir.clone(),
                                                    ));
                                                    downloader.apply_piece_manager_preferences(&pm);

                                                    // Verify existing files (same as start() does for torrent files)
                                                    *downloader.state.write() =
                                                        TorrentState::Checking;
                                                    match pm.verify_existing().await {
                                                        Ok(valid) => {
                                                            tracing::info!(
                                                                "Verified {} existing pieces for {}",
                                                                valid,
                                                                downloader.name()
                                                            );
                                                        }
                                                        Err(e) => {
                                                            tracing::warn!(
                                                                "Failed to verify existing pieces: {}",
                                                                e
                                                            );
                                                        }
                                                    }

                                                    *downloader.metainfo.write() = Some(metainfo);
                                                    *downloader.piece_manager.write() =
                                                        Some(pm.clone());

                                                    // Store raw torrent bytes for crash recovery
                                                    if let Some(raw_bytes) =
                                                        fetcher.raw_torrent_bytes().await
                                                    {
                                                        *downloader.raw_torrent_data.write() =
                                                            Some(raw_bytes);
                                                    }

                                                    // Set final state based on completion
                                                    if pm.is_complete() {
                                                        *downloader.state.write() =
                                                            TorrentState::Seeding;
                                                    } else {
                                                        *downloader.state.write() =
                                                            TorrentState::Downloading;
                                                    }
                                                }
                                            } else {
                                                // Not complete yet - request more pieces
                                                if let Some(peer_ext_id) = peer_metadata_ext_id {
                                                    let needed = fetcher.get_needed_pieces().await;
                                                    for piece in needed.into_iter().take(2) {
                                                        let msg = MetadataMessage::request(piece);
                                                        if conn
                                                            .send_extension_message(
                                                                peer_ext_id,
                                                                msg.encode(),
                                                            )
                                                            .await
                                                            .is_ok()
                                                        {
                                                            fetcher.mark_requested(piece).await;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        _ => {}
                    }
                }

                Err(EngineError::Network {
                    kind: NetworkErrorKind::Timeout,
                    ..
                }) => {
                    // The receive timeout is not fatal: compliant clients
                    // keep-alive only every ~120s. Send our own keep-alive
                    // (rate-limited) so the peer doesn't idle-kill us, apply
                    // any pending choking decision so an idle-but-interested
                    // peer still gets unchoked promptly, and disconnect only
                    // after a prolonged silence.
                    const IDLE_DISCONNECT: Duration = Duration::from_secs(150);
                    if conn.idle_time() >= IDLE_DISCONNECT {
                        tracing::debug!(
                            "Peer {} idle for {:?}, disconnecting",
                            addr,
                            conn.idle_time()
                        );
                        break;
                    }
                    if let Err(e) = conn.keep_alive().await {
                        tracing::debug!("Peer {} keep-alive failed: {}", addr, e);
                        break;
                    }

                    let choking_decision: Option<bool> = {
                        let decisions = choking_decisions.read();
                        decisions.get(&addr).copied()
                    };
                    if let Some(should_unchoke) = choking_decision {
                        let currently_unchoked = !conn.am_choking();
                        if should_unchoke && !currently_unchoked {
                            if conn.unchoke().await.is_ok() {
                                tracing::debug!("Unchoked idle peer {}", addr);
                            }
                        } else if !should_unchoke
                            && currently_unchoked
                            && conn.choke().await.is_ok()
                        {
                            tracing::debug!("Choked idle peer {}", addr);
                        }
                    }
                    continue;
                }
                Err(e) => {
                    // Connection error from recv
                    tracing::debug!("Peer {} recv error: {}", addr, e);
                    break;
                }
            }

            if metadata_only && downloader.metainfo.read().is_some() {
                tracing::debug!(
                    "Metadata available for {}, reconnecting peer {}",
                    downloader.name(),
                    addr
                );
                break;
            }

            // Request more blocks if we have capacity and peer is unchoked
            if !conn.peer_choking() && pending_requests.len() < max_pending {
                // Get all blocks to request in one pass
                let mut in_endgame = false;
                let blocks_to_request: Vec<BlockRequest> = {
                    let pm_guard = downloader.piece_manager.read();
                    if let Some(ref pm) = *pm_guard {
                        let peer_pieces = conn.peer_pieces();
                        let slots_available = max_pending - pending_requests.len();

                        // Check for endgame mode (10 or fewer pieces remaining)
                        let endgame_pieces = if downloader.config.enable_endgame {
                            pm.endgame_pieces()
                        } else {
                            Vec::new()
                        };
                        in_endgame = !endgame_pieces.is_empty();
                        if !endgame_pieces.is_empty() {
                            let mut pending = pm.pending_pieces();
                            for piece_idx in endgame_pieces {
                                if !pending.contains(&piece_idx)
                                    && !pm.have_piece(piece_idx as usize)
                                    && pm.start_piece(piece_idx).is_some()
                                {
                                    pending.insert(piece_idx);
                                }
                            }

                            // In endgame mode: request all pending blocks that the peer has
                            pm.endgame_requests()
                                .into_iter()
                                .filter(|req| conn.peer_has_piece(req.piece as usize))
                                .take(slots_available)
                                .collect()
                        } else {
                            // Normal mode: collect blocks to fill the pipeline
                            let mut blocks = Vec::with_capacity(slots_available);

                            // First, get unrequested blocks from pieces we're already downloading
                            for piece_idx in pm.pending_pieces() {
                                if blocks.len() >= slots_available {
                                    break;
                                }
                                if conn.peer_has_piece(piece_idx as usize) {
                                    for b in pm.get_block_requests(piece_idx) {
                                        if blocks.len() >= slots_available {
                                            break;
                                        }
                                        blocks.push(b);
                                    }
                                }
                            }

                            // Then select new pieces if we still have capacity
                            while blocks.len() < slots_available {
                                if let Some(piece_idx) = pm.select_piece(peer_pieces) {
                                    if pm.start_piece(piece_idx).is_some() {
                                        for b in pm.get_block_requests(piece_idx) {
                                            if blocks.len() >= slots_available {
                                                break;
                                            }
                                            blocks.push(b);
                                        }
                                    }
                                } else {
                                    break;
                                }
                            }

                            blocks
                        }
                    } else {
                        Vec::new()
                    }
                };

                // Send all requests
                for block in &blocks_to_request {
                    let key = (block.piece, block.offset, block.length);
                    if !pending_requests.contains(&key)
                        && conn
                            .request_block(block.piece, block.offset, block.length)
                            .await
                            .is_ok()
                    {
                        pending_requests.insert(key);
                        // Record the request so other peers aren't handed the
                        // same block (outside endgame). Marks are cleared on
                        // block receipt and by stale-piece cleanup.
                        if !in_endgame {
                            if let Some(ref pm) = *downloader.piece_manager.read() {
                                pm.mark_block_requested(
                                    block.piece,
                                    block.offset / crate::torrent::peer::BLOCK_SIZE,
                                    addr.port() as usize,
                                );
                            }
                        }
                    }
                }

                // In endgame every remaining block is requested from several
                // peers; cancel our copy of any request another peer already
                // satisfied so the duplicate bytes never hit the wire.
                if in_endgame && !pending_requests.is_empty() {
                    let pm_opt = downloader.piece_manager.read().clone();
                    if let Some(ref pm) = pm_opt {
                        let satisfied: Vec<(u32, u32, u32)> = pending_requests
                            .iter()
                            .filter(|(piece, offset, _)| pm.is_block_received(*piece, *offset))
                            .copied()
                            .collect();
                        for (piece, offset, length) in satisfied {
                            let _ = conn.cancel_request(piece, offset, length).await;
                            pending_requests.remove(&(piece, offset, length));
                        }
                    }
                }
            }

            // Send PEX message if enabled, supported, and interval elapsed
            if let Some(ref mut state) = pex_state {
                if state.is_supported() && state.can_send() {
                    // Get current known peers, excluding this peer
                    let current_peers: HashSet<SocketAddr> = {
                        let peers = downloader.known_peers.read();
                        peers.iter().filter(|p| **p != addr).cloned().collect()
                    };

                    if let Some(pex_msg) = state.build_message(&current_peers) {
                        if let Err(e) = conn.send_pex(&pex_msg).await {
                            tracing::debug!("Failed to send PEX to {}: {}", addr, e);
                        } else {
                            tracing::debug!(
                                "Sent PEX message to {} with {} added peers",
                                addr,
                                pex_msg.added.len()
                            );
                        }
                    }
                }
            }

            // Update peer stats periodically (every second)
            if last_stats_update.elapsed() >= Duration::from_secs(1) {
                let elapsed_secs = last_stats_update.elapsed().as_secs_f64();
                let current_downloaded = conn.downloaded();
                let current_uploaded = conn.uploaded();

                let download_rate = if elapsed_secs > 0.0 {
                    ((current_downloaded - last_downloaded) as f64 / elapsed_secs) as u64
                } else {
                    0
                };
                let upload_rate = if elapsed_secs > 0.0 {
                    ((current_uploaded - last_uploaded) as f64 / elapsed_secs) as u64
                } else {
                    0
                };

                last_downloaded = current_downloaded;
                last_uploaded = current_uploaded;
                last_stats_update = Instant::now();

                // Update shared stats for choking algorithm
                {
                    let mut stats = shared_stats.write();
                    if let Some(peer_stats) = stats.get_mut(&addr) {
                        peer_stats.download_rate = download_rate;
                        peer_stats.upload_rate = upload_rate;
                        peer_stats.peer_interested = conn.peer_interested();
                        peer_stats.am_interested = conn.am_interested();
                        peer_stats.is_unchoked = !conn.am_choking();
                    }
                }
            }

            // Check and apply choking decisions from the choking manager
            // Get the decision first, then drop the lock before awaiting
            let choking_decision: Option<bool> = {
                let decisions = choking_decisions.read();
                decisions.get(&addr).copied()
            };

            if let Some(should_unchoke) = choking_decision {
                let currently_unchoked = !conn.am_choking();
                if should_unchoke && !currently_unchoked {
                    // Need to unchoke this peer
                    if conn.unchoke().await.is_ok() {
                        tracing::debug!("Unchoked peer {}", addr);
                    }
                } else if !should_unchoke && currently_unchoked {
                    // Need to choke this peer
                    if conn.choke().await.is_ok() {
                        tracing::debug!("Choked peer {}", addr);
                    }
                }
            }
        }

        // Clean up peer stats on exit
        shared_stats.write().remove(&addr);
        choking_decisions.write().remove(&addr);

        // Remove this peer's pieces from the availability counters so
        // rarest-first reflects the live swarm.
        if let Some(ref pm) = *downloader.piece_manager.read() {
            pm.update_availability(conn.peer_pieces(), false);
        }

        downloader
            .stats
            .peers_connected
            .fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }

    /// Run DHT peer discovery in the background.
    ///
    /// Periodically queries the DHT for peers and adds them to known_peers.
    pub async fn run_dht_discovery(self: Arc<Self>) -> Result<()> {
        if !self.dht_enabled() {
            tracing::debug!("DHT disabled for torrent {}", self.name());
            return Ok(());
        }

        let listen_port = self.advertised_port();
        let bootstrap_nodes = &self.config.dht_bootstrap_nodes;

        // Use custom bootstrap nodes if configured, otherwise use defaults
        let dht_client = if bootstrap_nodes.is_empty() {
            match DhtClient::new(listen_port) {
                Ok(client) => Arc::new(client),
                Err(e) => {
                    tracing::warn!("Failed to create DHT client: {}", e);
                    return Ok(());
                }
            }
        } else {
            match DhtClient::with_bootstrap(listen_port, bootstrap_nodes) {
                Ok(client) => Arc::new(client),
                Err(e) => {
                    tracing::warn!("Failed to create DHT client with custom bootstrap: {}", e);
                    return Ok(());
                }
            }
        };

        const DHT_LOOKUP_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes
        const DHT_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1800); // 30 minutes

        let mut lookup_timer = tokio::time::interval(DHT_LOOKUP_INTERVAL);
        let mut announce_timer = tokio::time::interval(DHT_ANNOUNCE_INTERVAL);

        // Initial announce
        if let Err(e) = dht_client.announce(&self.info_hash) {
            tracing::debug!("DHT announce failed: {}", e);
        }

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            let state = *self.state.read();
            if state == TorrentState::Stopped {
                break;
            }

            tokio::select! {
                _ = lookup_timer.tick() => {
                    // Skip lookup if paused
                    if state == TorrentState::Paused {
                        continue;
                    }

                    // Find peers from DHT
                    let peers = dht_client.find_peers_timeout(&self.info_hash, Duration::from_secs(30)).await;
                    if !peers.is_empty() {
                        tracing::debug!("DHT found {} peers for {}", peers.len(), self.name());
                        self.add_known_peers(peers);
                    }
                }

                _ = announce_timer.tick() => {
                    // Re-announce to DHT
                    if let Err(e) = dht_client.announce(&self.info_hash) {
                        tracing::debug!("DHT re-announce failed: {}", e);
                    }
                }
            }
        }

        dht_client.shutdown();
        Ok(())
    }

    /// Run LPD (Local Peer Discovery) in the background.
    ///
    /// Announces to the local network and listens for other peers.
    pub async fn run_lpd_discovery(self: Arc<Self>) -> Result<()> {
        if !self.lpd_enabled() {
            tracing::debug!("LPD disabled for torrent {}", self.name());
            return Ok(());
        }

        let listen_port = self.advertised_port();
        let lpd_service = match LpdService::new(listen_port).await {
            Ok(service) => Arc::new(service),
            Err(e) => {
                tracing::warn!("Failed to create LPD service: {}", e);
                return Ok(());
            }
        };

        // Track this torrent
        lpd_service.track(self.info_hash).await;

        // Start listening for announcements
        let mut lpd_rx = lpd_service.listen();

        const LPD_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes
        let mut announce_timer = tokio::time::interval(LPD_ANNOUNCE_INTERVAL);

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            let state = *self.state.read();
            if state == TorrentState::Stopped {
                break;
            }

            tokio::select! {
                result = lpd_rx.recv() => {
                    match result {
                        Ok(local_peer) => {
                            // Only add peers for our torrent
                            if local_peer.info_hash == self.info_hash {
                                tracing::debug!("LPD discovered peer {} for {}", local_peer.addr, self.name());
                                self.add_known_peers(std::iter::once(local_peer.addr));
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Missed some messages, continue
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }

                _ = announce_timer.tick() => {
                    // Skip announce if paused
                    if state == TorrentState::Paused {
                        continue;
                    }

                    // Announce to local network
                    if let Err(e) = lpd_service.announce(&self.info_hash).await {
                        tracing::debug!("LPD announce failed: {}", e);
                    }
                }
            }
        }

        lpd_service.shutdown();
        Ok(())
    }

    /// Run periodic tracker re-announcements.
    pub async fn run_tracker_reannounce(self: Arc<Self>) -> Result<()> {
        // Default interval, may be overridden by tracker response
        let interval = if self.config.announce_interval > 0 {
            Duration::from_secs(self.config.announce_interval)
        } else {
            Duration::from_secs(1800) // 30 minutes default
        };

        let mut timer = tokio::time::interval(interval);
        // Skip first tick (we already announced on start)
        timer.tick().await;

        loop {
            timer.tick().await;

            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }

            let state = *self.state.read();
            if state == TorrentState::Stopped {
                break;
            }

            if state == TorrentState::Paused {
                continue;
            }

            // Re-announce to trackers
            if let Err(e) = self.announce_to_trackers(AnnounceEvent::None).await {
                tracing::debug!("Tracker re-announce failed: {}", e);
            }
        }

        Ok(())
    }

    /// Process a PEX (Peer Exchange) message from a peer.
    ///
    /// Returns new peers discovered from the message.
    pub fn process_pex_message(&self, payload: &[u8]) -> Vec<SocketAddr> {
        if !self.pex_enabled() {
            return vec![];
        }

        match PexMessage::parse(payload) {
            Ok(msg) => {
                let new_peers: Vec<SocketAddr> = {
                    let known = self.known_peers.read();
                    msg.all_added()
                        .into_iter()
                        .filter(|addr| !known.contains(addr))
                        .collect()
                };

                if !new_peers.is_empty() {
                    tracing::debug!(
                        "PEX received {} new peers for {}",
                        new_peers.len(),
                        self.name()
                    );
                    self.add_known_peers(new_peers.iter().cloned());
                }

                new_peers
            }
            Err(e) => {
                tracing::debug!("Failed to parse PEX message: {}", e);
                vec![]
            }
        }
    }

    /// Get the peer ID used for tracker announcements.
    pub fn peer_id(&self) -> [u8; 20] {
        *self.tracker_client.peer_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_torrent_config_default() {
        let config = TorrentConfig::default();
        assert_eq!(config.max_peers, 50);
        assert_eq!(config.listen_port_range, (6881, 6889));
        assert!(config.enable_dht);
    }

    #[test]
    fn test_torrent_state() {
        assert_ne!(TorrentState::Downloading, TorrentState::Seeding);
        assert_eq!(TorrentState::Paused, TorrentState::Paused);
    }

    #[test]
    fn test_magnet_preferences_are_retained_until_metadata_arrives() {
        let magnet =
            MagnetUri::parse("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567")
                .expect("valid magnet");
        let (event_tx, _) = broadcast::channel(4);
        let downloader = TorrentDownloader::from_magnet(
            DownloadId::new(),
            magnet,
            PathBuf::from("."),
            TorrentConfig::default(),
            event_tx,
        )
        .expect("downloader");

        downloader.set_selected_files(Some(&[1, 3]));
        downloader.set_sequential(true);

        assert_eq!(
            downloader.pending_selected_files.read().clone(),
            Some(vec![1, 3])
        );
        assert_eq!(*downloader.pending_sequential.read(), Some(true));
    }
}
