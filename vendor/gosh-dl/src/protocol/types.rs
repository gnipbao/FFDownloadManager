//! Core protocol types
//!
//! Fundamental types used throughout the protocol.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a download
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DownloadId(Uuid);

impl DownloadId {
    /// Create a new random download ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Create from an existing UUID
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// Get the underlying UUID
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }

    /// Generate an aria2-compatible GID (16-char hex string).
    ///
    /// This is a **lossy** projection: only the first 8 bytes of the 16-byte
    /// UUID are encoded, so the upper 8 bytes are discarded. Two distinct
    /// `DownloadId` values that share the same first 8 bytes will produce
    /// the same GID.
    ///
    /// Use [`matches_gid`](Self::matches_gid) to check whether a given GID
    /// corresponds to this `DownloadId` without assuming a lossless round-trip.
    pub fn to_gid(&self) -> String {
        hex::encode(&self.0.as_bytes()[0..8])
    }

    /// Create a **new** `DownloadId` from an aria2-compatible GID string.
    ///
    /// Because a GID only carries 8 bytes of the original 16-byte UUID, the
    /// returned `DownloadId` is **not** equal to whichever `DownloadId`
    /// originally produced the GID (the upper 8 bytes are zero-filled).
    /// In other words, `DownloadId::from_gid(id.to_gid()) != id` in the
    /// general case.
    ///
    /// If you need to test whether an existing `DownloadId` matches a GID,
    /// use [`matches_gid`](Self::matches_gid) instead.
    pub fn from_gid(gid: &str) -> Option<Self> {
        if gid.len() != 16 {
            return None;
        }
        let bytes = hex::decode(gid).ok()?;
        if bytes.len() != 8 {
            return None;
        }
        // Create a UUID with the GID bytes + zeros for the upper half
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes[0..8].copy_from_slice(&bytes);
        Some(Self(Uuid::from_bytes(uuid_bytes)))
    }

    /// Check whether this `DownloadId`'s first 8 bytes match the given GID.
    ///
    /// This is the correct way to look up a `DownloadId` by GID without
    /// relying on the lossy `from_gid` round-trip. It compares only the
    /// bytes that `to_gid` encodes, so it works even when the upper 8 bytes
    /// of two `DownloadId` values differ.
    pub fn matches_gid(&self, gid: &str) -> bool {
        self.to_gid() == gid
    }
}

impl Default for DownloadId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DownloadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_gid())
    }
}

/// Type of download
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadKind {
    /// HTTP/HTTPS download
    Http,
    /// BitTorrent download from .torrent file
    Torrent,
    /// BitTorrent download from magnet URI
    Magnet,
}

/// Current state of a download
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum DownloadState {
    /// Waiting in queue
    Queued,
    /// Connecting to server/peers
    Connecting,
    /// Actively downloading
    Downloading,
    /// Seeding (torrent only)
    Seeding,
    /// Paused by user
    Paused,
    /// Successfully completed
    Completed,
    /// Failed with error
    Error {
        kind: String,
        message: String,
        retryable: bool,
    },
}

impl DownloadState {
    /// Check if download is active (downloading or seeding)
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Downloading | Self::Seeding | Self::Connecting)
    }

    /// Check if download is finished (completed or error)
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Completed | Self::Error { .. })
    }

    /// Convert to aria2-compatible status string
    pub fn to_aria2_status(&self) -> &'static str {
        match self {
            Self::Queued => "waiting",
            Self::Connecting => "active",
            Self::Downloading => "active",
            Self::Seeding => "active",
            Self::Paused => "paused",
            Self::Completed => "complete",
            Self::Error { .. } => "error",
        }
    }
}

/// Progress information for a download
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloadProgress {
    /// Total size in bytes (may be unknown initially)
    pub total_size: Option<u64>,
    /// Bytes downloaded so far
    pub completed_size: u64,
    /// Current download speed in bytes/sec
    pub download_speed: u64,
    /// Current upload speed in bytes/sec (torrent only)
    pub upload_speed: u64,
    /// Number of active connections
    pub connections: u32,
    /// Number of seeders (torrent only)
    pub seeders: u32,
    /// Number of peers (torrent only)
    pub peers: u32,
    /// Estimated time remaining in seconds
    pub eta_seconds: Option<u64>,
}

impl DownloadProgress {
    /// Calculate progress percentage (0.0 - 100.0)
    pub fn percentage(&self) -> f64 {
        match self.total_size {
            Some(total) if total > 0 => (self.completed_size as f64 / total as f64) * 100.0,
            _ => 0.0,
        }
    }
}

// Helper for hex encoding (used by DownloadId)
mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, ()> {
        // Reject non-ASCII up front: byte-indexed slicing below panics on
        // multi-byte UTF-8, and GIDs arrive from untrusted callers.
        if s.len() % 2 != 0 || !s.is_ascii() {
            return Err(());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_gid_returns_16_char_hex_string() {
        let id = DownloadId::new();
        let gid = id.to_gid();
        assert_eq!(gid.len(), 16);
        assert!(gid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn from_gid_round_trip_is_lossy() {
        let id = DownloadId::new();
        let gid = id.to_gid();
        let reconstructed = DownloadId::from_gid(&gid).expect("valid GID");
        assert_ne!(
            id, reconstructed,
            "round-trip should be lossy (upper 8 bytes zeroed)"
        );
        assert_eq!(gid, reconstructed.to_gid());
    }

    #[test]
    fn matches_gid_works_without_round_trip() {
        let id = DownloadId::new();
        let gid = id.to_gid();
        assert!(id.matches_gid(&gid));
        let other = DownloadId::new();
        assert!(!other.matches_gid(&gid));
    }

    #[test]
    fn from_gid_rejects_invalid_input() {
        assert!(DownloadId::from_gid("").is_none());
        assert!(DownloadId::from_gid("abc").is_none());
        assert!(DownloadId::from_gid("0123456789abcde").is_none());
        assert!(DownloadId::from_gid("0123456789abcdef0").is_none());
        assert!(DownloadId::from_gid("zzzzzzzzzzzzzzzz").is_none());
    }

    #[test]
    fn from_gid_accepts_valid_input() {
        let id = DownloadId::from_gid("0123456789abcdef");
        assert!(id.is_some());
        assert_eq!(id.unwrap().to_gid(), "0123456789abcdef");
    }

    #[test]
    fn matches_gid_rejects_wrong_length() {
        let id = DownloadId::new();
        assert!(!id.matches_gid("abc"));
        assert!(!id.matches_gid(""));
    }
}
