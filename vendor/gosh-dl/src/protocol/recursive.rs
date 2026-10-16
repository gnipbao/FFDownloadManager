//! Recursive HTTP directory mirroring types.
//!
//! These types describe discovery and orchestration for recursive HTTP/HTTPS
//! mirroring. They are intentionally additive and do not change the existing
//! single-resource HTTP download model.

use super::types::DownloadId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Options that control recursive HTTP/HTTPS discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecursiveOptions {
    /// Maximum traversal depth, starting at the root page.
    pub max_depth: usize,
    /// Restrict discovery to the same host as the root URL.
    pub same_host_only: bool,
    /// Optional path prefix that discovered URLs must remain under.
    pub allowed_prefix: Option<String>,
    /// Glob-like include patterns evaluated against discovered paths.
    #[serde(default)]
    pub include_patterns: Vec<String>,
    /// Glob-like exclude patterns evaluated against discovered paths.
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    /// Preserve the remote relative directory structure locally.
    pub preserve_paths: bool,
    /// Permit overwriting colliding local files during manifest construction.
    pub overwrite_existing: bool,
    /// Abort queued/active sibling child downloads after the first child failure.
    pub fail_fast: bool,
    /// Maximum number of discovery requests allowed to run concurrently.
    pub max_discovery_concurrency: usize,
}

impl Default for RecursiveOptions {
    fn default() -> Self {
        Self {
            max_depth: 16,
            same_host_only: true,
            allowed_prefix: None,
            include_patterns: Vec::new(),
            exclude_patterns: Vec::new(),
            preserve_paths: true,
            overwrite_existing: false,
            fail_fast: false,
            max_discovery_concurrency: 4,
        }
    }
}

/// A discovered file candidate in a recursive crawl manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecursiveEntry {
    /// Fully qualified remote URL for the file.
    pub url: String,
    /// Relative local path under the configured download root.
    pub relative_path: PathBuf,
    /// Optional size hint from discovery metadata.
    pub size_hint: Option<u64>,
}

/// Discovery result for a recursive HTTP/HTTPS root.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecursiveManifest {
    /// Root URL used to build the manifest.
    pub root_url: String,
    /// Discovered file entries.
    #[serde(default)]
    pub entries: Vec<RecursiveEntry>,
}

/// Result of expanding a recursive job into child HTTP downloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecursiveJob {
    /// Root URL used for discovery.
    pub root_url: String,
    /// Child HTTP download IDs created from the manifest.
    #[serde(default)]
    pub child_ids: Vec<DownloadId>,
}

/// Persisted recursive job record tracked independently from child downloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackedRecursiveJob {
    /// Stable identifier for the recursive job record.
    pub id: Uuid,
    /// Root URL used for discovery.
    pub root_url: String,
    /// Child HTTP download IDs created from the manifest.
    #[serde(default)]
    pub child_ids: Vec<DownloadId>,
    /// Timestamp when the recursive job was created.
    pub created_at: DateTime<Utc>,
}

impl TrackedRecursiveJob {
    /// Project the tracked record into the simpler child-based job view.
    pub fn as_job(&self) -> RecursiveJob {
        RecursiveJob {
            root_url: self.root_url.clone(),
            child_ids: self.child_ids.clone(),
        }
    }
}

/// Aggregate state derived from a recursive job's child downloads.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecursiveJobState {
    /// The job has no child downloads.
    Empty,
    /// All known children are queued.
    Queued,
    /// One or more children are actively connecting/downloading.
    Running,
    /// The job is currently stopped with paused children and no active work.
    Paused,
    /// All children completed successfully.
    Completed,
    /// All children failed or were removed.
    Failed,
    /// The job has a mix of successful, failed, removed, or still-pending children.
    Partial,
}

/// Aggregate child counts and byte counters for a recursive job.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecursiveJobProgress {
    /// Total child IDs tracked by the job.
    pub total_children: usize,
    /// Children currently queued.
    pub queued_children: usize,
    /// Children actively connecting or downloading.
    pub active_children: usize,
    /// Children currently paused.
    pub paused_children: usize,
    /// Children completed successfully.
    pub completed_children: usize,
    /// Children currently failed.
    pub failed_children: usize,
    /// Child IDs no longer present in the engine.
    pub missing_children: usize,
    /// Sum of completed child bytes.
    pub completed_size: u64,
    /// Sum of child total sizes when all present children know their length.
    pub total_size: Option<u64>,
}

/// Aggregate status for a recursive job derived from its child downloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecursiveJobStatus {
    /// Root URL used for discovery.
    pub root_url: String,
    /// Child IDs tracked by this recursive job.
    #[serde(default)]
    pub child_ids: Vec<DownloadId>,
    /// Aggregate state derived from the child downloads.
    pub state: RecursiveJobState,
    /// Aggregate child counts and byte counters.
    pub progress: RecursiveJobProgress,
}

/// Events emitted on the dedicated recursive job event stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RecursiveJobEvent {
    /// A tracked recursive job was created or restored into the engine.
    Added {
        job: TrackedRecursiveJob,
        status: RecursiveJobStatus,
    },
    /// Aggregate recursive job state changed because one or more children changed.
    Updated {
        job: TrackedRecursiveJob,
        status: RecursiveJobStatus,
    },
    /// A tracked recursive job record was explicitly removed.
    Removed { id: Uuid },
}
