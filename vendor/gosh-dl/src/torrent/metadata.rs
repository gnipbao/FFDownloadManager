//! Metadata fetching for magnet links (BEP 9).
//!
//! When downloading via a magnet link, we only have the info_hash but not
//! the full torrent metadata. BEP 9 defines the ut_metadata extension that
//! allows fetching metadata from peers.
//!
//! The metadata is split into 16KB pieces and exchanged via extended messages.

use std::collections::HashMap;
use std::time::Duration;

use sha1::{Digest, Sha1};
use tokio::sync::RwLock;

use crate::error::{EngineError, ProtocolErrorKind, Result};
use crate::torrent::bencode::BencodeValue;
use crate::torrent::metainfo::{Metainfo, Sha1Hash};

/// Size of metadata pieces (16KB).
pub const METADATA_PIECE_SIZE: usize = 16 * 1024;

/// Upper bound on peer-claimed metadata size. Info dicts for even enormous
/// multi-file torrents are a few MB; an unbounded claim lets one malicious
/// Data message drive allocations of billions of piece indices.
pub const MAX_METADATA_SIZE: usize = 16 * 1024 * 1024;

/// Extension name for ut_metadata in BEP 10 handshake.
pub const METADATA_EXTENSION_NAME: &str = "ut_metadata";

/// Our extension ID for ut_metadata.
pub const OUR_METADATA_EXTENSION_ID: u8 = 2;

/// Metadata message types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataMessageType {
    /// Request a piece of metadata.
    Request = 0,
    /// Data response with a piece of metadata.
    Data = 1,
    /// Reject - peer doesn't have metadata.
    Reject = 2,
}

impl MetadataMessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Request),
            1 => Some(Self::Data),
            2 => Some(Self::Reject),
            _ => None,
        }
    }
}

/// A metadata request/response message.
#[derive(Debug, Clone)]
pub struct MetadataMessage {
    /// Message type.
    pub msg_type: MetadataMessageType,
    /// Piece index.
    pub piece: usize,
    /// Total metadata size (only in Data messages).
    pub total_size: Option<usize>,
    /// Piece data (only in Data messages).
    pub data: Option<Vec<u8>>,
}

impl MetadataMessage {
    /// Create a request message.
    pub fn request(piece: usize) -> Self {
        Self {
            msg_type: MetadataMessageType::Request,
            piece,
            total_size: None,
            data: None,
        }
    }

    /// Create a data response message.
    pub fn data(piece: usize, total_size: usize, data: Vec<u8>) -> Self {
        Self {
            msg_type: MetadataMessageType::Data,
            piece,
            total_size: Some(total_size),
            data: Some(data),
        }
    }

    /// Create a reject message.
    pub fn reject(piece: usize) -> Self {
        Self {
            msg_type: MetadataMessageType::Reject,
            piece,
            total_size: None,
            data: None,
        }
    }

    /// Encode the message to bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut dict = std::collections::BTreeMap::new();
        dict.insert(
            b"msg_type".to_vec(),
            BencodeValue::Integer(self.msg_type as i64),
        );
        dict.insert(b"piece".to_vec(), BencodeValue::Integer(self.piece as i64));

        if let Some(size) = self.total_size {
            dict.insert(b"total_size".to_vec(), BencodeValue::Integer(size as i64));
        }

        let mut encoded = BencodeValue::Dict(dict).encode();

        // For data messages, append the raw data after the dict
        if let Some(ref data) = self.data {
            encoded.extend_from_slice(data);
        }

        encoded
    }

    /// Parse a metadata message from bytes.
    pub fn parse(data: &[u8]) -> Result<Self> {
        // The message is a bencoded dict optionally followed by raw piece data
        let parse_result = BencodeValue::parse(data)?;
        let consumed = data.len() - parse_result.remaining.len();

        let dict = parse_result.value.as_dict().ok_or_else(|| {
            EngineError::protocol(
                ProtocolErrorKind::MetadataError,
                "Metadata message must be a dict",
            )
        })?;

        let msg_type = dict
            .get(b"msg_type".as_slice())
            .and_then(|v: &BencodeValue| v.as_uint())
            .and_then(|v| MetadataMessageType::from_u8(v as u8))
            .ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::MetadataError,
                    "Invalid or missing msg_type",
                )
            })?;

        let piece = dict
            .get(b"piece".as_slice())
            .and_then(|v: &BencodeValue| v.as_uint())
            .map(|v| v as usize)
            .ok_or_else(|| {
                EngineError::protocol(ProtocolErrorKind::MetadataError, "Invalid or missing piece")
            })?;

        let total_size = dict
            .get(b"total_size".as_slice())
            .and_then(|v: &BencodeValue| v.as_uint())
            .map(|v| v as usize);

        // For data messages, the rest is the piece data
        let piece_data = if msg_type == MetadataMessageType::Data && consumed < data.len() {
            Some(data[consumed..].to_vec())
        } else {
            None
        };

        Ok(Self {
            msg_type,
            piece,
            total_size,
            data: piece_data,
        })
    }
}

/// Metadata fetcher state for a single torrent.
pub struct MetadataFetcher {
    /// Expected info hash.
    info_hash: Sha1Hash,
    /// Total metadata size (learned from peers).
    total_size: RwLock<Option<usize>>,
    /// Received pieces (piece index -> data).
    pieces: RwLock<HashMap<usize, Vec<u8>>>,
    /// Pieces we've requested but not received.
    pending_requests: RwLock<HashMap<usize, std::time::Instant>>,
    /// Assembled metadata (set when complete).
    metadata: RwLock<Option<Vec<u8>>>,
}

impl MetadataFetcher {
    /// Create a new metadata fetcher for the given info hash.
    pub fn new(info_hash: Sha1Hash) -> Self {
        Self {
            info_hash,
            total_size: RwLock::new(None),
            pieces: RwLock::new(HashMap::new()),
            pending_requests: RwLock::new(HashMap::new()),
            metadata: RwLock::new(None),
        }
    }

    /// Check if we've received the complete metadata.
    pub async fn is_complete(&self) -> bool {
        self.metadata.read().await.is_some()
    }

    /// Get the number of pieces needed based on total_size.
    pub async fn num_pieces(&self) -> Option<usize> {
        let size = (*self.total_size.read().await)?;
        Some(size.div_ceil(METADATA_PIECE_SIZE))
    }

    /// Get pieces we should request.
    pub async fn get_needed_pieces(&self) -> Vec<usize> {
        let Some(num_pieces) = self.num_pieces().await else {
            // Don't know size yet, request piece 0 to learn it
            return vec![0];
        };

        let pieces = self.pieces.read().await;
        let pending = self.pending_requests.read().await;
        let now = std::time::Instant::now();
        let timeout = Duration::from_secs(30);

        (0..num_pieces)
            .filter(|i| {
                // Not yet received
                !pieces.contains_key(i)
                    // And not pending, or pending but timed out
                    && !pending
                        .get(i)
                        .map(|t| now.duration_since(*t) < timeout)
                        .unwrap_or(false)
            })
            .collect()
    }

    /// Mark a piece as requested.
    pub async fn mark_requested(&self, piece: usize) {
        self.pending_requests
            .write()
            .await
            .insert(piece, std::time::Instant::now());
    }

    /// Process a received metadata message.
    ///
    /// Returns true if the metadata is now complete and valid.
    pub async fn process_message(&self, msg: MetadataMessage) -> Result<bool> {
        match msg.msg_type {
            MetadataMessageType::Request => {
                // Peer is requesting from us - we don't have metadata to share
                // (we're fetching it ourselves)
                Ok(false)
            }

            MetadataMessageType::Reject => {
                // Peer doesn't have metadata
                self.pending_requests.write().await.remove(&msg.piece);
                Ok(false)
            }

            MetadataMessageType::Data => {
                let Some(total_size) = msg.total_size else {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::MetadataError,
                        "Data message missing total_size",
                    ));
                };

                let Some(data) = msg.data else {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::MetadataError,
                        "Data message missing piece data",
                    ));
                };

                if total_size == 0 || total_size > MAX_METADATA_SIZE {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::MetadataError,
                        format!(
                            "Peer-claimed metadata size {} outside allowed range 1..={}",
                            total_size, MAX_METADATA_SIZE
                        ),
                    ));
                }

                // Reject out-of-range piece indices: junk indices would count
                // toward the completion check and wedge metadata assembly.
                if msg.piece >= total_size.div_ceil(METADATA_PIECE_SIZE) {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::MetadataError,
                        format!(
                            "Metadata piece index {} out of range for size {}",
                            msg.piece, total_size
                        ),
                    ));
                }

                // Validate piece size
                let expected_size = if msg.piece < (total_size / METADATA_PIECE_SIZE) {
                    METADATA_PIECE_SIZE
                } else {
                    total_size % METADATA_PIECE_SIZE
                };

                if data.len() != expected_size && expected_size != 0 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::MetadataError,
                        format!(
                            "Piece {} has wrong size: {} (expected {})",
                            msg.piece,
                            data.len(),
                            expected_size
                        ),
                    ));
                }

                // Store total_size if we didn't know it
                {
                    let mut size = self.total_size.write().await;
                    if size.is_none() {
                        *size = Some(total_size);
                    }
                }

                // Store the piece
                {
                    let mut pieces = self.pieces.write().await;
                    pieces.insert(msg.piece, data);
                    self.pending_requests.write().await.remove(&msg.piece);
                }

                // Check if complete
                if let Some(num_pieces) = self.num_pieces().await {
                    let pieces = self.pieces.read().await;
                    if pieces.len() >= num_pieces {
                        // Assemble and verify
                        drop(pieces);
                        return self.assemble_and_verify().await;
                    }
                }

                Ok(false)
            }
        }
    }

    /// Assemble all pieces and verify the info hash.
    async fn assemble_and_verify(&self) -> Result<bool> {
        let Some(total_size) = *self.total_size.read().await else {
            return Ok(false);
        };

        let num_pieces = total_size.div_ceil(METADATA_PIECE_SIZE);
        let pieces = self.pieces.read().await;

        // Assemble in order
        let mut assembled = Vec::with_capacity(total_size);
        for i in 0..num_pieces {
            let Some(piece) = pieces.get(&i) else {
                return Ok(false); // Missing piece
            };
            assembled.extend_from_slice(piece);
        }

        // Trim to exact size (last piece might have extra)
        assembled.truncate(total_size);

        // Verify info hash
        let mut hasher = Sha1::new();
        hasher.update(&assembled);
        let hash: [u8; 20] = hasher.finalize().into();

        if hash != self.info_hash {
            tracing::warn!(
                "Metadata info_hash mismatch: expected {:?}, got {:?}",
                self.info_hash,
                hash
            );
            // Clear pieces to retry
            drop(pieces);
            self.pieces.write().await.clear();
            return Ok(false);
        }

        // Success! Store the metadata
        *self.metadata.write().await = Some(assembled);
        Ok(true)
    }

    /// Get the assembled metadata (if complete).
    pub async fn get_metadata(&self) -> Option<Vec<u8>> {
        self.metadata.read().await.clone()
    }

    /// Parse the metadata into a Metainfo struct.
    ///
    /// Note: This creates a synthetic Metainfo since we only have the info dict.
    pub async fn parse_metainfo(&self) -> Result<Option<Metainfo>> {
        let Some(data) = self.get_metadata().await else {
            return Ok(None);
        };

        // The metadata is just the info dict, not a full torrent
        let info_value = BencodeValue::parse_exact(&data)?;

        // Create a synthetic full torrent structure
        let mut torrent_dict = std::collections::BTreeMap::new();
        torrent_dict.insert(b"info".to_vec(), info_value);

        let torrent_bytes = BencodeValue::Dict(torrent_dict).encode();
        let metainfo = Metainfo::parse(&torrent_bytes)?;

        Ok(Some(metainfo))
    }

    /// Get the raw bencoded torrent data (synthetic, built from the info dict).
    /// Returns `None` if metadata hasn't been fully received yet.
    pub async fn raw_torrent_bytes(&self) -> Option<Vec<u8>> {
        let data = self.get_metadata().await?;
        let info_value = BencodeValue::parse_exact(&data).ok()?;
        let mut torrent_dict = std::collections::BTreeMap::new();
        torrent_dict.insert(b"info".to_vec(), info_value);
        Some(BencodeValue::Dict(torrent_dict).encode())
    }

    /// Get the total metadata size (if known).
    pub async fn total_size(&self) -> Option<usize> {
        *self.total_size.read().await
    }

    /// Get the number of received pieces.
    pub async fn received_count(&self) -> usize {
        self.pieces.read().await.len()
    }
}

/// Build extension handshake advertising ut_metadata support.
pub fn build_metadata_extension_handshake(
    metadata_size: Option<usize>,
) -> std::collections::BTreeMap<Vec<u8>, BencodeValue> {
    let mut m = std::collections::BTreeMap::new();
    m.insert(
        b"ut_metadata".to_vec(),
        BencodeValue::Integer(OUR_METADATA_EXTENSION_ID as i64),
    );

    let mut result = std::collections::BTreeMap::new();
    result.insert(b"m".to_vec(), BencodeValue::Dict(m));

    if let Some(size) = metadata_size {
        result.insert(
            b"metadata_size".to_vec(),
            BencodeValue::Integer(size as i64),
        );
    }

    result
}

/// Parse metadata_size from extension handshake.
pub fn parse_metadata_size(handshake_data: &[u8]) -> Option<usize> {
    let value = BencodeValue::parse_exact(handshake_data).ok()?;
    let dict = value.as_dict()?;
    dict.get(b"metadata_size".as_slice())
        .and_then(|v| v.as_uint())
        .map(|v| v as usize)
}

/// Parse ut_metadata extension ID from handshake.
pub fn parse_metadata_extension_id(handshake_data: &[u8]) -> Option<u8> {
    let value = BencodeValue::parse_exact(handshake_data).ok()?;
    let dict = value.as_dict()?;
    let m = dict.get(b"m".as_slice())?.as_dict()?;
    m.get(b"ut_metadata".as_slice())
        .and_then(|v| v.as_uint())
        .map(|v| v as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metadata_message_request() {
        let msg = MetadataMessage::request(5);
        let encoded = msg.encode();
        let parsed = MetadataMessage::parse(&encoded).unwrap();

        assert_eq!(parsed.msg_type, MetadataMessageType::Request);
        assert_eq!(parsed.piece, 5);
        assert!(parsed.data.is_none());
    }

    #[test]
    fn test_metadata_message_data() {
        let data = vec![1, 2, 3, 4, 5];
        let msg = MetadataMessage::data(0, 5, data.clone());
        let encoded = msg.encode();
        let parsed = MetadataMessage::parse(&encoded).unwrap();

        assert_eq!(parsed.msg_type, MetadataMessageType::Data);
        assert_eq!(parsed.piece, 0);
        assert_eq!(parsed.total_size, Some(5));
        assert_eq!(parsed.data, Some(data));
    }

    #[test]
    fn test_metadata_message_reject() {
        let msg = MetadataMessage::reject(3);
        let encoded = msg.encode();
        let parsed = MetadataMessage::parse(&encoded).unwrap();

        assert_eq!(parsed.msg_type, MetadataMessageType::Reject);
        assert_eq!(parsed.piece, 3);
    }

    #[tokio::test]
    async fn test_metadata_rejects_huge_claimed_size() {
        let fetcher = MetadataFetcher::new([0u8; 20]);
        let msg = MetadataMessage::data(0, MAX_METADATA_SIZE + 1, vec![0u8; METADATA_PIECE_SIZE]);
        assert!(fetcher.process_message(msg).await.is_err());
        // A rejected size must not poison the fetcher's known size
        assert!(fetcher.num_pieces().await.is_none());

        let msg = MetadataMessage::data(0, 0, Vec::new());
        assert!(fetcher.process_message(msg).await.is_err());
    }

    #[tokio::test]
    async fn test_metadata_rejects_out_of_range_piece_index() {
        let fetcher = MetadataFetcher::new([0u8; 20]);
        // total_size 5 => exactly 1 piece; index 7 is junk that previously
        // counted toward the completion check.
        let msg = MetadataMessage::data(7, 5, vec![0u8; 5]);
        assert!(fetcher.process_message(msg).await.is_err());
    }

    #[tokio::test]
    async fn test_metadata_fetcher_pieces() {
        // Create a small test metadata
        let test_metadata = b"d4:name4:test12:piece lengthi16384ee";
        let mut hasher = Sha1::new();
        hasher.update(test_metadata);
        let info_hash: [u8; 20] = hasher.finalize().into();

        let fetcher = MetadataFetcher::new(info_hash);

        // Process a data message with the full metadata (single piece)
        let msg = MetadataMessage::data(0, test_metadata.len(), test_metadata.to_vec());
        let complete = fetcher.process_message(msg).await.unwrap();

        assert!(complete);
        assert!(fetcher.is_complete().await);

        let metadata = fetcher.get_metadata().await.unwrap();
        assert_eq!(metadata, test_metadata);
    }

    #[tokio::test]
    async fn test_metadata_fetcher_wrong_hash() {
        let wrong_hash = [0u8; 20]; // Wrong hash
        let fetcher = MetadataFetcher::new(wrong_hash);

        let test_metadata = b"d4:name4:test12:piece lengthi16384ee";
        let msg = MetadataMessage::data(0, test_metadata.len(), test_metadata.to_vec());

        // Should fail verification
        let complete = fetcher.process_message(msg).await.unwrap();
        assert!(!complete);
        assert!(!fetcher.is_complete().await);
    }

    #[test]
    fn test_parse_metadata_extension_id() {
        let handshake = b"d1:md11:ut_metadatai2eee";
        let id = parse_metadata_extension_id(handshake);
        assert_eq!(id, Some(2));
    }

    #[test]
    fn test_parse_metadata_size() {
        let handshake = b"d13:metadata_sizei12345ee";
        let size = parse_metadata_size(handshake);
        assert_eq!(size, Some(12345));
    }
}
