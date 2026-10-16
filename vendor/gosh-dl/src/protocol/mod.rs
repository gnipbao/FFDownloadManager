//! Protocol types for gosh-dl
//!
//! This module contains all types that cross the engine boundary:
//! - Events emitted by the engine
//! - Status and progress information
//! - Options for adding downloads
//! - Protocol-level errors
//!
//! These types are designed for serialization and can be used for IPC,
//! RPC, or any message-passing interface.

mod checksum;
mod error;
mod events;
mod options;
#[cfg(feature = "recursive-http")]
mod recursive;
mod scheduler;
mod status;
mod torrent;
mod types;

// Re-export all protocol types
pub use checksum::{ChecksumAlgorithm, ExpectedChecksum};
pub use error::{ProtocolError, ProtocolResult};
pub use events::DownloadEvent;
pub use options::{DownloadOptions, DownloadPriority};
#[cfg(feature = "recursive-http")]
pub use recursive::{
    RecursiveEntry, RecursiveJob, RecursiveJobEvent, RecursiveJobProgress, RecursiveJobState,
    RecursiveJobStatus, RecursiveManifest, RecursiveOptions, TrackedRecursiveJob,
};
pub use scheduler::{BandwidthLimits, ScheduleRule};
pub use status::{DownloadMetadata, DownloadStatus, GlobalStats};
pub use torrent::{PeerInfo, TorrentFile, TorrentInfo, TorrentStatusInfo};
pub use types::{DownloadId, DownloadKind, DownloadProgress, DownloadState};
