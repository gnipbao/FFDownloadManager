//! SQLite Storage Implementation
//!
//! Provides persistent storage using SQLite with WAL mode for crash safety.

use super::{Segment, SegmentState, Storage};
use crate::error::{EngineError, Result};
#[cfg(feature = "recursive-http")]
use crate::types::TrackedRecursiveJob;
use crate::types::{
    DownloadId, DownloadKind, DownloadMetadata, DownloadProgress, DownloadState, DownloadStatus,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// SQLite-based storage for download persistence
pub struct SqliteStorage {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStorage {
    /// Create a new SQLite storage at the given path
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    EngineError::Database(format!("Failed to create database directory: {}", e))
                })?;
            }
        }

        let path = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open(&path)?;

            // Enable WAL mode for better concurrency and crash safety
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;

            // Run schema migrations
            migrate(&conn)?;

            Ok(conn)
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to initialize database: {}", e)))??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create an in-memory SQLite database (for testing)
    pub async fn in_memory() -> Result<Self> {
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let conn = Connection::open_in_memory()?;
            conn.pragma_update(None, "foreign_keys", "ON")?;
            migrate(&conn)?;
            Ok(conn)
        })
        .await
        .map_err(|e| {
            EngineError::Database(format!("Failed to create in-memory database: {}", e))
        })??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

/// Current schema version — bump when adding migrations
const CURRENT_SCHEMA_VERSION: u32 = 4;

/// Database schema v1
const SCHEMA_V1: &str = r#"
-- Downloads table
CREATE TABLE IF NOT EXISTS downloads (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    state_error_kind TEXT,
    state_error_message TEXT,
    state_error_retryable INTEGER,

    -- Progress
    total_size INTEGER,
    completed_size INTEGER NOT NULL DEFAULT 0,
    download_speed INTEGER NOT NULL DEFAULT 0,
    upload_speed INTEGER NOT NULL DEFAULT 0,
    connections INTEGER NOT NULL DEFAULT 0,
    seeders INTEGER NOT NULL DEFAULT 0,
    peers INTEGER NOT NULL DEFAULT 0,

    -- Priority
    priority TEXT NOT NULL DEFAULT 'normal',

    -- Metadata
    name TEXT NOT NULL,
    url TEXT,
    magnet_uri TEXT,
    info_hash TEXT,
    save_dir TEXT NOT NULL,
    filename TEXT,
    user_agent TEXT,
    referer TEXT,
    headers_json TEXT,
    cookies_json TEXT,
    checksum_json TEXT,
    mirrors_json TEXT,

    -- Resume validation (HTTP)
    etag TEXT,
    last_modified TEXT,

    -- Timestamps
    created_at TEXT NOT NULL,
    completed_at TEXT
);

-- Segments table for HTTP multi-connection downloads
CREATE TABLE IF NOT EXISTS segments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    download_id TEXT NOT NULL,
    segment_index INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,
    end_byte INTEGER NOT NULL,
    downloaded INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL,
    error_message TEXT,
    error_retries INTEGER DEFAULT 0,

    FOREIGN KEY (download_id) REFERENCES downloads(id) ON DELETE CASCADE,
    UNIQUE (download_id, segment_index)
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_downloads_state ON downloads(state);
CREATE INDEX IF NOT EXISTS idx_downloads_kind ON downloads(kind);
CREATE INDEX IF NOT EXISTS idx_segments_download ON segments(download_id);
"#;

/// Run schema migrations to bring the database up to `CURRENT_SCHEMA_VERSION`.
///
/// Uses SQLite's `PRAGMA user_version` to track the current version. Each
/// migration is applied in order, and the version is bumped after each step.
/// The function is idempotent — calling it on an already-current database is a
/// no-op.
fn migrate(conn: &Connection) -> std::result::Result<(), rusqlite::Error> {
    // Run every step inside one transaction: a crash between an ALTER TABLE
    // and its user_version bump would otherwise make all future migrations
    // fail with "duplicate column name" — permanently.
    let tx = conn.unchecked_transaction()?;
    migrate_steps(&tx)?;
    tx.commit()?;

    debug_assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        CURRENT_SCHEMA_VERSION
    );

    Ok(())
}

fn migrate_steps(conn: &Connection) -> std::result::Result<(), rusqlite::Error> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if version < 1 {
        // Check whether this is a legacy database (tables already created
        // before versioning was added) or a fresh database.
        let table_exists: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='downloads'",
            [],
            |row| row.get(0),
        )?;

        if !table_exists {
            // Fresh database — create all tables
            conn.execute_batch(SCHEMA_V1)?;
        }
        // Legacy database — tables already exist, skip creation

        conn.pragma_update(None, "user_version", 1)?;
    }

    if version < 2 {
        // Add column to store raw torrent/magnet metadata for crash recovery.
        // On fresh databases v1 already ran, so the table exists but lacks this column.
        conn.execute_batch("ALTER TABLE downloads ADD COLUMN torrent_data BLOB")?;
        conn.pragma_update(None, "user_version", 2)?;
    }

    if version < 3 {
        // Add engine runtime metadata sidecar for resumable per-download context.
        conn.execute_batch("ALTER TABLE downloads ADD COLUMN runtime_metadata_json TEXT")?;
        conn.pragma_update(None, "user_version", 3)?;
    }

    if version < 4 {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS recursive_jobs (
                id TEXT PRIMARY KEY,
                root_url TEXT NOT NULL,
                child_ids_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_recursive_jobs_created_at ON recursive_jobs(created_at);
            "#,
        )?;
        conn.pragma_update(None, "user_version", 4)?;
    }

    Ok(())
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn save_download(&self, status: &DownloadStatus) -> Result<()> {
        let conn = self.conn.clone();
        let status = status.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();

            // Serialize state
            let (state_str, error_kind, error_msg, error_retryable) = match &status.state {
                DownloadState::Queued => ("queued", None, None, None),
                DownloadState::Connecting => ("connecting", None, None, None),
                DownloadState::Downloading => ("downloading", None, None, None),
                DownloadState::Seeding => ("seeding", None, None, None),
                DownloadState::Paused => ("paused", None, None, None),
                DownloadState::Completed => ("completed", None, None, None),
                DownloadState::Error {
                    kind,
                    message,
                    retryable,
                } => ("error", Some(kind.clone()), Some(message.clone()), Some(*retryable)),
            };

            // Serialize kind
            let kind_str = match status.kind {
                DownloadKind::Http => "http",
                DownloadKind::Torrent => "torrent",
                DownloadKind::Magnet => "magnet",
            };

            // Serialize priority
            let priority_str = status.priority.to_string();

            // Serialize headers to JSON
            let headers_json = serde_json::to_string(&status.metadata.headers)
                .unwrap_or_else(|_| "[]".to_string());

            // Serialize cookies to JSON
            let cookies_json = serde_json::to_string(&status.metadata.cookies)
                .unwrap_or_else(|_| "[]".to_string());

            // Serialize checksum to JSON (if present)
            let checksum_json = status
                .metadata
                .checksum
                .as_ref()
                .and_then(|c| serde_json::to_string(c).ok());

            // Serialize mirrors to JSON
            let mirrors_json = serde_json::to_string(&status.metadata.mirrors)
                .unwrap_or_else(|_| "[]".to_string());

            conn.execute(
                r#"
                INSERT INTO downloads (
                    id, kind, state, state_error_kind, state_error_message, state_error_retryable,
                    total_size, completed_size, download_speed, upload_speed, connections, seeders, peers,
                    priority,
                    name, url, magnet_uri, info_hash, save_dir, filename, user_agent, referer,
                    headers_json, cookies_json, checksum_json, mirrors_json,
                    etag, last_modified, created_at, completed_at
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6,
                    ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14,
                    ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22,
                    ?23, ?24, ?25, ?26,
                    ?27, ?28, ?29, ?30
                )
                ON CONFLICT(id) DO UPDATE SET
                    state = excluded.state,
                    state_error_kind = excluded.state_error_kind,
                    state_error_message = excluded.state_error_message,
                    state_error_retryable = excluded.state_error_retryable,
                    total_size = excluded.total_size,
                    completed_size = excluded.completed_size,
                    download_speed = excluded.download_speed,
                    upload_speed = excluded.upload_speed,
                    connections = excluded.connections,
                    seeders = excluded.seeders,
                    peers = excluded.peers,
                    priority = excluded.priority,
                    name = excluded.name,
                    url = excluded.url,
                    magnet_uri = excluded.magnet_uri,
                    info_hash = excluded.info_hash,
                    save_dir = excluded.save_dir,
                    filename = excluded.filename,
                    user_agent = excluded.user_agent,
                    referer = excluded.referer,
                    headers_json = excluded.headers_json,
                    cookies_json = excluded.cookies_json,
                    checksum_json = excluded.checksum_json,
                    mirrors_json = excluded.mirrors_json,
                    etag = excluded.etag,
                    last_modified = excluded.last_modified,
                    completed_at = excluded.completed_at
                "#,
                params![
                    status.id.as_uuid().to_string(),
                    kind_str,
                    state_str,
                    error_kind,
                    error_msg,
                    error_retryable,
                    status.progress.total_size.map(|s| s as i64),
                    status.progress.completed_size as i64,
                    status.progress.download_speed as i64,
                    status.progress.upload_speed as i64,
                    status.progress.connections as i64,
                    status.progress.seeders as i64,
                    status.progress.peers as i64,
                    priority_str,
                    status.metadata.name,
                    status.metadata.url,
                    status.metadata.magnet_uri,
                    status.metadata.info_hash,
                    status.metadata.save_dir.to_string_lossy().to_string(),
                    status.metadata.filename,
                    status.metadata.user_agent,
                    status.metadata.referer,
                    headers_json,
                    cookies_json,
                    checksum_json,
                    mirrors_json,
                    status.metadata.etag,
                    status.metadata.last_modified,
                    status.created_at.to_rfc3339(),
                    status.completed_at.map(|t| t.to_rfc3339()),
                ],
            )?;

            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to save download: {}", e)))?
    }

    async fn load_download(&self, id: DownloadId) -> Result<Option<DownloadStatus>> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<DownloadStatus>> {
            let conn = conn.blocking_lock();

            let result: Option<DownloadStatus> = conn
                .query_row(
                    r#"
                    SELECT
                        id, kind, state, state_error_kind, state_error_message, state_error_retryable,
                        total_size, completed_size, download_speed, upload_speed, connections, seeders, peers,
                        priority,
                        name, url, magnet_uri, info_hash, save_dir, filename, user_agent, referer,
                        headers_json, cookies_json, checksum_json, mirrors_json,
                        etag, last_modified, created_at, completed_at
                    FROM downloads
                    WHERE id = ?1
                    "#,
                    params![id_str],
                    |row| {
                        row_to_status(row)
                    },
                )
                .optional()?;

            Ok(result)
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to load download: {}", e)))?
    }

    async fn load_all(&self) -> Result<Vec<DownloadStatus>> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<DownloadStatus>> {
            let conn = conn.blocking_lock();

            let mut stmt = conn.prepare(
                r#"
                SELECT
                    id, kind, state, state_error_kind, state_error_message, state_error_retryable,
                    total_size, completed_size, download_speed, upload_speed, connections, seeders, peers,
                    priority,
                    name, url, magnet_uri, info_hash, save_dir, filename, user_agent, referer,
                    headers_json, cookies_json, checksum_json, mirrors_json,
                    etag, last_modified, created_at, completed_at
                FROM downloads
                ORDER BY created_at DESC
                "#,
            )?;

            let iter = stmt.query_map([], row_to_status)?;

            let mut results = Vec::new();
            for status in iter {
                results.push(status?);
            }

            Ok(results)
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to load all downloads: {}", e)))?
    }

    async fn delete_download(&self, id: DownloadId) -> Result<()> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            conn.execute("DELETE FROM downloads WHERE id = ?1", params![id_str])?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to delete download: {}", e)))?
    }

    async fn save_segments(&self, id: DownloadId, segments: &[Segment]) -> Result<()> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();
        let segments = segments.to_vec();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();

            let tx = conn.unchecked_transaction()?;

            // Delete existing segments first
            tx.execute(
                "DELETE FROM segments WHERE download_id = ?1",
                params![id_str],
            )?;

            // Insert new segments
            {
                let mut stmt = tx.prepare(
                    r#"
                    INSERT INTO segments (download_id, segment_index, start_byte, end_byte, downloaded, state, error_message, error_retries)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                    "#,
                )?;

                for segment in &segments {
                    let (state_str, error_msg, retries) = match &segment.state {
                        SegmentState::Pending => ("pending", None, 0),
                        SegmentState::Downloading => ("downloading", None, 0),
                        SegmentState::Completed => ("completed", None, 0),
                        SegmentState::Failed { error, retries } => {
                            ("failed", Some(error.clone()), *retries)
                        }
                    };

                    stmt.execute(params![
                        id_str,
                        segment.index as i64,
                        segment.start as i64,
                        segment.end as i64,
                        segment.downloaded as i64,
                        state_str,
                        error_msg,
                        retries as i64,
                    ])?;
                }
            }

            tx.commit()?;

            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to save segments: {}", e)))?
    }

    async fn load_segments(&self, id: DownloadId) -> Result<Vec<Segment>> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();

        tokio::task::spawn_blocking(move || -> Result<Vec<Segment>> {
            let conn = conn.blocking_lock();

            let mut stmt = conn.prepare(
                r#"
                SELECT segment_index, start_byte, end_byte, downloaded, state, error_message, error_retries
                FROM segments
                WHERE download_id = ?1
                ORDER BY segment_index
                "#,
            )?;

            let iter = stmt.query_map(params![id_str], |row| {
                let index: i64 = row.get(0)?;
                let start: i64 = row.get(1)?;
                let end: i64 = row.get(2)?;
                let downloaded: i64 = row.get(3)?;
                let state_str: String = row.get(4)?;
                let error_msg: Option<String> = row.get(5)?;
                let retries: i64 = row.get(6)?;

                // CRASH RECOVERY SEMANTICS:
                // When loading segment state from disk, we apply conservative recovery:
                //
                // - "downloading" -> Pending: A segment marked as "downloading" means the
                //   process crashed mid-download. The `downloaded` field preserves how many
                //   bytes were written, allowing resume from that offset. We reset to Pending
                //   so the download logic will re-request the remaining bytes.
                //
                // - Unknown states -> Pending: Database corruption or schema migration could
                //   produce unknown state values. Defaulting to Pending ensures the segment
                //   will be re-downloaded rather than skipped or causing errors.
                //
                // The `downloaded` field is preserved in all cases, enabling byte-accurate
                // resume via HTTP Range requests.
                let state = match state_str.as_str() {
                    "pending" => SegmentState::Pending,
                    "downloading" => SegmentState::Pending,
                    "completed" => SegmentState::Completed,
                    "failed" => SegmentState::Failed {
                        error: error_msg.unwrap_or_default(),
                        retries: retries as u32,
                    },
                    _ => SegmentState::Pending,
                };

                Ok(Segment {
                    index: index as usize,
                    start: start as u64,
                    end: end as u64,
                    downloaded: downloaded as u64,
                    state,
                })
            })?;

            let mut segments = Vec::new();
            for segment in iter {
                segments.push(segment?);
            }

            Ok(segments)
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to load segments: {}", e)))?
    }

    async fn delete_segments(&self, id: DownloadId) -> Result<()> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM segments WHERE download_id = ?1",
                params![id_str],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to delete segments: {}", e)))?
    }

    async fn save_torrent_data(&self, id: DownloadId, data: &[u8]) -> Result<()> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();
        let data = data.to_vec();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            let updated = conn.execute(
                "UPDATE downloads SET torrent_data = ?1 WHERE id = ?2",
                params![data, id_str],
            )?;
            // A bare UPDATE silently affects 0 rows when the download row
            // doesn't exist; surface that instead of pretending the data
            // was persisted.
            if updated == 0 {
                return Err(EngineError::NotFound(id.to_string()));
            }
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to save torrent data: {}", e)))?
    }

    async fn load_torrent_data(&self, id: DownloadId) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
            let conn = conn.blocking_lock();
            let result: Option<Option<Vec<u8>>> = conn
                .query_row(
                    "SELECT torrent_data FROM downloads WHERE id = ?1",
                    params![id_str],
                    |row| row.get(0),
                )
                .optional()?;
            // Flatten: None (row missing) or Some(None) (NULL column) → None
            Ok(result.flatten())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to load torrent data: {}", e)))?
    }

    async fn save_runtime_metadata(&self, id: DownloadId, runtime_json: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();
        let runtime_json = runtime_json.to_string();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            let updated = conn.execute(
                "UPDATE downloads SET runtime_metadata_json = ?1 WHERE id = ?2",
                params![runtime_json, id_str],
            )?;
            // A bare UPDATE silently affects 0 rows when the download row
            // doesn't exist; surface that instead of pretending the data
            // was persisted.
            if updated == 0 {
                return Err(EngineError::NotFound(id.to_string()));
            }
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to save runtime metadata: {}", e)))?
    }

    async fn load_runtime_metadata(&self, id: DownloadId) -> Result<Option<String>> {
        let conn = self.conn.clone();
        let id_str = id.as_uuid().to_string();

        tokio::task::spawn_blocking(move || -> Result<Option<String>> {
            let conn = conn.blocking_lock();
            conn.query_row(
                "SELECT runtime_metadata_json FROM downloads WHERE id = ?1",
                params![id_str],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to load runtime metadata: {}", e)))?
    }

    async fn load_all_runtime_metadata(&self) -> Result<HashMap<DownloadId, String>> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<HashMap<DownloadId, String>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                r#"
                SELECT id, runtime_metadata_json
                FROM downloads
                WHERE runtime_metadata_json IS NOT NULL
                "#,
            )?;

            let rows = stmt.query_map([], |row| {
                let id_str: String = row.get(0)?;
                let runtime_json: String = row.get(1)?;
                Ok((id_str, runtime_json))
            })?;

            let mut results = HashMap::new();
            for row in rows {
                let (id_str, runtime_json) = row?;
                let uuid = uuid::Uuid::parse_str(&id_str).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                results.insert(DownloadId::from_uuid(uuid), runtime_json);
            }

            Ok(results)
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to load runtime metadata: {}", e)))?
    }

    #[cfg(feature = "recursive-http")]
    async fn save_recursive_job(&self, job: &TrackedRecursiveJob) -> Result<()> {
        let conn = self.conn.clone();
        let job = job.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            let child_ids_json = serde_json::to_string(&job.child_ids).map_err(|e| {
                EngineError::Database(format!("Failed to serialize child ids: {}", e))
            })?;
            conn.execute(
                r#"
                INSERT INTO recursive_jobs (id, root_url, child_ids_json, created_at)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(id) DO UPDATE SET
                    root_url = excluded.root_url,
                    child_ids_json = excluded.child_ids_json,
                    created_at = excluded.created_at
                "#,
                params![
                    job.id.to_string(),
                    job.root_url,
                    child_ids_json,
                    job.created_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to save recursive job: {}", e)))?
    }

    #[cfg(feature = "recursive-http")]
    async fn load_recursive_jobs(&self) -> Result<Vec<TrackedRecursiveJob>> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<Vec<TrackedRecursiveJob>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                r#"
                SELECT id, root_url, child_ids_json, created_at
                FROM recursive_jobs
                ORDER BY created_at DESC
                "#,
            )?;

            let rows = stmt.query_map([], |row| {
                let id_str: String = row.get(0)?;
                let root_url: String = row.get(1)?;
                let child_ids_json: String = row.get(2)?;
                let created_at_str: String = row.get(3)?;
                Ok((id_str, root_url, child_ids_json, created_at_str))
            })?;

            let mut jobs = Vec::new();
            for row in rows {
                let (id_str, root_url, child_ids_json, created_at_str) = row?;
                let id = uuid::Uuid::parse_str(&id_str).map_err(|e| {
                    EngineError::Database(format!("Invalid recursive job id '{}': {}", id_str, e))
                })?;
                let child_ids = serde_json::from_str(&child_ids_json).map_err(|e| {
                    EngineError::Database(format!(
                        "Failed to deserialize recursive child ids for {}: {}",
                        id, e
                    ))
                })?;
                let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        EngineError::Database(format!(
                            "Invalid recursive job timestamp for {}: {}",
                            id, e
                        ))
                    })?;
                jobs.push(TrackedRecursiveJob {
                    id,
                    root_url,
                    child_ids,
                    created_at,
                });
            }

            Ok(jobs)
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to load recursive jobs: {}", e)))?
    }

    #[cfg(feature = "recursive-http")]
    async fn delete_recursive_job(&self, id: uuid::Uuid) -> Result<()> {
        let conn = self.conn.clone();
        let id_str = id.to_string();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            conn.execute("DELETE FROM recursive_jobs WHERE id = ?1", params![id_str])?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Failed to delete recursive job: {}", e)))?
    }

    async fn health_check(&self) -> Result<()> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            // Use query_row since we're expecting a result
            let _: i64 = conn.query_row("SELECT 1", [], |row| row.get(0))?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Health check failed: {}", e)))?
    }

    async fn compact(&self) -> Result<()> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = conn.blocking_lock();
            conn.execute("VACUUM", [])?;
            Ok(())
        })
        .await
        .map_err(|e| EngineError::Database(format!("Compact failed: {}", e)))?
    }
}

/// Convert a database row to a DownloadStatus
fn row_to_status(row: &rusqlite::Row<'_>) -> rusqlite::Result<DownloadStatus> {
    let id_str: String = row.get(0)?;
    let kind_str: String = row.get(1)?;
    let state_str: String = row.get(2)?;
    let error_kind: Option<String> = row.get(3)?;
    let error_msg: Option<String> = row.get(4)?;
    let error_retryable: Option<bool> = row.get(5)?;

    let total_size: Option<i64> = row.get(6)?;
    let completed_size: i64 = row.get(7)?;
    let download_speed: i64 = row.get(8)?;
    let upload_speed: i64 = row.get(9)?;
    let connections: i64 = row.get(10)?;
    let seeders: i64 = row.get(11)?;
    let peers: i64 = row.get(12)?;

    let priority_str: String = row.get(13)?;

    let name: String = row.get(14)?;
    let url: Option<String> = row.get(15)?;
    let magnet_uri: Option<String> = row.get(16)?;
    let info_hash: Option<String> = row.get(17)?;
    let save_dir: String = row.get(18)?;
    let filename: Option<String> = row.get(19)?;
    let user_agent: Option<String> = row.get(20)?;
    let referer: Option<String> = row.get(21)?;
    let headers_json: Option<String> = row.get(22)?;
    let cookies_json: Option<String> = row.get(23)?;
    let checksum_json: Option<String> = row.get(24)?;
    let mirrors_json: Option<String> = row.get(25)?;

    let etag: Option<String> = row.get(26)?;
    let last_modified: Option<String> = row.get(27)?;
    let created_at_str: String = row.get(28)?;
    let completed_at_str: Option<String> = row.get(29)?;

    // Parse ID
    let uuid = uuid::Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let id = DownloadId::from_uuid(uuid);

    // Parse kind
    // CRASH RECOVERY: Unknown kind values (from database corruption or schema changes)
    // default to Http as the safest option - HTTP downloads are simpler and won't
    // attempt to connect to BitTorrent peers with invalid metadata.
    let kind = match kind_str.as_str() {
        "http" => DownloadKind::Http,
        "torrent" => DownloadKind::Torrent,
        "magnet" => DownloadKind::Magnet,
        _ => {
            tracing::warn!(
                "Unknown download kind '{}' for download {}, defaulting to Http",
                kind_str,
                id_str
            );
            DownloadKind::Http
        }
    };

    // Parse state
    // CRASH RECOVERY: Unknown state values default to Queued, which is a safe
    // initial state that will cause the download to be re-evaluated and started
    // appropriately based on its metadata.
    let state = match state_str.as_str() {
        "queued" => DownloadState::Queued,
        "connecting" => DownloadState::Connecting,
        "downloading" => DownloadState::Downloading,
        "seeding" => DownloadState::Seeding,
        "paused" => DownloadState::Paused,
        "completed" => DownloadState::Completed,
        "error" => DownloadState::Error {
            kind: error_kind.unwrap_or_default(),
            message: error_msg.unwrap_or_default(),
            retryable: error_retryable.unwrap_or(false),
        },
        _ => {
            tracing::warn!(
                "Unknown download state '{}' for download {}, defaulting to Queued",
                state_str,
                id_str
            );
            DownloadState::Queued
        }
    };

    // Parse priority
    let priority = priority_str
        .parse::<crate::priority_queue::DownloadPriority>()
        .unwrap_or_default();

    // Parse headers
    let headers: Vec<(String, String)> = headers_json
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // Parse cookies
    let cookies: Vec<String> = cookies_json
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // Parse checksum
    let checksum: Option<crate::http::ExpectedChecksum> =
        checksum_json.and_then(|s| serde_json::from_str(&s).ok());

    // Parse mirrors
    let mirrors: Vec<String> = mirrors_json
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // Parse timestamps
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());

    let completed_at = completed_at_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    });

    Ok(DownloadStatus {
        id,
        kind,
        state,
        priority,
        progress: DownloadProgress {
            total_size: total_size.map(|n| n as u64),
            completed_size: completed_size as u64,
            download_speed: download_speed as u64,
            upload_speed: upload_speed as u64,
            connections: connections as u32,
            seeders: seeders as u32,
            peers: peers as u32,
            eta_seconds: None,
        },
        metadata: DownloadMetadata {
            name,
            url,
            magnet_uri,
            info_hash,
            save_dir: PathBuf::from(save_dir),
            filename,
            user_agent,
            referer,
            headers,
            cookies,
            checksum,
            mirrors,
            etag,
            last_modified,
        },
        torrent_info: None,
        peers: None,
        created_at,
        completed_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_status() -> DownloadStatus {
        DownloadStatus {
            id: DownloadId::new(),
            kind: DownloadKind::Http,
            state: DownloadState::Downloading,
            priority: crate::priority_queue::DownloadPriority::Normal,
            progress: DownloadProgress {
                total_size: Some(1000),
                completed_size: 500,
                download_speed: 100,
                upload_speed: 0,
                connections: 4,
                seeders: 0,
                peers: 0,
                eta_seconds: Some(5),
            },
            metadata: DownloadMetadata {
                name: "test.zip".to_string(),
                url: Some("https://example.com/test.zip".to_string()),
                magnet_uri: None,
                info_hash: None,
                save_dir: PathBuf::from("/tmp/downloads"),
                filename: Some("test.zip".to_string()),
                user_agent: Some("gosh-dl/0.1.0".to_string()),
                referer: None,
                headers: vec![("X-Custom".to_string(), "value".to_string())],
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
    async fn test_sqlite_save_load() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let status = create_test_status();
        let id = status.id;

        // Save
        storage.save_download(&status).await.unwrap();

        // Load
        let loaded = storage.load_download(id).await.unwrap().unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.metadata.name, "test.zip");
        assert_eq!(loaded.progress.completed_size, 500);
    }

    #[tokio::test]
    async fn test_sqlite_load_all() {
        let storage = SqliteStorage::in_memory().await.unwrap();

        // Save multiple
        for i in 0..5 {
            let mut status = create_test_status();
            status.metadata.name = format!("file{}.zip", i);
            storage.save_download(&status).await.unwrap();
        }

        // Load all
        let all = storage.load_all().await.unwrap();
        assert_eq!(all.len(), 5);
    }

    #[tokio::test]
    async fn test_sqlite_delete() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let status = create_test_status();
        let id = status.id;

        storage.save_download(&status).await.unwrap();
        storage.delete_download(id).await.unwrap();

        let loaded = storage.load_download(id).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_sqlite_segments() {
        let storage = SqliteStorage::in_memory().await.unwrap();

        // First create a download (foreign key constraint)
        let status = create_test_status();
        let id = status.id;
        storage.save_download(&status).await.unwrap();

        let segments = vec![
            Segment::new(0, 0, 999),
            Segment {
                index: 1,
                start: 1000,
                end: 1999,
                downloaded: 500,
                state: SegmentState::Downloading,
            },
            Segment {
                index: 2,
                start: 2000,
                end: 2999,
                downloaded: 1000,
                state: SegmentState::Completed,
            },
        ];

        // Save segments
        storage.save_segments(id, &segments).await.unwrap();

        // Load segments
        let loaded = storage.load_segments(id).await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].start, 0);
        assert_eq!(loaded[1].downloaded, 500);
        assert!(matches!(loaded[2].state, SegmentState::Completed));
    }

    #[tokio::test]
    async fn test_sqlite_update() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let mut status = create_test_status();
        let id = status.id;

        // Save initial
        storage.save_download(&status).await.unwrap();

        // Update
        status.progress.completed_size = 800;
        status.state = DownloadState::Completed;
        status.completed_at = Some(Utc::now());
        storage.save_download(&status).await.unwrap();

        // Verify update
        let loaded = storage.load_download(id).await.unwrap().unwrap();
        assert_eq!(loaded.progress.completed_size, 800);
        assert!(matches!(loaded.state, DownloadState::Completed));
        assert!(loaded.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_sqlite_health_check() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        storage.health_check().await.unwrap();
    }

    #[tokio::test]
    async fn test_sqlite_priority_persistence() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let mut status = create_test_status();
        status.priority = crate::priority_queue::DownloadPriority::High;
        let id = status.id;

        storage.save_download(&status).await.unwrap();
        let loaded = storage.load_download(id).await.unwrap().unwrap();

        assert_eq!(
            loaded.priority,
            crate::priority_queue::DownloadPriority::High
        );
    }

    #[tokio::test]
    async fn test_sqlite_cookies_persistence() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let mut status = create_test_status();
        status.metadata.cookies = vec!["session=abc123".to_string(), "user=test".to_string()];
        let id = status.id;

        storage.save_download(&status).await.unwrap();
        let loaded = storage.load_download(id).await.unwrap().unwrap();

        assert_eq!(loaded.metadata.cookies.len(), 2);
        assert!(loaded
            .metadata
            .cookies
            .contains(&"session=abc123".to_string()));
        assert!(loaded.metadata.cookies.contains(&"user=test".to_string()));
    }

    #[tokio::test]
    async fn test_sqlite_checksum_persistence() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let mut status = create_test_status();
        status.metadata.checksum = Some(crate::http::ExpectedChecksum::sha256(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ));
        let id = status.id;

        storage.save_download(&status).await.unwrap();
        let loaded = storage.load_download(id).await.unwrap().unwrap();

        assert!(loaded.metadata.checksum.is_some());
        let checksum = loaded.metadata.checksum.unwrap();
        assert_eq!(checksum.algorithm, crate::http::ChecksumAlgorithm::Sha256);
        assert_eq!(
            checksum.value,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn test_sqlite_mirrors_persistence() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let mut status = create_test_status();
        status.metadata.mirrors = vec![
            "https://mirror1.example.com/file.zip".to_string(),
            "https://mirror2.example.com/file.zip".to_string(),
        ];
        let id = status.id;

        storage.save_download(&status).await.unwrap();
        let loaded = storage.load_download(id).await.unwrap().unwrap();

        assert_eq!(loaded.metadata.mirrors.len(), 2);
        assert!(loaded
            .metadata
            .mirrors
            .contains(&"https://mirror1.example.com/file.zip".to_string()));
    }

    #[tokio::test]
    async fn test_sqlite_full_metadata_persistence() {
        // Test all new fields together
        let storage = SqliteStorage::in_memory().await.unwrap();
        let mut status = create_test_status();

        status.priority = crate::priority_queue::DownloadPriority::Critical;
        status.metadata.cookies = vec!["auth=token".to_string()];
        status.metadata.checksum = Some(crate::http::ExpectedChecksum::md5(
            "d41d8cd98f00b204e9800998ecf8427e",
        ));
        status.metadata.mirrors = vec!["https://backup.example.com/file.zip".to_string()];

        let id = status.id;
        storage.save_download(&status).await.unwrap();

        let loaded = storage.load_download(id).await.unwrap().unwrap();

        assert_eq!(
            loaded.priority,
            crate::priority_queue::DownloadPriority::Critical
        );
        assert_eq!(loaded.metadata.cookies, vec!["auth=token".to_string()]);
        assert!(loaded.metadata.checksum.is_some());
        assert_eq!(loaded.metadata.mirrors.len(), 1);
    }

    #[tokio::test]
    async fn test_schema_versioning() {
        // Create a fresh in-memory database
        let storage = SqliteStorage::in_memory().await.unwrap();

        // Verify version is set to CURRENT_SCHEMA_VERSION
        let conn = storage.conn.lock().await;
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);

        // Running migrate again should be idempotent (no-op)
        migrate(&conn).unwrap();
        let version2: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version2, CURRENT_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn test_schema_versioning_legacy_db() {
        // Simulate a legacy database (tables exist but no user_version set)
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        // user_version defaults to 0 for a fresh SQLite database

        // migrate should detect existing tables and just bump version
        migrate(&conn).unwrap();
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn test_torrent_data_persistence() {
        let storage = SqliteStorage::in_memory().await.unwrap();

        let status = create_test_status();
        let id = status.id;
        storage.save_download(&status).await.unwrap();

        // No torrent data initially
        let data = storage.load_torrent_data(id).await.unwrap();
        assert!(data.is_none());

        // Save torrent data
        let torrent_bytes = b"d4:infod6:lengthi1024e4:name9:test.txte4:name9:test.txte";
        storage.save_torrent_data(id, torrent_bytes).await.unwrap();

        // Load it back
        let loaded = storage.load_torrent_data(id).await.unwrap();
        assert_eq!(loaded.unwrap(), torrent_bytes);
    }

    #[tokio::test]
    async fn test_torrent_data_survives_status_update() {
        let storage = SqliteStorage::in_memory().await.unwrap();

        let mut status = create_test_status();
        let id = status.id;
        storage.save_download(&status).await.unwrap();

        // Save torrent data
        let torrent_bytes = vec![1, 2, 3, 4, 5];
        storage.save_torrent_data(id, &torrent_bytes).await.unwrap();

        // Update the download status (upsert)
        status.progress.completed_size = 999;
        storage.save_download(&status).await.unwrap();

        // Torrent data should still be there (save_download doesn't touch it)
        let loaded = storage.load_torrent_data(id).await.unwrap();
        assert_eq!(loaded.unwrap(), torrent_bytes);
    }

    #[tokio::test]
    async fn test_save_torrent_data_missing_download_errors() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let id = DownloadId::new();

        let err = storage.save_torrent_data(id, b"data").await.unwrap_err();
        assert!(matches!(err, EngineError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_save_runtime_metadata_missing_download_errors() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let id = DownloadId::new();

        let err = storage
            .save_runtime_metadata(id, r#"{"key":"value"}"#)
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_runtime_metadata_persistence() {
        let storage = SqliteStorage::in_memory().await.unwrap();
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
    }

    #[cfg(feature = "recursive-http")]
    #[tokio::test]
    async fn test_recursive_job_persistence() {
        let storage = SqliteStorage::in_memory().await.unwrap();
        let job = TrackedRecursiveJob {
            id: uuid::Uuid::new_v4(),
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

    #[tokio::test]
    async fn test_schema_v1_to_v3_migration() {
        // Simulate a v1 database (tables exist, version = 1, no torrent_data column)
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();

        // Migrate should add the torrent_data and runtime metadata columns.
        migrate(&conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);

        // Verify both columns exist by inserting and querying.
        conn.execute(
            "INSERT INTO downloads (id, kind, state, name, save_dir, created_at) VALUES (?1, 'http', 'queued', 'test', '/tmp', '2024-01-01T00:00:00Z')",
            params!["test-id"],
        ).unwrap();
        conn.execute(
            "UPDATE downloads SET torrent_data = ?1 WHERE id = 'test-id'",
            params![vec![1u8, 2, 3]],
        )
        .unwrap();
        conn.execute(
            "UPDATE downloads SET runtime_metadata_json = ?1 WHERE id = 'test-id'",
            params![r#"{"recursive_child":{"fail_fast":true}}"#],
        )
        .unwrap();
        let data: Option<Vec<u8>> = conn
            .query_row(
                "SELECT torrent_data FROM downloads WHERE id = 'test-id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(data.unwrap(), vec![1u8, 2, 3]);
        let runtime: Option<String> = conn
            .query_row(
                "SELECT runtime_metadata_json FROM downloads WHERE id = 'test-id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            runtime.as_deref(),
            Some(r#"{"recursive_child":{"fail_fast":true}}"#)
        );

        conn.execute(
            "INSERT INTO recursive_jobs (id, root_url, child_ids_json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                "job-id",
                "https://example.com/pub/",
                r#"["0000000000000000"]"#,
                "2024-01-01T00:00:00Z"
            ],
        )
        .unwrap();
        let root_url: String = conn
            .query_row(
                "SELECT root_url FROM recursive_jobs WHERE id = 'job-id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(root_url, "https://example.com/pub/");
    }
}
