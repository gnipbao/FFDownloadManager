//! Torrent-specific protocol types
//!
//! Types for representing torrent metadata and peer information.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Information about a file in a torrent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFile {
    /// File index
    pub index: usize,
    /// File path within torrent
    pub path: PathBuf,
    /// File size in bytes
    pub size: u64,
    /// Whether this file is selected for download
    pub selected: bool,
    /// Bytes completed for this file
    pub completed: u64,
}

/// Parsed torrent metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentInfo {
    /// Info hash (hex string)
    pub info_hash: String,
    /// Torrent name
    pub name: String,
    /// Total size in bytes
    pub total_size: u64,
    /// Piece length
    pub piece_length: u64,
    /// Number of pieces
    pub num_pieces: usize,
    /// Files in the torrent
    pub files: Vec<TorrentFile>,
    /// Announce URL
    pub announce: Option<String>,
    /// Announce list
    pub announce_list: Vec<Vec<String>>,
    /// Creation date
    pub creation_date: Option<DateTime<Utc>>,
    /// Comment
    pub comment: Option<String>,
    /// Created by
    pub created_by: Option<String>,
}

/// Torrent status information embedded in DownloadStatus
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentStatusInfo {
    /// Files in the torrent
    pub files: Vec<TorrentFile>,
    /// Piece length
    pub piece_length: u64,
    /// Number of pieces
    pub pieces_count: usize,
    /// Is private torrent
    pub private: bool,
}

/// Peer information (for torrents)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    /// Peer ID (if known)
    pub id: Option<String>,
    /// IP address
    pub ip: String,
    /// Port
    pub port: u16,
    /// Client name (if known)
    pub client: Option<String>,
    /// Download speed from this peer
    pub download_speed: u64,
    /// Upload speed to this peer
    pub upload_speed: u64,
    /// Progress of peer (0.0 - 1.0)
    pub progress: f64,
    /// Whether we're choking them
    pub am_choking: bool,
    /// Whether they're choking us
    pub peer_choking: bool,
}
