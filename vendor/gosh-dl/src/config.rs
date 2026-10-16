//! Engine configuration
//!
//! This module contains all configuration options for the download engine.

use crate::error::{EngineError, Result};
use crate::scheduler::ScheduleRule;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Main configuration for the download engine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Directory to save downloads
    pub download_dir: PathBuf,

    /// Maximum concurrent downloads
    pub max_concurrent_downloads: usize,

    /// Maximum connections per download (for segmented HTTP)
    pub max_connections_per_download: usize,

    /// Minimum segment size in bytes (won't split smaller than this)
    pub min_segment_size: u64,

    /// Global download speed limit (bytes/sec, None = unlimited)
    pub global_download_limit: Option<u64>,

    /// Global upload speed limit (bytes/sec, None = unlimited)
    pub global_upload_limit: Option<u64>,

    /// Bandwidth schedule rules for time-based limits
    /// Rules are evaluated in order, first match wins
    #[serde(default)]
    pub schedule_rules: Vec<ScheduleRule>,

    /// Default user agent
    pub user_agent: String,

    /// Enable DHT for torrents
    pub enable_dht: bool,

    /// Enable PEX (Peer Exchange) for torrents
    pub enable_pex: bool,

    /// Enable LPD (Local Peer Discovery) for torrents
    pub enable_lpd: bool,

    /// Maximum peers per torrent
    pub max_peers: usize,

    /// Stop seeding when this ratio is reached
    pub seed_ratio: f64,

    /// Database path for session persistence
    pub database_path: Option<PathBuf>,

    /// HTTP configuration
    pub http: HttpConfig,

    /// BitTorrent configuration
    pub torrent: TorrentConfig,
}

/// HTTP-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    /// Connection timeout in seconds
    pub connect_timeout: u64,

    /// Per-read idle timeout in seconds — if no data is received for this duration,
    /// the request is cancelled. Resets after each successful read, so large downloads
    /// are not affected as long as data keeps flowing.
    pub read_timeout: u64,

    /// Maximum redirects to follow
    pub max_redirects: usize,

    /// Retry attempts for failed segments
    pub max_retries: usize,

    /// Initial retry delay in milliseconds
    pub retry_delay_ms: u64,

    /// Maximum retry delay in milliseconds
    pub max_retry_delay_ms: u64,

    /// Whether to accept invalid TLS certificates (dangerous!)
    pub accept_invalid_certs: bool,

    /// Proxy URL (e.g., "http://proxy:8080" or "socks5://proxy:1080")
    /// Supports HTTP, HTTPS, and SOCKS5 proxies
    pub proxy_url: Option<String>,
}

/// File allocation mode for torrent downloads
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AllocationMode {
    /// No preallocation (default) - files grow as data is written
    #[default]
    None,
    /// Sparse allocation - set file size but don't write zeros (fast, most filesystems)
    Sparse,
    /// Full allocation - preallocate entire file with zeros (slow but prevents fragmentation)
    Full,
}

impl std::fmt::Display for AllocationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Sparse => write!(f, "sparse"),
            Self::Full => write!(f, "full"),
        }
    }
}

impl std::str::FromStr for AllocationMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "none" => Ok(Self::None),
            "sparse" => Ok(Self::Sparse),
            "full" | "preallocate" => Ok(Self::Full),
            _ => Err(format!("Invalid allocation mode: {}", s)),
        }
    }
}

/// BitTorrent-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentConfig {
    /// Port range for incoming connections
    pub listen_port_range: (u16, u16),

    /// DHT bootstrap nodes
    pub dht_bootstrap_nodes: Vec<String>,

    /// File allocation mode (none, sparse, or full)
    #[serde(default)]
    pub allocation_mode: AllocationMode,

    /// Tracker update interval in seconds
    pub tracker_update_interval: u64,

    /// Peer request timeout in seconds
    pub peer_timeout: u64,

    /// Maximum outstanding piece requests per peer
    pub max_pending_requests: usize,

    /// Enable endgame mode
    pub enable_endgame: bool,

    /// Peer loop tick interval in milliseconds.
    /// Controls how frequently the peer loop checks for state changes and cleanup.
    /// Default: 100ms. Lower values increase responsiveness but use more CPU.
    #[serde(default = "default_tick_interval_ms")]
    pub tick_interval_ms: u64,

    /// Peer connection attempt interval in seconds.
    /// Controls how frequently we attempt to connect to new peers.
    /// Default: 5 seconds.
    #[serde(default = "default_connect_interval_secs")]
    pub connect_interval_secs: u64,

    /// Choking algorithm update interval in seconds.
    /// Controls how frequently we recalculate which peers to unchoke.
    /// Per BEP 3, this should be around 10 seconds for regular unchoke
    /// and 30 seconds for optimistic unchoke.
    /// Default: 10 seconds.
    #[serde(default = "default_choking_interval_secs")]
    pub choking_interval_secs: u64,

    /// WebSeed configuration
    #[serde(default)]
    pub webseed: WebSeedConfig,

    /// Encryption configuration (MSE/PE)
    #[serde(default)]
    pub encryption: EncryptionConfig,

    /// uTP transport configuration
    #[serde(default)]
    pub utp: UtpConfigSettings,
}

/// WebSeed-specific configuration (BEP 19/BEP 17)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSeedConfig {
    /// Enable web seed downloads
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Maximum concurrent web seed connections per torrent
    #[serde(default = "default_webseed_connections")]
    pub max_connections: usize,

    /// Request timeout in seconds
    #[serde(default = "default_webseed_timeout")]
    pub timeout_seconds: u64,

    /// Maximum consecutive failures before disabling a web seed
    #[serde(default = "default_webseed_max_failures")]
    pub max_failures: u32,
}

fn default_true() -> bool {
    true
}

fn default_webseed_connections() -> usize {
    4
}

fn default_webseed_timeout() -> u64 {
    30
}

fn default_webseed_max_failures() -> u32 {
    5
}

fn default_tick_interval_ms() -> u64 {
    100
}

fn default_connect_interval_secs() -> u64 {
    5
}

fn default_choking_interval_secs() -> u64 {
    10
}

impl Default for WebSeedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_connections: 4,
            timeout_seconds: 30,
            max_failures: 5,
        }
    }
}

/// Encryption policy for peer connections (MSE/PE)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EncryptionPolicy {
    /// Disable encryption entirely (plaintext only)
    Disabled,
    /// Allow encryption but don't require it (accept both)
    Allowed,
    /// Prefer encryption, fall back to plaintext if peer doesn't support
    #[default]
    Preferred,
    /// Require encryption (reject non-MSE peers)
    Required,
}

impl std::fmt::Display for EncryptionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled"),
            Self::Allowed => write!(f, "allowed"),
            Self::Preferred => write!(f, "preferred"),
            Self::Required => write!(f, "required"),
        }
    }
}

/// Encryption configuration for peer connections (MSE/PE)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionConfig {
    /// Encryption policy
    #[serde(default)]
    pub policy: EncryptionPolicy,

    /// Allow plaintext as fallback (when policy is Preferred)
    #[serde(default = "default_true")]
    pub allow_plaintext: bool,

    /// Allow RC4 encryption
    #[serde(default = "default_true")]
    pub allow_rc4: bool,

    /// Minimum random padding bytes for obfuscation
    #[serde(default)]
    pub min_padding: usize,

    /// Maximum random padding bytes for obfuscation
    #[serde(default = "default_max_padding")]
    pub max_padding: usize,
}

fn default_max_padding() -> usize {
    512
}

impl Default for EncryptionConfig {
    fn default() -> Self {
        Self {
            policy: EncryptionPolicy::Preferred,
            allow_plaintext: true,
            allow_rc4: true,
            min_padding: 0,
            max_padding: 512,
        }
    }
}

/// Transport policy for peer connections
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TransportPolicy {
    /// Use TCP only
    TcpOnly,
    /// Use uTP only
    UtpOnly,
    /// Prefer uTP, fall back to TCP (default)
    #[default]
    PreferUtp,
    /// Prefer TCP, fall back to uTP
    PreferTcp,
}

impl std::fmt::Display for TransportPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TcpOnly => write!(f, "tcp-only"),
            Self::UtpOnly => write!(f, "utp-only"),
            Self::PreferUtp => write!(f, "prefer-utp"),
            Self::PreferTcp => write!(f, "prefer-tcp"),
        }
    }
}

/// uTP (Micro Transport Protocol) configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtpConfigSettings {
    /// Enable uTP transport.
    ///
    /// The uTP stack is experimental and currently non-functional for real
    /// data transfer; a config file that omits this key gets the same `false`
    /// default as `UtpConfigSettings::default()`.
    #[serde(default)]
    pub enabled: bool,

    /// Transport policy (prefer-utp, prefer-tcp, utp-only, tcp-only)
    #[serde(default)]
    pub policy: TransportPolicy,

    /// Enable TCP fallback when uTP fails
    #[serde(default = "default_true")]
    pub tcp_fallback: bool,

    /// Target delay in microseconds for LEDBAT (default: 100,000 = 100ms)
    #[serde(default = "default_target_delay")]
    pub target_delay_us: u32,

    /// Maximum congestion window size in bytes (default: 1MB)
    #[serde(default = "default_max_window")]
    pub max_window_size: u32,

    /// Initial receive window size in bytes (default: 1MB)
    #[serde(default = "default_recv_window")]
    pub recv_window: u32,

    /// Enable selective ACK extension
    #[serde(default = "default_true")]
    pub enable_sack: bool,
}

fn default_target_delay() -> u32 {
    100_000 // 100ms
}

fn default_max_window() -> u32 {
    1024 * 1024 // 1MB
}

fn default_recv_window() -> u32 {
    1024 * 1024 // 1MB
}

impl Default for UtpConfigSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            policy: TransportPolicy::PreferUtp,
            tcp_fallback: true,
            target_delay_us: 100_000,
            max_window_size: 1024 * 1024,
            recv_window: 1024 * 1024,
            enable_sack: true,
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            download_dir: dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")),
            max_concurrent_downloads: 5,
            max_connections_per_download: 16,
            min_segment_size: 1024 * 1024, // 1 MiB
            global_download_limit: None,
            global_upload_limit: None,
            schedule_rules: Vec::new(),
            user_agent: format!("gosh-dl/{}", env!("CARGO_PKG_VERSION")),
            enable_dht: true,
            enable_pex: true,
            enable_lpd: true,
            max_peers: 55,
            seed_ratio: 1.0,
            database_path: None,
            http: HttpConfig::default(),
            torrent: TorrentConfig::default(),
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: 30,
            read_timeout: 60,
            max_redirects: 10,
            max_retries: 5,
            retry_delay_ms: 1000,
            max_retry_delay_ms: 30000,
            accept_invalid_certs: false,
            proxy_url: None,
        }
    }
}

impl Default for TorrentConfig {
    fn default() -> Self {
        Self {
            listen_port_range: (6881, 6889),
            dht_bootstrap_nodes: vec![
                "router.bittorrent.com:6881".to_string(),
                "router.utorrent.com:6881".to_string(),
                "dht.transmissionbt.com:6881".to_string(),
            ],
            allocation_mode: AllocationMode::None,
            tracker_update_interval: 1800, // 30 minutes
            peer_timeout: 120,
            max_pending_requests: 16,
            enable_endgame: true,
            tick_interval_ms: 100,
            connect_interval_secs: 5,
            choking_interval_secs: 10,
            webseed: WebSeedConfig::default(),
            encryption: EncryptionConfig::default(),
            utp: UtpConfigSettings::default(),
        }
    }
}

impl EngineConfig {
    /// Create a new config with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the download directory
    pub fn download_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.download_dir = path.into();
        self
    }

    /// Set maximum concurrent downloads
    pub fn max_concurrent_downloads(mut self, max: usize) -> Self {
        self.max_concurrent_downloads = max;
        self
    }

    /// Set maximum connections per download
    pub fn max_connections_per_download(mut self, max: usize) -> Self {
        self.max_connections_per_download = max;
        self
    }

    /// Set global download speed limit
    pub fn download_limit(mut self, limit: Option<u64>) -> Self {
        self.global_download_limit = limit;
        self
    }

    /// Set global upload speed limit
    pub fn upload_limit(mut self, limit: Option<u64>) -> Self {
        self.global_upload_limit = limit;
        self
    }

    /// Set the user agent
    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.user_agent = ua.into();
        self
    }

    /// Set bandwidth schedule rules
    pub fn schedule_rules(mut self, rules: Vec<ScheduleRule>) -> Self {
        self.schedule_rules = rules;
        self
    }

    /// Add a bandwidth schedule rule
    pub fn add_schedule_rule(mut self, rule: ScheduleRule) -> Self {
        self.schedule_rules.push(rule);
        self
    }

    /// Set the database path for persistence
    pub fn database_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.database_path = Some(path.into());
        self
    }

    /// Validate the configuration
    pub fn validate(&self) -> Result<()> {
        // Check download directory
        if !self.download_dir.exists() {
            return Err(EngineError::invalid_input(
                "download_dir",
                format!("Directory does not exist: {:?}", self.download_dir),
            ));
        }

        if !self.download_dir.is_dir() {
            return Err(EngineError::invalid_input(
                "download_dir",
                format!("Path is not a directory: {:?}", self.download_dir),
            ));
        }

        // Check numeric limits
        if self.max_concurrent_downloads == 0 {
            return Err(EngineError::invalid_input(
                "max_concurrent_downloads",
                "Must be at least 1",
            ));
        }

        if self.max_connections_per_download == 0 {
            return Err(EngineError::invalid_input(
                "max_connections_per_download",
                "Must be at least 1",
            ));
        }

        // Segment-count math divides by this; zero would panic on any
        // range-capable server.
        if self.min_segment_size == 0 {
            return Err(EngineError::invalid_input(
                "min_segment_size",
                "Must be at least 1 byte",
            ));
        }

        if self.seed_ratio < 0.0 {
            return Err(EngineError::invalid_input(
                "seed_ratio",
                "Must be non-negative",
            ));
        }

        // Check port range
        if self.torrent.listen_port_range.0 > self.torrent.listen_port_range.1 {
            return Err(EngineError::invalid_input(
                "listen_port_range",
                "Start port must be <= end port",
            ));
        }

        Ok(())
    }

    /// Get the database path, using default if not set
    pub fn get_database_path(&self) -> PathBuf {
        self.database_path.clone().unwrap_or_else(|| {
            dirs::data_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("gosh-dl")
                .join("gosh-dl.db")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_default_config() {
        let config = EngineConfig::default();
        assert_eq!(config.max_concurrent_downloads, 5);
        assert_eq!(config.max_connections_per_download, 16);
        assert!(config.enable_dht);
    }

    #[test]
    fn test_config_builder() {
        let config = EngineConfig::new()
            .max_concurrent_downloads(10)
            .max_connections_per_download(8)
            .download_limit(Some(1024 * 1024));

        assert_eq!(config.max_concurrent_downloads, 10);
        assert_eq!(config.max_connections_per_download, 8);
        assert_eq!(config.global_download_limit, Some(1024 * 1024));
    }

    #[test]
    fn test_config_validation() {
        let dir = tempdir().unwrap();
        let config = EngineConfig::new().download_dir(dir.path());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_download_dir() {
        let config = EngineConfig::new().download_dir("/nonexistent/path/12345");
        assert!(config.validate().is_err());
    }
}
