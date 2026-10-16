//! Checksum types for download verification
//!
//! Supports MD5 and SHA256 hash verification.

use serde::{Deserialize, Serialize};

/// Supported checksum algorithms
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChecksumAlgorithm {
    /// MD5 hash (128-bit, fast but less secure)
    Md5,
    /// SHA-256 hash (256-bit, more secure)
    Sha256,
}

impl std::fmt::Display for ChecksumAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChecksumAlgorithm::Md5 => write!(f, "MD5"),
            ChecksumAlgorithm::Sha256 => write!(f, "SHA256"),
        }
    }
}

/// Expected checksum for verification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedChecksum {
    /// The hash algorithm to use
    pub algorithm: ChecksumAlgorithm,
    /// The expected hash value (hex-encoded, lowercase)
    pub value: String,
}

impl ExpectedChecksum {
    /// Create an MD5 checksum expectation
    pub fn md5(hex_value: impl Into<String>) -> Self {
        Self {
            algorithm: ChecksumAlgorithm::Md5,
            value: hex_value.into().to_lowercase(),
        }
    }

    /// Create a SHA-256 checksum expectation
    pub fn sha256(hex_value: impl Into<String>) -> Self {
        Self {
            algorithm: ChecksumAlgorithm::Sha256,
            value: hex_value.into().to_lowercase(),
        }
    }

    /// Parse from a string like "md5:abc123" or "sha256:def456"
    pub fn parse(s: &str) -> Option<Self> {
        let (algo, hash) = s.split_once(':')?;
        let algorithm = match algo.to_lowercase().as_str() {
            "md5" => ChecksumAlgorithm::Md5,
            "sha256" | "sha-256" => ChecksumAlgorithm::Sha256,
            _ => return None,
        };
        Some(Self {
            algorithm,
            value: hash.to_lowercase(),
        })
    }
}
