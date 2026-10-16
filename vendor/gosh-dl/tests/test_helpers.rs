//! Test Helpers
//!
//! This module provides helper functions and builders for creating test data
//! such as torrent files, piece data, and mock configurations.

use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::PathBuf;

/// Builder for creating test torrent data
pub struct TestTorrentBuilder {
    name: String,
    piece_length: u64,
    files: Vec<TestFile>,
}

/// A file in the test torrent
struct TestFile {
    path: PathBuf,
    content: Vec<u8>,
}

impl TestTorrentBuilder {
    /// Create a new test torrent builder
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            piece_length: 16384, // 16KB default
            files: Vec::new(),
        }
    }

    /// Set the piece length
    pub fn piece_length(mut self, length: u64) -> Self {
        self.piece_length = length;
        self
    }

    /// Add a file with specific content
    pub fn add_file(mut self, path: impl Into<PathBuf>, content: Vec<u8>) -> Self {
        self.files.push(TestFile {
            path: path.into(),
            content,
        });
        self
    }

    /// Create a single-file torrent with random content
    pub fn single_file(name: impl Into<String>, size: usize) -> Self {
        let name = name.into();
        let content: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        Self::new(&name).add_file(&name, content)
    }

    /// Create a multi-file torrent with sample files
    pub fn multi_file(name: impl Into<String>) -> Self {
        let name = name.into();
        Self::new(&name)
            .add_file(format!("{}/file1.txt", name), b"Hello World!".to_vec())
            .add_file(format!("{}/file2.txt", name), b"Test content here".to_vec())
            .add_file(
                format!("{}/subdir/file3.bin", name),
                (0..1000).map(|i| (i % 256) as u8).collect(),
            )
    }

    /// Build the torrent, returning the bencoded torrent data and piece hashes
    pub fn build(self) -> (Vec<u8>, Vec<[u8; 20]>) {
        let is_single_file = self.files.len() == 1;

        // Concatenate all file content for piece hashing
        let mut all_content = Vec::new();
        for file in &self.files {
            all_content.extend_from_slice(&file.content);
        }

        // Calculate piece hashes
        let mut piece_hashes = Vec::new();
        let mut offset = 0;
        while offset < all_content.len() {
            let end = (offset + self.piece_length as usize).min(all_content.len());
            let piece_data = &all_content[offset..end];

            let mut hasher = Sha1::new();
            hasher.update(piece_data);
            let hash: [u8; 20] = hasher.finalize().into();
            piece_hashes.push(hash);

            offset = end;
        }

        // Build info dict
        let mut info = HashMap::new();
        info.insert("name".to_string(), BencodeValue::String(self.name.clone()));
        info.insert(
            "piece length".to_string(),
            BencodeValue::Integer(self.piece_length as i64),
        );

        // Concatenate all piece hashes
        let pieces: Vec<u8> = piece_hashes
            .iter()
            .flat_map(|h| h.iter().copied())
            .collect();
        info.insert("pieces".to_string(), BencodeValue::Bytes(pieces));

        if is_single_file {
            let file = &self.files[0];
            info.insert(
                "length".to_string(),
                BencodeValue::Integer(file.content.len() as i64),
            );
        } else {
            let mut files_list = Vec::new();
            for file in &self.files {
                let mut file_dict = HashMap::new();
                file_dict.insert(
                    "length".to_string(),
                    BencodeValue::Integer(file.content.len() as i64),
                );

                // Split path into components
                let path_components: Vec<BencodeValue> = file
                    .path
                    .components()
                    .filter_map(|c| {
                        if let std::path::Component::Normal(s) = c {
                            Some(BencodeValue::String(s.to_string_lossy().to_string()))
                        } else {
                            None
                        }
                    })
                    .collect();
                file_dict.insert("path".to_string(), BencodeValue::List(path_components));

                files_list.push(BencodeValue::Dict(file_dict));
            }
            info.insert("files".to_string(), BencodeValue::List(files_list));
        }

        // Build torrent dict
        let mut torrent = HashMap::new();
        torrent.insert("info".to_string(), BencodeValue::Dict(info));
        torrent.insert(
            "announce".to_string(),
            BencodeValue::String("http://tracker.example.com/announce".to_string()),
        );

        // Encode to bencode
        let encoded = bencode_encode(&BencodeValue::Dict(torrent));

        (encoded, piece_hashes)
    }

    /// Get the piece data for a specific piece index
    pub fn get_piece_data(&self, piece_index: usize) -> Vec<u8> {
        let mut all_content = Vec::new();
        for file in &self.files {
            all_content.extend_from_slice(&file.content);
        }

        let start = piece_index * self.piece_length as usize;
        let end = (start + self.piece_length as usize).min(all_content.len());

        if start >= all_content.len() {
            return Vec::new();
        }

        all_content[start..end].to_vec()
    }
}

/// Simple bencode value for encoding
#[derive(Clone)]
enum BencodeValue {
    Integer(i64),
    String(String),
    Bytes(Vec<u8>),
    List(Vec<BencodeValue>),
    Dict(HashMap<String, BencodeValue>),
}

/// Encode a bencode value
fn bencode_encode(value: &BencodeValue) -> Vec<u8> {
    match value {
        BencodeValue::Integer(i) => format!("i{}e", i).into_bytes(),
        BencodeValue::String(s) => {
            let mut result = format!("{}:", s.len()).into_bytes();
            result.extend_from_slice(s.as_bytes());
            result
        }
        BencodeValue::Bytes(b) => {
            let mut result = format!("{}:", b.len()).into_bytes();
            result.extend_from_slice(b);
            result
        }
        BencodeValue::List(list) => {
            let mut result = vec![b'l'];
            for item in list {
                result.extend(bencode_encode(item));
            }
            result.push(b'e');
            result
        }
        BencodeValue::Dict(dict) => {
            // Keys must be sorted
            let mut keys: Vec<_> = dict.keys().collect();
            keys.sort();

            let mut result = vec![b'd'];
            for key in keys {
                result.extend(bencode_encode(&BencodeValue::String(key.clone())));
                result.extend(bencode_encode(&dict[key]));
            }
            result.push(b'e');
            result
        }
    }
}

/// Calculate the info hash from torrent data
pub fn calculate_info_hash(torrent_data: &[u8]) -> Option<[u8; 20]> {
    // Find the info dict in the bencoded data
    // This is a simplified parser - looks for "4:info" prefix
    let info_key = b"4:infod";
    let info_start = torrent_data
        .windows(info_key.len())
        .position(|w| w == info_key)?;

    // Find the matching end 'e' for the info dict
    let dict_start = info_start + 6; // After "4:info"
    let mut depth = 1;
    let mut pos = dict_start + 1; // After the 'd'

    while pos < torrent_data.len() && depth > 0 {
        match torrent_data[pos] {
            b'd' | b'l' => depth += 1,
            b'e' => depth -= 1,
            b'i' => {
                // Skip integer
                while pos < torrent_data.len() && torrent_data[pos] != b'e' {
                    pos += 1;
                }
            }
            b'0'..=b'9' => {
                // Skip string
                let len_start = pos;
                while pos < torrent_data.len() && torrent_data[pos] != b':' {
                    pos += 1;
                }
                let len_str = std::str::from_utf8(&torrent_data[len_start..pos]).ok()?;
                let len: usize = len_str.parse().ok()?;
                pos += 1 + len; // Skip ':' and string content
                continue;
            }
            _ => {}
        }
        pos += 1;
    }

    if depth != 0 {
        return None;
    }

    // Hash the info dict
    let info_data = &torrent_data[dict_start..pos];
    let mut hasher = Sha1::new();
    hasher.update(info_data);
    Some(hasher.finalize().into())
}

/// Create a temporary directory for test downloads
pub fn create_temp_download_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("Failed to create temp directory")
}

/// Wait for a condition with timeout
pub async fn wait_for<F>(timeout_ms: u64, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(timeout_ms);

    while start.elapsed() < timeout {
        if condition() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    false
}

/// Wait for an async condition with timeout
pub async fn wait_for_async<F, Fut>(timeout_ms: u64, mut condition: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(timeout_ms);

    while start.elapsed() < timeout {
        if condition().await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_file_torrent() {
        let (torrent_data, piece_hashes) =
            TestTorrentBuilder::single_file("test.txt", 32768).build();

        // Should have 2 pieces (32KB / 16KB)
        assert_eq!(piece_hashes.len(), 2);

        // Verify torrent data is valid bencode
        assert!(torrent_data.starts_with(b"d"));
        assert!(torrent_data.ends_with(b"e"));
    }

    #[test]
    fn test_multi_file_torrent() {
        let (torrent_data, piece_hashes) = TestTorrentBuilder::multi_file("test_dir").build();

        // Should have at least 1 piece
        assert!(!piece_hashes.is_empty());

        // Verify torrent data is valid bencode
        assert!(torrent_data.starts_with(b"d"));
        assert!(torrent_data.ends_with(b"e"));
    }

    #[test]
    fn test_custom_piece_length() {
        let builder = TestTorrentBuilder::new("test")
            .piece_length(1024)
            .add_file("test.bin", vec![0u8; 4096]);

        let (_, piece_hashes) = builder.build();

        // Should have 4 pieces (4096 / 1024)
        assert_eq!(piece_hashes.len(), 4);
    }

    #[test]
    fn test_get_piece_data() {
        let builder = TestTorrentBuilder::new("test")
            .piece_length(10)
            .add_file("test.bin", (0..25).collect());

        let piece0 = builder.get_piece_data(0);
        let piece1 = builder.get_piece_data(1);
        let piece2 = builder.get_piece_data(2);

        assert_eq!(piece0, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(piece1, vec![10, 11, 12, 13, 14, 15, 16, 17, 18, 19]);
        assert_eq!(piece2, vec![20, 21, 22, 23, 24]);
    }
}
