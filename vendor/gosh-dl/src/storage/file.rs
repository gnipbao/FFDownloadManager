//! File-based storage: JSON sidecar files, one per download.
//!
//! This is the moral equivalent of aria2's `.aria2` control files: each
//! download gets a `<id>.json` record holding its status, segment state and
//! runtime metadata (plus a raw `<id>.torrent` sidecar for torrent
//! metainfo), so downloads can resume across process restarts without a
//! database. All writes go through a temp-file-plus-rename so a crash never
//! leaves a half-written record.

use super::{Segment, Storage};
use crate::error::Result;
#[cfg(feature = "recursive-http")]
use crate::types::TrackedRecursiveJob;
use crate::types::{DownloadId, DownloadStatus};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// On-disk JSON record for a single download.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DownloadRecord {
    #[serde(default)]
    status: Option<DownloadStatus>,
    #[serde(default)]
    segments: Vec<Segment>,
    #[serde(default)]
    runtime_metadata: Option<String>,
}

/// File-based [`Storage`] implementation using one JSON sidecar per download.
///
/// ```no_run
/// use std::sync::Arc;
/// use gosh_dl::{DownloadEngine, EngineConfig, FileStorage};
///
/// # async fn example() -> gosh_dl::Result<()> {
/// let storage = Arc::new(FileStorage::new("/var/lib/myapp/downloads-state").await?);
/// let engine = DownloadEngine::with_storage(EngineConfig::default(), storage).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct FileStorage {
    dir: PathBuf,
    /// Serializes read-modify-write cycles on the record files.
    write_lock: tokio::sync::Mutex<()>,
}

const RECURSIVE_PREFIX: &str = "recursive-";

impl FileStorage {
    /// Create a file storage rooted at `dir`, creating the directory if needed.
    pub async fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        tokio::fs::create_dir_all(&dir).await?;
        Ok(Self {
            dir,
            write_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Directory holding the sidecar files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    // Filenames use the full UUID rather than `DownloadId`'s `Display`,
    // which is a lossy aria2-style GID that cannot be mapped back to an ID.
    fn record_path(&self, id: DownloadId) -> PathBuf {
        self.dir.join(format!("{}.json", id.as_uuid()))
    }

    fn torrent_path(&self, id: DownloadId) -> PathBuf {
        self.dir.join(format!("{}.torrent", id.as_uuid()))
    }

    #[cfg(feature = "recursive-http")]
    fn recursive_path(&self, id: uuid::Uuid) -> PathBuf {
        self.dir.join(format!("{RECURSIVE_PREFIX}{id}.json"))
    }

    async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
        match tokio::fs::read(path).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        Self::write_atomic(path, &bytes).await
    }

    async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut file = tokio::fs::File::create(&tmp).await?;
            tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
            // Flush data to disk before the rename: on power loss the rename
            // can survive while unsynced data doesn't, leaving a truncated
            // record at the final path.
            file.sync_all().await?;
        }
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }

    async fn remove_if_exists(path: &Path) -> Result<()> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn update_record<F>(&self, id: DownloadId, update: F) -> Result<()>
    where
        F: FnOnce(&mut DownloadRecord),
    {
        let _guard = self.write_lock.lock().await;
        let path = self.record_path(id);
        let mut record: DownloadRecord = Self::read_json(&path).await?.unwrap_or_default();
        update(&mut record);
        Self::write_json(&path, &record).await
    }

    /// Iterate the IDs of all download record files in the directory.
    ///
    /// IDs are recovered from the `<uuid>.json` filenames so that records
    /// whose status hasn't been written yet are still visible.
    async fn record_ids(&self) -> Result<Vec<DownloadId>> {
        let mut ids = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.starts_with(RECURSIVE_PREFIX) {
                continue;
            }
            if let Ok(uuid) = uuid::Uuid::parse_str(stem) {
                ids.push(DownloadId::from_uuid(uuid));
            }
        }
        Ok(ids)
    }
}

#[async_trait]
impl Storage for FileStorage {
    async fn save_download(&self, status: &DownloadStatus) -> Result<()> {
        let status = status.clone();
        self.update_record(status.id, move |record| record.status = Some(status))
            .await
    }

    async fn load_download(&self, id: DownloadId) -> Result<Option<DownloadStatus>> {
        let record: Option<DownloadRecord> = Self::read_json(&self.record_path(id)).await?;
        Ok(record.and_then(|r| r.status))
    }

    async fn load_all(&self) -> Result<Vec<DownloadStatus>> {
        let mut statuses = Vec::new();
        for id in self.record_ids().await? {
            match Self::read_json::<DownloadRecord>(&self.record_path(id)).await {
                Ok(Some(record)) => {
                    if let Some(status) = record.status {
                        statuses.push(status);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("Skipping unreadable download record for {}: {}", id, e);
                }
            }
        }
        Ok(statuses)
    }

    async fn delete_download(&self, id: DownloadId) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        Self::remove_if_exists(&self.record_path(id)).await?;
        Self::remove_if_exists(&self.torrent_path(id)).await?;
        Ok(())
    }

    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> Result<()> {
        let segments = segments.to_vec();
        self.update_record(id, move |record| record.segments = segments)
            .await
    }

    async fn load_segments(&self, id: DownloadId) -> Result<Vec<Segment>> {
        let record: Option<DownloadRecord> = Self::read_json(&self.record_path(id)).await?;
        Ok(record.map(|r| r.segments).unwrap_or_default())
    }

    async fn delete_segments(&self, id: DownloadId) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        let path = self.record_path(id);
        if let Some(mut record) = Self::read_json::<DownloadRecord>(&path).await? {
            record.segments.clear();
            Self::write_json(&path, &record).await?;
        }
        Ok(())
    }

    async fn save_torrent_data(&self, id: DownloadId, data: &[u8]) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        Self::write_atomic(&self.torrent_path(id), data).await
    }

    async fn load_torrent_data(&self, id: DownloadId) -> Result<Option<Vec<u8>>> {
        match tokio::fs::read(self.torrent_path(id)).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn save_runtime_metadata(&self, id: DownloadId, runtime_json: &str) -> Result<()> {
        let runtime_json = runtime_json.to_string();
        self.update_record(id, move |record| {
            record.runtime_metadata = Some(runtime_json)
        })
        .await
    }

    async fn load_runtime_metadata(&self, id: DownloadId) -> Result<Option<String>> {
        let record: Option<DownloadRecord> = Self::read_json(&self.record_path(id)).await?;
        Ok(record.and_then(|r| r.runtime_metadata))
    }

    async fn load_all_runtime_metadata(&self) -> Result<HashMap<DownloadId, String>> {
        let mut metadata = HashMap::new();
        for id in self.record_ids().await? {
            if let Some(json) = self.load_runtime_metadata(id).await? {
                metadata.insert(id, json);
            }
        }
        Ok(metadata)
    }

    #[cfg(feature = "recursive-http")]
    async fn save_recursive_job(&self, job: &TrackedRecursiveJob) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        Self::write_json(&self.recursive_path(job.id), job).await
    }

    #[cfg(feature = "recursive-http")]
    async fn load_recursive_jobs(&self) -> Result<Vec<TrackedRecursiveJob>> {
        let mut jobs: Vec<TrackedRecursiveJob> = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !stem.starts_with(RECURSIVE_PREFIX) {
                continue;
            }
            match Self::read_json::<TrackedRecursiveJob>(&path).await {
                Ok(Some(job)) => jobs.push(job),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("Skipping unreadable recursive job record {:?}: {}", path, e);
                }
            }
        }
        jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at));
        Ok(jobs)
    }

    #[cfg(feature = "recursive-http")]
    async fn delete_recursive_job(&self, id: uuid::Uuid) -> Result<()> {
        let _guard = self.write_lock.lock().await;
        Self::remove_if_exists(&self.recursive_path(id)).await
    }

    async fn health_check(&self) -> Result<()> {
        // Recreate the directory if it disappeared and prove it is writable.
        tokio::fs::create_dir_all(&self.dir).await?;
        // The probe deliberately runs without `write_lock`: a concurrent
        // `compact` may sweep it up (its extension is `tmp`), but
        // `remove_if_exists` tolerates the file already being gone, so the
        // check cannot fail from that race.
        let probe = self.dir.join(".health-check.tmp");
        tokio::fs::write(&probe, b"ok").await?;
        Self::remove_if_exists(&probe).await
    }

    async fn compact(&self) -> Result<()> {
        // Hold the write lock for the whole sweep so we never delete a temp
        // file between another task's write and rename inside `write_atomic`,
        // which would make that save fail.
        let _guard = self.write_lock.lock().await;
        // Clean up temp files left behind by interrupted writes.
        let mut entries = tokio::fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("tmp") {
                Self::remove_if_exists(&path).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SegmentState;
    use crate::types::{DownloadKind, DownloadMetadata, DownloadProgress, DownloadState};
    use chrono::Utc;

    fn create_test_status() -> DownloadStatus {
        DownloadStatus {
            id: DownloadId::new(),
            kind: DownloadKind::Http,
            state: DownloadState::Paused,
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
    async fn test_download_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path()).await.unwrap();
        let status = create_test_status();
        let id = status.id;

        storage.save_download(&status).await.unwrap();

        let loaded = storage.load_download(id).await.unwrap().unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.state, DownloadState::Paused);

        let all = storage.load_all().await.unwrap();
        assert_eq!(all.len(), 1);

        storage.delete_download(id).await.unwrap();
        assert!(storage.load_download(id).await.unwrap().is_none());
        assert!(storage.load_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_segments_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path()).await.unwrap();
        let id = DownloadId::new();

        let mut segments = vec![
            Segment::new(0, 0, 999),
            Segment::new(1, 1000, 1999),
            Segment::new(2, 2000, 2999),
        ];
        segments[0].downloaded = 1000;
        segments[0].state = SegmentState::Completed;
        segments[1].downloaded = 512;
        segments[1].state = SegmentState::Failed {
            error: "boom".to_string(),
            retries: 2,
        };

        storage.save_segments(id, &segments).await.unwrap();

        let loaded = storage.load_segments(id).await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].downloaded, 1000);
        assert_eq!(loaded[0].state, SegmentState::Completed);
        assert_eq!(
            loaded[1].state,
            SegmentState::Failed {
                error: "boom".to_string(),
                retries: 2
            }
        );
        assert_eq!(loaded[2].state, SegmentState::Pending);

        storage.delete_segments(id).await.unwrap();
        assert!(storage.load_segments(id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_torrent_data_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path()).await.unwrap();
        let id = DownloadId::new();

        assert!(storage.load_torrent_data(id).await.unwrap().is_none());

        let data = b"d8:announce3:url4:infod4:name4:teste".to_vec();
        storage.save_torrent_data(id, &data).await.unwrap();
        assert_eq!(storage.load_torrent_data(id).await.unwrap(), Some(data));

        storage.delete_download(id).await.unwrap();
        assert!(storage.load_torrent_data(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_runtime_metadata_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path()).await.unwrap();
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

        // Saving metadata must not clobber the status saved earlier.
        assert!(storage.load_download(id).await.unwrap().is_some());

        storage.delete_download(id).await.unwrap();
        assert!(storage.load_runtime_metadata(id).await.unwrap().is_none());
    }

    #[cfg(feature = "recursive-http")]
    #[tokio::test]
    async fn test_recursive_job_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path()).await.unwrap();
        let job = TrackedRecursiveJob {
            id: uuid::Uuid::new_v4(),
            root_url: "https://example.com/pub/".to_string(),
            child_ids: vec![DownloadId::new(), DownloadId::new()],
            created_at: Utc::now(),
        };

        storage.save_recursive_job(&job).await.unwrap();
        assert_eq!(
            storage.load_recursive_jobs().await.unwrap(),
            vec![job.clone()]
        );

        // Recursive records must not show up as downloads.
        assert!(storage.load_all().await.unwrap().is_empty());

        storage.delete_recursive_job(job.id).await.unwrap();
        assert!(storage.load_recursive_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let status = create_test_status();
        let id = status.id;

        {
            let storage = FileStorage::new(dir.path()).await.unwrap();
            storage.save_download(&status).await.unwrap();
            storage
                .save_segments(id, &[Segment::new(0, 0, 999)])
                .await
                .unwrap();
        }

        let storage = FileStorage::new(dir.path()).await.unwrap();
        assert!(storage.load_download(id).await.unwrap().is_some());
        assert_eq!(storage.load_segments(id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_health_check_and_compact() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path()).await.unwrap();
        storage.health_check().await.unwrap();

        // Simulate an interrupted write and verify compact cleans it up.
        let leftover = dir.path().join("garbage.json.tmp");
        tokio::fs::write(&leftover, b"partial").await.unwrap();
        storage.compact().await.unwrap();
        assert!(!leftover.exists());
    }

    #[tokio::test]
    async fn test_compact_waits_for_write_lock() {
        let dir = tempfile::tempdir().unwrap();
        let storage = std::sync::Arc::new(FileStorage::new(dir.path()).await.unwrap());

        // Hold the write lock as `write_atomic` callers do mid-save, with a
        // temp file present as if a write were in flight.
        let guard = storage.write_lock.lock().await;
        let in_flight = dir.path().join("garbage.json.tmp");
        tokio::fs::write(&in_flight, b"partial").await.unwrap();

        let compact_storage = storage.clone();
        let compact = tokio::spawn(async move { compact_storage.compact().await });

        // While the lock is held, compact must not delete the temp file out
        // from under the in-flight writer.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!compact.is_finished());
        assert!(in_flight.exists());

        // Once the writer is done, compact proceeds and sweeps the leftover.
        drop(guard);
        compact.await.unwrap().unwrap();
        assert!(!in_flight.exists());
    }
}
