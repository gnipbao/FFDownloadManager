//! Download status types
//!
//! Types for representing the current state of downloads.

use super::checksum::ExpectedChecksum;
use super::options::DownloadPriority;
use super::torrent::{PeerInfo, TorrentStatusInfo};
use super::types::{DownloadId, DownloadKind, DownloadProgress, DownloadState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Metadata about a download
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadMetadata {
    /// Display name
    pub name: String,
    /// Original URL (for HTTP)
    pub url: Option<String>,
    /// Magnet URI (for magnet links)
    pub magnet_uri: Option<String>,
    /// Info hash (for torrents)
    pub info_hash: Option<String>,
    /// Save directory
    pub save_dir: PathBuf,
    /// Output filename (may differ from name for multi-file torrents)
    pub filename: Option<String>,
    /// Custom user agent
    pub user_agent: Option<String>,
    /// Custom referer
    pub referer: Option<String>,
    /// Custom headers
    pub headers: Vec<(String, String)>,
    /// Cookies for authenticated downloads
    #[serde(default)]
    pub cookies: Vec<String>,
    /// Expected checksum for verification (HTTP only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<ExpectedChecksum>,
    /// Mirror/fallback URLs (HTTP only)
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// ETag for resume validation (HTTP only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Last-Modified for resume validation (HTTP only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

/// Full status of a download
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadStatus {
    /// Unique identifier
    pub id: DownloadId,
    /// Type of download
    pub kind: DownloadKind,
    /// Current state
    pub state: DownloadState,
    /// Download priority
    #[serde(default)]
    pub priority: DownloadPriority,
    /// Progress information
    pub progress: DownloadProgress,
    /// Metadata
    pub metadata: DownloadMetadata,
    /// Torrent-specific info (only for torrent/magnet downloads)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torrent_info: Option<TorrentStatusInfo>,
    /// Connected peers (only for torrent downloads)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers: Option<Vec<PeerInfo>>,
    /// When the download was created
    pub created_at: DateTime<Utc>,
    /// When the download completed (if completed)
    pub completed_at: Option<DateTime<Utc>>,
}

impl DownloadStatus {
    /// Get GID for frontend compatibility
    pub fn gid(&self) -> String {
        self.id.to_gid()
    }
}

/// Global statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalStats {
    /// Total download speed
    pub download_speed: u64,
    /// Total upload speed
    pub upload_speed: u64,
    /// Number of active downloads
    pub num_active: usize,
    /// Number of waiting downloads
    pub num_waiting: usize,
    /// Number of stopped downloads
    pub num_stopped: usize,
}
