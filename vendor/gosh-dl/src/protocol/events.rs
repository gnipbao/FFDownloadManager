//! Download events
//!
//! Events emitted by the download engine.

use super::types::{DownloadId, DownloadProgress, DownloadState};
use serde::{Deserialize, Serialize};

/// Events emitted by the download engine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DownloadEvent {
    /// Download was added
    Added { id: DownloadId },
    /// Download started
    Started { id: DownloadId },
    /// Progress update
    Progress {
        id: DownloadId,
        progress: DownloadProgress,
    },
    /// State changed
    StateChanged {
        id: DownloadId,
        old_state: DownloadState,
        new_state: DownloadState,
    },
    /// Download completed successfully
    Completed { id: DownloadId },
    /// Download failed
    Failed {
        id: DownloadId,
        error: String,
        retryable: bool,
    },
    /// Download was removed
    Removed { id: DownloadId },
    /// Download was paused
    Paused { id: DownloadId },
    /// Download was resumed
    Resumed { id: DownloadId },
}
