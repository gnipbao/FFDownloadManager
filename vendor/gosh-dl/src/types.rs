//! Core types for gosh-dl
//!
//! This module re-exports all protocol types for backward compatibility.
//! New code should prefer importing from `crate::protocol` directly.

// Re-export all types from protocol module for backward compatibility
pub use crate::protocol::{
    // Scheduler types
    BandwidthLimits,
    // Checksum types
    ChecksumAlgorithm,
    // Events
    DownloadEvent,
    // Core types
    DownloadId,
    DownloadKind,
    // Status types
    DownloadMetadata,
    // Options
    DownloadOptions,
    DownloadPriority,
    DownloadProgress,
    DownloadState,
    DownloadStatus,
    ExpectedChecksum,
    GlobalStats,
    // Torrent types
    PeerInfo,
    ScheduleRule,
    TorrentFile,
    TorrentInfo,
    TorrentStatusInfo,
};

#[cfg(feature = "recursive-http")]
pub use crate::protocol::{
    RecursiveEntry, RecursiveJob, RecursiveJobEvent, RecursiveJobProgress, RecursiveJobState,
    RecursiveJobStatus, RecursiveManifest, RecursiveOptions, TrackedRecursiveJob,
};
