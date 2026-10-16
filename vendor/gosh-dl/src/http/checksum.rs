//! Checksum verification for downloaded files
//!
//! Supports MD5 and SHA256 hash verification to ensure download integrity.

use crate::error::{EngineError, ProtocolErrorKind, Result, StorageErrorKind};
use md5::{Digest as Md5Digest, Md5};
use sha2::Sha256;
use std::path::Path;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

// Re-export protocol types for backward compatibility
pub use crate::protocol::{ChecksumAlgorithm, ExpectedChecksum};

/// Verify file checksum against expected value
///
/// Returns `Ok(true)` if checksums match, `Ok(false)` if they don't.
/// Returns `Err` if the file cannot be read.
pub async fn verify_checksum(path: &Path, expected: &ExpectedChecksum) -> Result<bool> {
    let computed = compute_checksum(path, expected.algorithm).await?;
    Ok(computed.to_lowercase() == expected.value.to_lowercase())
}

/// Compute file checksum using specified algorithm
///
/// Returns the hex-encoded hash value.
pub async fn compute_checksum(path: &Path, algorithm: ChecksumAlgorithm) -> Result<String> {
    let mut file = File::open(path).await.map_err(|e| {
        EngineError::storage(
            StorageErrorKind::Io,
            path,
            format!("Failed to open file for checksum: {}", e),
        )
    })?;

    // 64KB buffer for efficient reading
    let mut buffer = vec![0u8; 64 * 1024];

    match algorithm {
        ChecksumAlgorithm::Md5 => {
            let mut hasher = Md5::new();
            loop {
                let n = file.read(&mut buffer).await.map_err(|e| {
                    EngineError::storage(
                        StorageErrorKind::Io,
                        path,
                        format!("Failed to read file for checksum: {}", e),
                    )
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            Ok(hex::encode(hasher.finalize()))
        }
        ChecksumAlgorithm::Sha256 => {
            let mut hasher = Sha256::new();
            loop {
                let n = file.read(&mut buffer).await.map_err(|e| {
                    EngineError::storage(
                        StorageErrorKind::Io,
                        path,
                        format!("Failed to read file for checksum: {}", e),
                    )
                })?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            Ok(hex::encode(hasher.finalize()))
        }
    }
}

/// Create a checksum mismatch error
pub fn checksum_mismatch_error(expected: &str, actual: &str) -> EngineError {
    EngineError::Protocol {
        kind: ProtocolErrorKind::HashMismatch,
        message: format!(
            "Checksum verification failed: expected {}, got {}",
            expected, actual
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_md5_checksum() {
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, b"Hello, World!").unwrap();

        let computed = compute_checksum(file.path(), ChecksumAlgorithm::Md5)
            .await
            .unwrap();
        // MD5 of "Hello, World!" is 65a8e27d8879283831b664bd8b7f0ad4
        assert_eq!(computed, "65a8e27d8879283831b664bd8b7f0ad4");
    }

    #[tokio::test]
    async fn test_sha256_checksum() {
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, b"Hello, World!").unwrap();

        let computed = compute_checksum(file.path(), ChecksumAlgorithm::Sha256)
            .await
            .unwrap();
        // SHA256 of "Hello, World!" is dffd6021bb2bd5b0af676290809ec3a53191dd81c7f70a4b28688a362182986f
        assert_eq!(
            computed,
            "dffd6021bb2bd5b0af676290809ec3a53191dd81c7f70a4b28688a362182986f"
        );
    }

    #[tokio::test]
    async fn test_verify_checksum() {
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut file, b"Hello, World!").unwrap();

        let expected = ExpectedChecksum::md5("65a8e27d8879283831b664bd8b7f0ad4");
        assert!(verify_checksum(file.path(), &expected).await.unwrap());

        let wrong = ExpectedChecksum::md5("0000000000000000000000000000000");
        assert!(!verify_checksum(file.path(), &wrong).await.unwrap());
    }

    #[test]
    fn test_parse_checksum() {
        let md5 = ExpectedChecksum::parse("md5:abc123").unwrap();
        assert_eq!(md5.algorithm, ChecksumAlgorithm::Md5);
        assert_eq!(md5.value, "abc123");

        let sha = ExpectedChecksum::parse("sha256:def456").unwrap();
        assert_eq!(sha.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(sha.value, "def456");

        assert!(ExpectedChecksum::parse("invalid").is_none());
    }
}
