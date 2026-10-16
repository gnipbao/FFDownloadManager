//! Torrent Metainfo Parser
//!
//! This module parses .torrent files (metainfo files) as defined in BEP 3.
//! It extracts all metadata including file information, piece hashes, and
//! tracker URLs.

use sha1::{Digest, Sha1};
use std::path::PathBuf;

use super::bencode::{find_info_dict_bytes, BencodeValue};
use crate::error::{EngineError, ProtocolErrorKind, Result};

/// SHA-1 hash (20 bytes)
pub type Sha1Hash = [u8; 20];

/// Parsed torrent metainfo
#[derive(Debug, Clone)]
pub struct Metainfo {
    /// SHA-1 hash of the bencoded info dictionary
    pub info_hash: Sha1Hash,
    /// The parsed info dictionary
    pub info: Info,
    /// Primary announce URL
    pub announce: Option<String>,
    /// Announce list (BEP 12) - list of tiers, each tier is a list of trackers
    pub announce_list: Vec<Vec<String>>,
    /// Creation timestamp (Unix epoch)
    pub creation_date: Option<i64>,
    /// Comment
    pub comment: Option<String>,
    /// Created by (usually client name)
    pub created_by: Option<String>,
    /// Encoding (e.g., "UTF-8")
    pub encoding: Option<String>,
    /// Web seed URLs (BEP 19 GetRight-style) - direct file URLs
    pub url_list: Vec<String>,
    /// HTTP seeds (BEP 17 Hoffman-style) - seed server URLs
    pub httpseeds: Vec<String>,
}

/// The info dictionary
#[derive(Debug, Clone)]
pub struct Info {
    /// Suggested name for the file or directory
    pub name: String,
    /// Number of bytes per piece
    pub piece_length: u64,
    /// Concatenated SHA-1 hashes of each piece (20 bytes each)
    pub pieces: Vec<Sha1Hash>,
    /// Files in this torrent
    pub files: Vec<FileInfo>,
    /// Total size of all files
    pub total_size: u64,
    /// Whether this is a single-file torrent
    pub is_single_file: bool,
    /// Private flag (BEP 27) - if true, disable DHT/PEX
    pub private: bool,
}

/// Information about a single file in the torrent
#[derive(Debug, Clone)]
pub struct FileInfo {
    /// Path components (for multi-file) or just filename (for single-file)
    pub path: PathBuf,
    /// File size in bytes
    pub length: u64,
    /// Byte offset in the concatenated file stream
    pub offset: u64,
    /// MD5 hash (optional, rarely used)
    pub md5sum: Option<String>,
}

impl Metainfo {
    /// Parse a .torrent file from bytes
    pub fn parse(data: &[u8]) -> Result<Self> {
        // Parse the bencode structure
        let root = BencodeValue::parse_exact(data)?;
        let dict = root.as_dict().ok_or_else(|| {
            EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                "Root must be a dictionary",
            )
        })?;

        // Calculate info_hash from raw bytes
        let info_bytes = find_info_dict_bytes(data)?;
        let info_hash = Self::calculate_info_hash(info_bytes);

        // Parse info dictionary
        let info_value = dict.get(b"info".as_slice()).ok_or_else(|| {
            EngineError::protocol(ProtocolErrorKind::InvalidTorrent, "Missing 'info' key")
        })?;
        let info = Self::parse_info(info_value)?;

        // Parse announce URL
        let announce = dict
            .get(b"announce".as_slice())
            .and_then(|v| v.as_string())
            .map(String::from);

        // Parse announce-list (BEP 12)
        let announce_list = Self::parse_announce_list(dict.get(b"announce-list".as_slice()));

        // Parse optional fields
        let creation_date = dict
            .get(b"creation date".as_slice())
            .and_then(|v| v.as_int());

        let comment = dict
            .get(b"comment".as_slice())
            .and_then(|v| v.as_string())
            .map(String::from);

        let created_by = dict
            .get(b"created by".as_slice())
            .and_then(|v| v.as_string())
            .map(String::from);

        let encoding = dict
            .get(b"encoding".as_slice())
            .and_then(|v| v.as_string())
            .map(String::from);

        // Parse web seeds (BEP 19 - GetRight style)
        let url_list = Self::parse_url_list(dict.get(b"url-list".as_slice()));

        // Parse HTTP seeds (BEP 17 - Hoffman style)
        let httpseeds = Self::parse_url_list(dict.get(b"httpseeds".as_slice()));

        Ok(Metainfo {
            info_hash,
            info,
            announce,
            announce_list,
            creation_date,
            comment,
            created_by,
            encoding,
            url_list,
            httpseeds,
        })
    }

    /// Calculate SHA-1 hash of the info dictionary
    fn calculate_info_hash(info_bytes: &[u8]) -> Sha1Hash {
        let mut hasher = Sha1::new();
        hasher.update(info_bytes);
        hasher.finalize().into()
    }

    /// Parse the info dictionary
    fn parse_info(value: &BencodeValue) -> Result<Info> {
        let dict = value.as_dict().ok_or_else(|| {
            EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                "'info' must be a dictionary",
            )
        })?;

        // Name (required)
        let name = dict
            .get(b"name".as_slice())
            .and_then(|v| v.as_string())
            .ok_or_else(|| {
                EngineError::protocol(ProtocolErrorKind::InvalidTorrent, "Missing 'name' in info")
            })?
            .to_string();

        // Piece length (required)
        let piece_length = dict
            .get(b"piece length".as_slice())
            .and_then(|v| v.as_uint())
            .ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::InvalidTorrent,
                    "Missing or invalid 'piece length'",
                )
            })?;

        // Validate piece length is non-zero (prevents division by zero)
        if piece_length == 0 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                "Invalid 'piece length': must be greater than zero",
            ));
        }

        // Validate piece length is reasonable (16 KiB to 64 MiB)
        // BitTorrent typically uses 256 KiB to 16 MiB piece sizes
        const MIN_PIECE_LENGTH: u64 = 16 * 1024; // 16 KiB
        const MAX_PIECE_LENGTH: u64 = 64 * 1024 * 1024; // 64 MiB
        if !(MIN_PIECE_LENGTH..=MAX_PIECE_LENGTH).contains(&piece_length) {
            tracing::warn!(
                "Unusual piece length {} (typical range: {} - {})",
                piece_length,
                MIN_PIECE_LENGTH,
                MAX_PIECE_LENGTH
            );
            // Don't reject, just warn - some old torrents have unusual sizes
        }

        // Pieces (required) - concatenated 20-byte SHA-1 hashes
        let pieces_bytes = dict
            .get(b"pieces".as_slice())
            .and_then(|v| v.as_bytes())
            .ok_or_else(|| {
                EngineError::protocol(ProtocolErrorKind::InvalidTorrent, "Missing 'pieces'")
            })?;

        if pieces_bytes.len() % 20 != 0 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                format!(
                    "Invalid pieces length: {} (not a multiple of 20)",
                    pieces_bytes.len()
                ),
            ));
        }

        let pieces: Vec<Sha1Hash> = pieces_bytes
            .chunks_exact(20)
            .map(|chunk| {
                let mut hash = [0u8; 20];
                hash.copy_from_slice(chunk);
                hash
            })
            .collect();

        // Private flag (BEP 27)
        let private = dict
            .get(b"private".as_slice())
            .and_then(|v| v.as_int())
            .map(|v| v == 1)
            .unwrap_or(false);

        // Determine if single-file or multi-file
        let (files, total_size, is_single_file) = if dict.contains_key(b"files".as_slice()) {
            // Multi-file mode
            let files_value = dict.get(b"files".as_slice()).unwrap();
            let (files, total_size) = Self::parse_files(files_value)?;
            (files, total_size, false)
        } else {
            // Single-file mode
            let length = dict
                .get(b"length".as_slice())
                .and_then(|v| v.as_uint())
                .ok_or_else(|| {
                    EngineError::protocol(
                        ProtocolErrorKind::InvalidTorrent,
                        "Missing 'length' for single-file torrent",
                    )
                })?;

            let md5sum = dict
                .get(b"md5sum".as_slice())
                .and_then(|v| v.as_string())
                .map(String::from);

            let file = FileInfo {
                path: PathBuf::from(&name),
                length,
                offset: 0,
                md5sum,
            };

            (vec![file], length, true)
        };

        // Verify piece count matches total size
        let expected_pieces = total_size.div_ceil(piece_length); // Ceiling division
        if pieces.len() as u64 != expected_pieces {
            return Err(EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                format!(
                    "Piece count mismatch: have {}, expected {} for {} bytes with {} byte pieces",
                    pieces.len(),
                    expected_pieces,
                    total_size,
                    piece_length
                ),
            ));
        }

        Ok(Info {
            name,
            piece_length,
            pieces,
            files,
            total_size,
            is_single_file,
            private,
        })
    }

    /// Parse the files list for multi-file torrents
    fn parse_files(value: &BencodeValue) -> Result<(Vec<FileInfo>, u64)> {
        let files_list = value.as_list().ok_or_else(|| {
            EngineError::protocol(ProtocolErrorKind::InvalidTorrent, "'files' must be a list")
        })?;

        let mut files = Vec::new();
        let mut offset = 0u64;

        for file_value in files_list {
            let file_dict = file_value.as_dict().ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::InvalidTorrent,
                    "File entry must be a dictionary",
                )
            })?;

            let length = file_dict
                .get(b"length".as_slice())
                .and_then(|v| v.as_uint())
                .ok_or_else(|| {
                    EngineError::protocol(
                        ProtocolErrorKind::InvalidTorrent,
                        "Missing 'length' in file entry",
                    )
                })?;

            let path_value = file_dict.get(b"path".as_slice()).ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::InvalidTorrent,
                    "Missing 'path' in file entry",
                )
            })?;

            let path_list = path_value.as_list().ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::InvalidTorrent,
                    "'path' must be a list of strings",
                )
            })?;

            let mut path = PathBuf::new();
            for component in path_list {
                let component_str = component.as_string().ok_or_else(|| {
                    EngineError::protocol(
                        ProtocolErrorKind::InvalidTorrent,
                        "Path component must be a string",
                    )
                })?;
                path.push(component_str);
            }

            let md5sum = file_dict
                .get(b"md5sum".as_slice())
                .and_then(|v| v.as_string())
                .map(String::from);

            files.push(FileInfo {
                path,
                length,
                offset,
                md5sum,
            });

            offset += length;
        }

        Ok((files, offset))
    }

    /// Parse announce-list (BEP 12)
    fn parse_announce_list(value: Option<&BencodeValue>) -> Vec<Vec<String>> {
        let Some(value) = value else {
            return Vec::new();
        };

        let Some(tiers) = value.as_list() else {
            return Vec::new();
        };

        tiers
            .iter()
            .filter_map(|tier| {
                tier.as_list().map(|urls| {
                    urls.iter()
                        .filter_map(|url| url.as_string().map(String::from))
                        .collect()
                })
            })
            .filter(|tier: &Vec<String>| !tier.is_empty())
            .collect()
    }

    /// Parse url-list or httpseeds field (BEP 19/BEP 17)
    ///
    /// Handles both single string and list-of-strings formats.
    fn parse_url_list(value: Option<&BencodeValue>) -> Vec<String> {
        let Some(value) = value else {
            return Vec::new();
        };

        match value {
            // Single URL string
            BencodeValue::Bytes(bytes) => {
                if let Ok(s) = std::str::from_utf8(bytes) {
                    if s.starts_with("http://") || s.starts_with("https://") {
                        return vec![s.to_string()];
                    }
                }
                Vec::new()
            }
            // List of URL strings
            BencodeValue::List(list) => list
                .iter()
                .filter_map(|item| item.as_string())
                .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
                .map(String::from)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Get the info_hash as a hex string
    pub fn info_hash_hex(&self) -> String {
        self.info_hash
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    /// Get the info_hash URL-encoded (for tracker requests)
    pub fn info_hash_urlencoded(&self) -> String {
        self.info_hash
            .iter()
            .map(|b| format!("%{:02X}", b))
            .collect()
    }

    /// Get the piece hash for a given piece index
    pub fn piece_hash(&self, index: usize) -> Option<&Sha1Hash> {
        self.info.pieces.get(index)
    }

    /// Get the byte range for a piece
    pub fn piece_range(&self, index: usize) -> Option<(u64, u64)> {
        if index >= self.info.pieces.len() {
            return None;
        }

        let start = index as u64 * self.info.piece_length;
        let end = std::cmp::min(start + self.info.piece_length, self.info.total_size);

        Some((start, end))
    }

    /// Get the length of a piece (last piece may be shorter)
    pub fn piece_length(&self, index: usize) -> Option<u64> {
        self.piece_range(index).map(|(start, end)| end - start)
    }

    /// Get all trackers (combining announce and announce_list)
    pub fn all_trackers(&self) -> Vec<String> {
        let mut trackers = Vec::new();

        // Add primary announce URL
        if let Some(ref announce) = self.announce {
            trackers.push(announce.clone());
        }

        // Add announce-list URLs (flattened)
        for tier in &self.announce_list {
            for url in tier {
                if !trackers.contains(url) {
                    trackers.push(url.clone());
                }
            }
        }

        trackers
    }

    /// Get all web seed URLs (combining url-list and httpseeds)
    ///
    /// Returns deduplicated list of HTTP URLs that can serve torrent data.
    pub fn all_webseeds(&self) -> Vec<String> {
        let mut seeds = self.url_list.clone();

        // Add httpseeds, avoiding duplicates
        for seed in &self.httpseeds {
            if !seeds.contains(seed) {
                seeds.push(seed.clone());
            }
        }

        seeds
    }

    /// Check if this torrent has any web seeds
    pub fn has_webseeds(&self) -> bool {
        !self.url_list.is_empty() || !self.httpseeds.is_empty()
    }

    /// Get files that overlap with a given piece
    ///
    /// Returns list of (file_index, file_offset, length) tuples
    pub fn files_for_piece(&self, piece_index: usize) -> Vec<(usize, u64, u64)> {
        let Some((piece_start, piece_end)) = self.piece_range(piece_index) else {
            return Vec::new();
        };

        let mut result = Vec::new();

        for (file_idx, file) in self.info.files.iter().enumerate() {
            let file_start = file.offset;
            let file_end = file.offset + file.length;

            // Check for overlap
            if file_start >= piece_end || file_end <= piece_start {
                continue;
            }

            // Calculate overlap range
            let overlap_start = std::cmp::max(piece_start, file_start);
            let overlap_end = std::cmp::min(piece_end, file_end);

            // File offset is relative to file start
            let file_offset = overlap_start - file_start;
            let length = overlap_end - overlap_start;

            result.push((file_idx, file_offset, length));
        }

        result
    }
}

impl Info {
    /// Get the total number of pieces
    pub fn num_pieces(&self) -> usize {
        self.pieces.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_torrent() -> Vec<u8> {
        // Create a minimal valid torrent
        // This is a simplified torrent with:
        // - Single file "test.txt" of 100 bytes
        // - Piece length of 32768 (32KB)
        // - 1 piece hash (20 bytes of zeros)
        let pieces = vec![0u8; 20]; // One piece hash
        let pieces_str = format!("6:pieces{}:", pieces.len());

        let mut data = Vec::new();
        data.extend_from_slice(b"d");
        data.extend_from_slice(b"8:announce35:http://tracker.example.com/announce");
        data.extend_from_slice(b"4:infod");
        data.extend_from_slice(b"6:lengthi100e");
        data.extend_from_slice(b"4:name8:test.txt");
        data.extend_from_slice(b"12:piece lengthi32768e");
        data.extend_from_slice(pieces_str.as_bytes());
        data.extend_from_slice(&pieces);
        data.extend_from_slice(b"ee");

        data
    }

    #[test]
    fn test_parse_single_file_torrent() {
        let data = create_test_torrent();
        let metainfo = Metainfo::parse(&data).unwrap();

        assert_eq!(metainfo.info.name, "test.txt");
        assert_eq!(metainfo.info.piece_length, 32768);
        assert_eq!(metainfo.info.total_size, 100);
        assert_eq!(metainfo.info.pieces.len(), 1);
        assert!(metainfo.info.is_single_file);
        assert_eq!(metainfo.info.files.len(), 1);
        assert_eq!(metainfo.info.files[0].length, 100);
        assert_eq!(
            metainfo.announce,
            Some("http://tracker.example.com/announce".to_string())
        );
    }

    #[test]
    fn test_info_hash_calculation() {
        let data = create_test_torrent();
        let metainfo = Metainfo::parse(&data).unwrap();

        // Info hash should be 40 hex characters
        let hex = metainfo.info_hash_hex();
        assert_eq!(hex.len(), 40);

        // URL-encoded should be 60 characters (20 bytes * 3 chars each)
        let urlencoded = metainfo.info_hash_urlencoded();
        assert_eq!(urlencoded.len(), 60);
    }

    #[test]
    fn test_piece_range() {
        let data = create_test_torrent();
        let metainfo = Metainfo::parse(&data).unwrap();

        // Single piece covering the whole file
        let range = metainfo.piece_range(0).unwrap();
        assert_eq!(range.0, 0);
        assert_eq!(range.1, 100); // File is only 100 bytes

        // Non-existent piece
        assert!(metainfo.piece_range(1).is_none());
    }

    #[test]
    fn test_piece_length() {
        let data = create_test_torrent();
        let metainfo = Metainfo::parse(&data).unwrap();

        // Last piece is smaller than piece_length
        let len = metainfo.piece_length(0).unwrap();
        assert_eq!(len, 100); // File is only 100 bytes
    }

    #[test]
    fn test_files_for_piece() {
        let data = create_test_torrent();
        let metainfo = Metainfo::parse(&data).unwrap();

        let files = metainfo.files_for_piece(0);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, 0); // File index
        assert_eq!(files[0].1, 0); // File offset
        assert_eq!(files[0].2, 100); // Length
    }

    #[test]
    fn test_invalid_torrent() {
        // Missing info dict
        let data = b"d8:announce10:http://fooe";
        assert!(Metainfo::parse(data).is_err());

        // Invalid pieces length
        let data = b"d4:infod6:lengthi100e4:name4:test12:piece lengthi1024e6:pieces5:12345ee";
        assert!(Metainfo::parse(data).is_err());
    }
}
