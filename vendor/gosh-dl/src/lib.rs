//! # gosh-dl
//!
//! A fast, safe, and reliable download engine written in Rust.
//!
//! ## Features
//!
//! - **HTTP/HTTPS Downloads**: Multi-connection segmented downloads with
//!   cross-restart resume validation and mirror segment striping
//! - **BitTorrent**: Full protocol support including DHT, PEX, LPD, inbound
//!   peer connections/seeding, and an opt-in uTP (BEP 29) transport
//! - **Streaming**: [`DownloadEngine::open_reader`] yields an `AsyncRead`
//!   over an in-progress download; torrent piece selection follows the reader
//! - **Rate limiting**: global and per-download byte-rate limits covering
//!   every transfer path
//! - **Verify & repair**: [`DownloadEngine::verify`] re-checks on-disk data;
//!   [`DownloadEngine::repair`] re-downloads what's bad
//! - **Metalink**: RFC 5854 `.meta4` documents expand into downloads with
//!   mirrors and checksums (feature `metalink`)
//! - **Cross-platform**: Works on Linux, macOS, and Windows
//! - **Memory-safe**: Written in Rust with no unsafe code in core paths
//! - **Async**: Built on Tokio for efficient concurrent downloads
//!
//! ## Quick Start
//!
//! ```rust,ignore
//! use gosh_dl::{DownloadEngine, EngineConfig, DownloadOptions};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create engine with default config
//!     let config = EngineConfig::default();
//!     let engine = DownloadEngine::new(config).await?;
//!
//!     // Add a download
//!     let id = engine.add_http(
//!         "https://example.com/file.zip",
//!         DownloadOptions::default(),
//!     ).await?;
//!
//!     // Subscribe to events
//!     let mut events = engine.subscribe();
//!     while let Ok(event) = events.recv().await {
//!         println!("Event: {:?}", event);
//!     }
//!
//!     Ok(())
//! }
//! ```

// Modules
pub mod config;
pub mod engine;
pub mod error;
#[cfg(feature = "http")]
pub mod http;
pub mod limiter;
#[cfg(feature = "metalink")]
pub mod metalink;
pub(crate) mod priority_queue;
pub mod protocol;
pub(crate) mod scheduler;
pub mod storage;
#[cfg(feature = "torrent")]
pub mod torrent;
pub(crate) mod types;

// Re-exports for convenience
pub use config::{AllocationMode, EngineConfig, HttpConfig, TorrentConfig};
pub use engine::{BatchResult, DownloadEngine, DownloadReader, VerifyReport};
pub use error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result, StorageErrorKind};
pub use protocol::{ProtocolError, ProtocolResult};
pub use types::{
    DownloadEvent, DownloadId, DownloadKind, DownloadMetadata, DownloadOptions, DownloadProgress,
    DownloadState, DownloadStatus, GlobalStats, PeerInfo, TorrentFile, TorrentInfo,
    TorrentStatusInfo,
};

#[cfg(feature = "recursive-http")]
pub use types::{
    RecursiveEntry, RecursiveJob, RecursiveJobEvent, RecursiveJobProgress, RecursiveJobState,
    RecursiveJobStatus, RecursiveManifest, RecursiveOptions, TrackedRecursiveJob,
};

// Metalink exports
#[cfg(feature = "metalink")]
pub use metalink::{parse_metalink, MetalinkFile};

// Storage exports
#[cfg(feature = "storage")]
pub use storage::SqliteStorage;
pub use storage::{FileStorage, MemoryStorage, Segment, SegmentState, Storage};

// Priority queue exports
pub use limiter::RateLimiter;
pub use priority_queue::{DownloadPriority, PriorityQueue, PriorityQueueStats};

// Scheduler exports
pub use scheduler::{BandwidthLimits, BandwidthScheduler, ScheduleRule};

// HTTP module exports
#[cfg(feature = "http")]
pub use http::{
    ConnectionPool, HttpDownloader, ResumeInfo, RetryPolicy, SegmentedDownload, ServerCapabilities,
    SpeedCalculator,
};
