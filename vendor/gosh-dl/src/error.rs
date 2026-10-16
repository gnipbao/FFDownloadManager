//! Typed error hierarchy for gosh-dl
//!
//! [`EngineError`] is the primary error type returned by all public API methods.
//! It includes rich context (error kinds, retryability) for Rust consumers to
//! pattern-match on.
//!
//! For serialization boundaries (JSON-RPC, gRPC, IPC), convert to
//! [`crate::ProtocolError`] via the `From` impl — it carries only
//! string messages and is `Serialize`/`Deserialize`.
//!
//! # Error handling example
//!
//! ```rust,no_run
//! use gosh_dl::{EngineError, NetworkErrorKind};
//!
//! fn handle_error(err: EngineError) {
//!     if err.is_retryable() {
//!         eprintln!("Retryable error: {err}");
//!     } else if err.is_not_found() {
//!         eprintln!("Download not found");
//!     } else if let EngineError::Network { kind, .. } = &err {
//!         match kind {
//!             NetworkErrorKind::Timeout => eprintln!("Timed out"),
//!             _ => eprintln!("Network error: {err}"),
//!         }
//!     }
//! }
//! ```

use std::path::PathBuf;
use thiserror::Error;

/// Main error type for the download engine
#[derive(Debug, Error)]
pub enum EngineError {
    /// Network-related errors (connection, timeout, DNS, etc.)
    #[error("Network error: {message}")]
    Network {
        kind: NetworkErrorKind,
        message: String,
        retryable: bool,
    },

    /// Storage/filesystem errors
    #[error("Storage error at {path:?}: {message}")]
    Storage {
        kind: StorageErrorKind,
        path: PathBuf,
        message: String,
    },

    /// Protocol-level errors (HTTP, BitTorrent)
    #[error("Protocol error: {message}")]
    Protocol {
        kind: ProtocolErrorKind,
        message: String,
    },

    /// Invalid input from user
    #[error("Invalid input for '{field}': {message}")]
    InvalidInput {
        field: &'static str,
        message: String,
    },

    /// Resource limits exceeded
    #[error("Resource limit exceeded: {resource} (limit: {limit})")]
    ResourceLimit {
        resource: &'static str,
        limit: usize,
    },

    /// Download not found
    #[error("Download not found: {0}")]
    NotFound(String),

    /// Download already exists
    #[error("Download already exists: {0}")]
    AlreadyExists(String),

    /// Invalid state transition
    #[error("Invalid state: cannot {action} while {current_state}")]
    InvalidState {
        action: &'static str,
        current_state: String,
    },

    /// Engine is shutting down
    #[error("Engine is shutting down")]
    Shutdown,

    /// Database error
    #[error("Database error: {0}")]
    Database(String),

    /// Internal error (bug)
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Network error subtypes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkErrorKind {
    /// DNS resolution failed
    DnsResolution,
    /// Connection refused
    ConnectionRefused,
    /// Connection reset
    ConnectionReset,
    /// Connection timeout
    Timeout,
    /// TLS/SSL error
    Tls,
    /// Server returned error status
    HttpStatus(u16),
    /// Server not reachable
    Unreachable,
    /// Too many redirects
    TooManyRedirects,
    /// Other network error
    Other,
}

/// Storage error subtypes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageErrorKind {
    /// File/directory not found
    NotFound,
    /// Permission denied
    PermissionDenied,
    /// Disk full
    DiskFull,
    /// Path is outside allowed directory (security)
    PathTraversal,
    /// File already exists
    AlreadyExists,
    /// Invalid path
    InvalidPath,
    /// I/O error
    Io,
}

/// Protocol error subtypes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolErrorKind {
    /// Invalid URL
    InvalidUrl,
    /// Server doesn't support range requests
    RangeNotSupported,
    /// Invalid HTTP response
    InvalidResponse,
    /// Invalid torrent file
    InvalidTorrent,
    /// Invalid magnet URI
    InvalidMagnet,
    /// Piece hash verification failed
    HashMismatch,
    /// Tracker error
    TrackerError,
    /// Peer protocol violation
    PeerProtocol,
    /// Bencode parsing error
    BencodeParse,
    /// Peer Exchange (PEX) error
    PexError,
    /// DHT error
    DhtError,
    /// Local Peer Discovery error
    LpdError,
    /// Metadata fetch error (BEP 9)
    MetadataError,
}

impl EngineError {
    /// Check if this error is retryable
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network { retryable, .. } => *retryable,
            Self::Storage { kind, .. } => matches!(kind, StorageErrorKind::Io),
            Self::Protocol { kind, .. } => matches!(
                kind,
                ProtocolErrorKind::TrackerError | ProtocolErrorKind::PeerProtocol
            ),
            _ => false,
        }
    }

    /// Create a network error
    pub fn network(kind: NetworkErrorKind, message: impl Into<String>) -> Self {
        let retryable = matches!(
            kind,
            NetworkErrorKind::Timeout
                | NetworkErrorKind::ConnectionReset
                | NetworkErrorKind::Unreachable
                | NetworkErrorKind::HttpStatus(408)
                | NetworkErrorKind::HttpStatus(429)
                | NetworkErrorKind::HttpStatus(500..=599)
        );
        Self::Network {
            kind,
            message: message.into(),
            retryable,
        }
    }

    /// Create a storage error
    pub fn storage(
        kind: StorageErrorKind,
        path: impl Into<PathBuf>,
        message: impl Into<String>,
    ) -> Self {
        Self::Storage {
            kind,
            path: path.into(),
            message: message.into(),
        }
    }

    /// Create a protocol error
    pub fn protocol(kind: ProtocolErrorKind, message: impl Into<String>) -> Self {
        Self::Protocol {
            kind,
            message: message.into(),
        }
    }

    /// Create an invalid input error
    pub fn invalid_input(field: &'static str, message: impl Into<String>) -> Self {
        Self::InvalidInput {
            field,
            message: message.into(),
        }
    }

    /// Check if this is a "not found" error
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }

    /// Check if this is a network error
    pub fn is_network(&self) -> bool {
        matches!(self, Self::Network { .. })
    }

    /// Check if this is a shutdown error
    pub fn is_shutdown(&self) -> bool {
        matches!(self, Self::Shutdown)
    }
}

/// Result type alias for engine operations
pub type Result<T> = std::result::Result<T, EngineError>;

// Implement From traits for common error types

impl From<std::io::Error> for EngineError {
    fn from(err: std::io::Error) -> Self {
        use std::io::ErrorKind;
        let kind = match err.kind() {
            ErrorKind::NotFound => StorageErrorKind::NotFound,
            ErrorKind::PermissionDenied => StorageErrorKind::PermissionDenied,
            ErrorKind::AlreadyExists => StorageErrorKind::AlreadyExists,
            _ => StorageErrorKind::Io,
        };
        Self::Storage {
            kind,
            path: PathBuf::new(),
            message: err.to_string(),
        }
    }
}

#[cfg(any(feature = "http", feature = "torrent"))]
impl From<reqwest::Error> for EngineError {
    fn from(err: reqwest::Error) -> Self {
        let kind = if err.is_timeout() {
            NetworkErrorKind::Timeout
        } else if err.is_connect() {
            NetworkErrorKind::ConnectionRefused
        } else if err.is_redirect() {
            NetworkErrorKind::TooManyRedirects
        } else if err.is_body() || err.is_decode() || interrupted_request(&err) {
            // Retry an interrupted body or a socket closed before headers arrive.
            NetworkErrorKind::ConnectionReset
        } else if let Some(status) = err.status() {
            NetworkErrorKind::HttpStatus(status.as_u16())
        } else {
            NetworkErrorKind::Other
        };

        // Use the standard constructor so retryability is computed consistently
        Self::network(kind, err.to_string())
    }
}

#[cfg(any(feature = "http", feature = "torrent"))]
fn interrupted_request(err: &reqwest::Error) -> bool {
    use std::{error::Error as _, io::ErrorKind};

    if !err.is_request() {
        return false;
    }
    let mut source = err.source();
    while let Some(error) = source {
        if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
                    | ErrorKind::NotConnected
            )
        }) {
            return true;
        }
        source = error.source();
    }
    false
}

impl From<url::ParseError> for EngineError {
    fn from(err: url::ParseError) -> Self {
        Self::Protocol {
            kind: ProtocolErrorKind::InvalidUrl,
            message: err.to_string(),
        }
    }
}

#[cfg(feature = "storage")]
impl From<rusqlite::Error> for EngineError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Database(err.to_string())
    }
}

impl From<serde_json::Error> for EngineError {
    fn from(err: serde_json::Error) -> Self {
        Self::Internal(format!("JSON error: {}", err))
    }
}

impl From<tokio::sync::broadcast::error::SendError<crate::protocol::DownloadEvent>>
    for EngineError
{
    fn from(_: tokio::sync::broadcast::error::SendError<crate::protocol::DownloadEvent>) -> Self {
        Self::Shutdown
    }
}

// Conversion from EngineError to ProtocolError for the public API boundary
impl From<EngineError> for crate::protocol::ProtocolError {
    fn from(e: EngineError) -> Self {
        use crate::protocol::ProtocolError;
        match e {
            EngineError::NotFound(id) => ProtocolError::NotFound { id },
            EngineError::InvalidState {
                action,
                current_state,
            } => ProtocolError::InvalidState {
                action: action.to_string(),
                current_state,
            },
            EngineError::InvalidInput { field, message } => ProtocolError::InvalidInput {
                field: field.to_string(),
                message,
            },
            EngineError::Network {
                message, retryable, ..
            } => ProtocolError::Network { message, retryable },
            EngineError::Storage { message, .. } => ProtocolError::Storage { message },
            EngineError::Protocol { message, .. } => ProtocolError::Network {
                message,
                retryable: false,
            },
            EngineError::Shutdown => ProtocolError::Shutdown,
            EngineError::AlreadyExists(id) => ProtocolError::InvalidInput {
                field: "id".to_string(),
                message: format!("Download already exists: {}", id),
            },
            EngineError::ResourceLimit { resource, limit } => ProtocolError::InvalidInput {
                field: resource.to_string(),
                message: format!("Resource limit exceeded (limit: {})", limit),
            },
            EngineError::Database(msg) => ProtocolError::Storage { message: msg },
            EngineError::Internal(msg) => ProtocolError::Internal { message: msg },
        }
    }
}
