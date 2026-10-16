//! Storage Module
//!
//! This module handles persistent storage for download state and session data.
//! Three implementations are provided:
//!
//! - [`SqliteStorage`] (behind the `storage` feature): SQLite with WAL mode
//!   for crash-safe atomic commits, used automatically by
//!   `DownloadEngine::new` when `EngineConfig::database_path` is set.
//! - [`FileStorage`]: JSON sidecar files, one per download, similar to
//!   aria2's `.aria2` control files.
//! - [`MemoryStorage`]: in-memory, for tests and storage-less pause/resume.
//!
//! Applications with their own metadata store can implement the [`Storage`]
//! trait and pass it to `DownloadEngine::with_storage`.

pub(crate) mod file;
#[cfg(feature = "storage")]
pub(crate) mod sqlite;

pub use file::FileStorage;
#[cfg(feature = "storage")]
pub use sqlite::SqliteStorage;

use crate::error::Result;
#[cfg(feature = "recursive-http")]
use crate::types::TrackedRecursiveJob;
use crate::types::{DownloadId, DownloadStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Re-exported so custom `Storage` implementations don't need to depend on
// the `async-trait` crate themselves.
pub use async_trait::async_trait;

/// Segment state for HTTP multi-connection downloads
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentState {
    /// Segment is waiting to be downloaded
    Pending,
    /// Segment is currently being downloaded
    Downloading,
    /// Segment completed successfully
    Completed,
    /// Segment failed and may be retried
    Failed { error: String, retries: u32 },
}

/// Represents a download segment for multi-connection HTTP downloads
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    /// Segment index (0-based)
    pub index: usize,
    /// Start byte offset (inclusive)
    pub start: u64,
    /// End byte offset (inclusive)
    pub end: u64,
    /// Bytes downloaded for this segment
    pub downloaded: u64,
    /// Current state
    pub state: SegmentState,
}

impl Segment {
    /// Create a new pending segment
    pub fn new(index: usize, start: u64, end: u64) -> Self {
        Self {
            index,
            start,
            end,
            downloaded: 0,
            state: SegmentState::Pending,
        }
    }

    /// Get the total size of this segment
    pub fn size(&self) -> u64 {
        self.end - self.start + 1
    }

    /// Check if segment is complete
    pub fn is_complete(&self) -> bool {
        self.state == SegmentState::Completed
    }

    /// Get remaining bytes to download
    pub fn remaining(&self) -> u64 {
        self.size().saturating_sub(self.downloaded)
    }
}

/// Storage trait for persisting download state
///
/// Implementations of this trait handle storing and retrieving download
/// state to allow resume after crashes or restarts. Custom implementations
/// can be plugged into the engine via `DownloadEngine::with_storage`, e.g.
/// to keep download metadata in an application's own database.
///
/// Annotate implementations with the re-exported
/// [`#[async_trait]`](macro@async_trait) attribute. The runtime-metadata methods
/// and (under the `recursive-http` feature, which also adds `uuid` to the
/// trait surface) the recursive-job methods have no-op defaults; everything
/// else is required.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Save or update a download's status
    async fn save_download(&self, status: &DownloadStatus) -> Result<()>;

    /// Load a download by ID
    async fn load_download(&self, id: DownloadId) -> Result<Option<DownloadStatus>>;

    /// Load all downloads
    async fn load_all(&self) -> Result<Vec<DownloadStatus>>;

    /// Delete a download record
    async fn delete_download(&self, id: DownloadId) -> Result<()>;

    /// Save segment state for an HTTP download
    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> Result<()>;

    /// Load segment state for an HTTP download
    async fn load_segments(&self, id: DownloadId) -> Result<Vec<Segment>>;

    /// Delete segment state for a download
    async fn delete_segments(&self, id: DownloadId) -> Result<()>;

    /// Save raw torrent data (bencoded metainfo) for crash recovery
    async fn save_torrent_data(&self, id: DownloadId, data: &[u8]) -> Result<()>;

    /// Load raw torrent data for crash recovery
    async fn load_torrent_data(&self, id: DownloadId) -> Result<Option<Vec<u8>>>;

    /// Save engine-specific runtime metadata for a download.
    ///
    /// This is intended for resumable execution context that should not become
    /// part of the public `DownloadStatus` boundary.
    async fn save_runtime_metadata(&self, _id: DownloadId, _runtime_json: &str) -> Result<()> {
        Ok(())
    }

    /// Load engine-specific runtime metadata for a download.
    async fn load_runtime_metadata(&self, _id: DownloadId) -> Result<Option<String>> {
        Ok(None)
    }

    /// Load all persisted runtime metadata keyed by download ID.
    async fn load_all_runtime_metadata(&self) -> Result<HashMap<DownloadId, String>> {
        Ok(HashMap::new())
    }

    /// Save a tracked recursive job record.
    #[cfg(feature = "recursive-http")]
    async fn save_recursive_job(&self, _job: &TrackedRecursiveJob) -> Result<()> {
        Ok(())
    }

    /// Load all tracked recursive job records.
    #[cfg(feature = "recursive-http")]
    async fn load_recursive_jobs(&self) -> Result<Vec<TrackedRecursiveJob>> {
        Ok(Vec::new())
    }

    /// Delete a tracked recursive job record.
    #[cfg(feature = "recursive-http")]
    async fn delete_recursive_job(&self, _id: uuid::Uuid) -> Result<()> {
        Ok(())
    }

    /// Check if database is healthy
    async fn health_check(&self) -> Result<()>;

    /// Compact/vacuum the database
    async fn compact(&self) -> Result<()>;
}

/// In-memory storage for testing
#[derive(Debug, Default)]
pub struct MemoryStorage {
    downloads: parking_lot::RwLock<std::collections::HashMap<DownloadId, DownloadStatus>>,
    segments: parking_lot::RwLock<std::collections::HashMap<DownloadId, Vec<Segment>>>,
    torrent_data: parking_lot::RwLock<std::collections::HashMap<DownloadId, Vec<u8>>>,
    runtime_metadata: parking_lot::RwLock<HashMap<DownloadId, String>>,
    #[cfg(feature = "recursive-http")]
    recursive_jobs: parking_lot::RwLock<HashMap<uuid::Uuid, TrackedRecursiveJob>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn save_download(&self, status: &DownloadStatus) -> Result<()> {
        self.downloads.write().insert(status.id, status.clone());
        Ok(())
    }

    async fn load_download(&self, id: DownloadId) -> Result<Option<DownloadStatus>> {
        Ok(self.downloads.read().get(&id).cloned())
    }

    async fn load_all(&self) -> Result<Vec<DownloadStatus>> {
        Ok(self.downloads.read().values().cloned().collect())
    }

    async fn delete_download(&self, id: DownloadId) -> Result<()> {
        self.downloads.write().remove(&id);
        self.segments.write().remove(&id);
        self.torrent_data.write().remove(&id);
        self.runtime_metadata.write().remove(&id);
        Ok(())
    }

    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> Result<()> {
        self.segments.write().insert(id, segments.to_vec());
        Ok(())
    }

    async fn load_segments(&self, id: DownloadId) -> Result<Vec<Segment>> {
        Ok(self.segments.read().get(&id).cloned().unwrap_or_default())
    }

    async fn delete_segments(&self, id: DownloadId) -> Result<()> {
        self.segments.write().remove(&id);
        Ok(())
    }

    async fn save_torrent_data(&self, id: DownloadId, data: &[u8]) -> Result<()> {
        self.torrent_data.write().insert(id, data.to_vec());
        Ok(())
    }

    async fn load_torrent_data(&self, id: DownloadId) -> Result<Option<Vec<u8>>> {
        Ok(self.torrent_data.read().get(&id).cloned())
    }

    async fn save_runtime_metadata(&self, id: DownloadId, runtime_json: &str) -> Result<()> {
        self.runtime_metadata
            .write()
            .insert(id, runtime_json.to_string());
        Ok(())
    }

    async fn load_runtime_metadata(&self, id: DownloadId) -> Result<Option<String>> {
        Ok(self.runtime_metadata.read().get(&id).cloned())
    }

    async fn load_all_runtime_metadata(&self) -> Result<HashMap<DownloadId, String>> {
        Ok(self.runtime_metadata.read().clone())
    }

    #[cfg(feature = "recursive-http")]
    async fn save_recursive_job(&self, job: &TrackedRecursiveJob) -> Result<()> {
        self.recursive_jobs.write().insert(job.id, job.clone());
        Ok(())
    }

    #[cfg(feature = "recursive-http")]
    async fn load_recursive_jobs(&self) -> Result<Vec<TrackedRecursiveJob>> {
        Ok(self.recursive_jobs.read().values().cloned().collect())
    }

    #[cfg(feature = "recursive-http")]
    async fn delete_recursive_job(&self, id: uuid::Uuid) -> Result<()> {
        self.recursive_jobs.write().remove(&id);
        Ok(())
    }

    async fn health_check(&self) -> Result<()> {
        Ok(())
    }

    async fn compact(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DownloadKind, DownloadMetadata, DownloadProgress, DownloadState};
    use chrono::Utc;
    use std::path::PathBuf;
    #[cfg(feature = "recursive-http")]
    use uuid::Uuid;

    fn create_test_status() -> DownloadStatus {
        DownloadStatus {
            id: DownloadId::new(),
            kind: DownloadKind::Http,
            state: DownloadState::Downloading,
            priority: crate::priority_queue::DownloadPriority::Normal,
            progress: DownloadProgress::default(),
            metadata: DownloadMetadata {
                name: "test.zip".to_string(),
                url: Some("https://example.com/test.zip".to_string()),
                magnet_uri: None,
                info_hash: None,
                save_dir: PathBuf::from("/tmp"),
                filename: Some("test.zip".to_string()),
                user_agent: None,
                referer: None,
                headers: vec![],
                cookies: Vec::new(),
                checksum: None,
                mirrors: Vec::new(),
                etag: None,
                last_modified: None,
            },
            torrent_info: None,
            peers: None,
            created_at: Utc::now(),
            completed_at: None,
        }
    }

    #[tokio::test]
    async fn test_memory_storage() {
        let storage = MemoryStorage::new();
        let status = create_test_status();
        let id = status.id;

        // Save
        storage.save_download(&status).await.unwrap();

        // Load
        let loaded = storage.load_download(id).await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id, id);

        // Load all
        let all = storage.load_all().await.unwrap();
        assert_eq!(all.len(), 1);

        // Delete
        storage.delete_download(id).await.unwrap();
        let loaded = storage.load_download(id).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_segment_storage() {
        let storage = MemoryStorage::new();
        let id = DownloadId::new();

        let segments = vec![
            Segment::new(0, 0, 999),
            Segment::new(1, 1000, 1999),
            Segment::new(2, 2000, 2999),
        ];

        // Save segments
        storage.save_segments(id, &segments).await.unwrap();

        // Load segments
        let loaded = storage.load_segments(id).await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].start, 0);
        assert_eq!(loaded[1].start, 1000);
        assert_eq!(loaded[2].start, 2000);

        // Delete segments
        storage.delete_segments(id).await.unwrap();
        let loaded = storage.load_segments(id).await.unwrap();
        assert!(loaded.is_empty());
    }

    #[tokio::test]
    async fn test_delete_download_removes_all_associated_data() {
        let storage = MemoryStorage::new();
        let status = create_test_status();
        let id = status.id;

        storage.save_download(&status).await.unwrap();
        storage
            .save_segments(id, &[Segment::new(0, 0, 999)])
            .await
            .unwrap();
        storage
            .save_torrent_data(id, b"torrent-bytes")
            .await
            .unwrap();
        storage
            .save_runtime_metadata(id, r#"{"key":"value"}"#)
            .await
            .unwrap();

        storage.delete_download(id).await.unwrap();

        assert!(storage.load_download(id).await.unwrap().is_none());
        assert!(storage.load_segments(id).await.unwrap().is_empty());
        assert!(storage.load_torrent_data(id).await.unwrap().is_none());
        assert!(storage.load_runtime_metadata(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_runtime_metadata_storage() {
        let storage = MemoryStorage::new();
        let status = create_test_status();
        let id = status.id;

        storage.save_download(&status).await.unwrap();
        storage
            .save_runtime_metadata(id, r#"{"recursive_child":{"fail_fast":true}}"#)
            .await
            .unwrap();

        assert_eq!(
            storage.load_runtime_metadata(id).await.unwrap().as_deref(),
            Some(r#"{"recursive_child":{"fail_fast":true}}"#)
        );
        assert!(storage
            .load_all_runtime_metadata()
            .await
            .unwrap()
            .contains_key(&id));

        storage.delete_download(id).await.unwrap();
        assert!(storage.load_runtime_metadata(id).await.unwrap().is_none());
    }

    #[cfg(feature = "recursive-http")]
    #[tokio::test]
    async fn test_recursive_job_storage() {
        let storage = MemoryStorage::new();
        let job = crate::types::TrackedRecursiveJob {
            id: Uuid::new_v4(),
            root_url: "https://example.com/pub/".to_string(),
            child_ids: vec![DownloadId::new(), DownloadId::new()],
            created_at: Utc::now(),
        };

        storage.save_recursive_job(&job).await.unwrap();

        let jobs = storage.load_recursive_jobs().await.unwrap();
        assert_eq!(jobs, vec![job.clone()]);

        storage.delete_recursive_job(job.id).await.unwrap();
        assert!(storage.load_recursive_jobs().await.unwrap().is_empty());
    }

    #[test]
    fn test_segment_size() {
        let segment = Segment::new(0, 0, 999);
        assert_eq!(segment.size(), 1000);

        let segment = Segment::new(1, 1000, 1999);
        assert_eq!(segment.size(), 1000);
    }
}
