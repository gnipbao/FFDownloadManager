//! Mock BitTorrent Peer for Testing
//!
//! This module provides a mock BitTorrent peer that can be used to test
//! torrent downloading functionality without needing real peers.

use bitvec::prelude::*;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;

/// Protocol string for BitTorrent handshake
const PROTOCOL_STRING: &[u8] = b"BitTorrent protocol";

/// Mock peer configuration
#[derive(Clone)]
pub struct MockPeerConfig {
    /// Info hash to accept connections for
    pub info_hash: [u8; 20],
    /// Our peer ID
    pub peer_id: [u8; 20],
    /// Pieces we have (bitfield)
    pub pieces: BitVec<u8, Msb0>,
    /// Piece data to serve
    pub piece_data: HashMap<u32, Vec<u8>>,
    /// Whether to immediately unchoke connecting peers
    pub auto_unchoke: bool,
    /// Support BEP 10 extension protocol
    pub support_extensions: bool,
    /// Metadata (for ut_metadata extension)
    pub metadata: Option<Vec<u8>>,
}

impl MockPeerConfig {
    /// Create a new mock peer config for testing
    pub fn new(info_hash: [u8; 20], num_pieces: usize) -> Self {
        let mut peer_id = [0u8; 20];
        peer_id[0..8].copy_from_slice(b"-MO0001-");
        for i in 8..20 {
            peer_id[i] = rand::random();
        }

        Self {
            info_hash,
            peer_id,
            pieces: bitvec![u8, Msb0; 0; num_pieces],
            piece_data: HashMap::new(),
            auto_unchoke: true,
            support_extensions: true,
            metadata: None,
        }
    }

    /// Set all pieces as available
    pub fn with_all_pieces(mut self) -> Self {
        self.pieces.fill(true);
        self
    }

    /// Add piece data
    pub fn with_piece(mut self, index: u32, data: Vec<u8>) -> Self {
        self.piece_data.insert(index, data);
        self.pieces.set(index as usize, true);
        self
    }

    /// Set metadata for ut_metadata extension
    pub fn with_metadata(mut self, metadata: Vec<u8>) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A mock BitTorrent peer for testing
pub struct MockPeer {
    config: MockPeerConfig,
    listener: TcpListener,
    connections: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
}

impl MockPeer {
    /// Create a new mock peer and start listening
    pub async fn new(config: MockPeerConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        Ok(Self {
            config,
            listener,
            connections: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Get the address this peer is listening on
    pub fn addr(&self) -> SocketAddr {
        self.listener.local_addr().unwrap()
    }

    /// Accept a single connection and handle it
    pub async fn accept_one(&self) -> std::io::Result<()> {
        let (stream, _addr) = self.listener.accept().await?;
        self.handle_connection(stream).await
    }

    /// Start accepting connections in the background
    pub fn start_accepting(self: Arc<Self>) {
        let peer = Arc::clone(&self);
        tokio::spawn(async move {
            loop {
                match peer.listener.accept().await {
                    Ok((stream, _addr)) => {
                        let peer_clone = Arc::clone(&peer);
                        let handle = tokio::spawn(async move {
                            if let Err(e) = peer_clone.handle_connection(stream).await {
                                tracing::debug!("Mock peer connection error: {}", e);
                            }
                        });
                        peer.connections.write().await.push(handle);
                    }
                    Err(e) => {
                        tracing::error!("Mock peer accept error: {}", e);
                        break;
                    }
                }
            }
        });
    }

    /// Handle a peer connection
    async fn handle_connection(&self, mut stream: TcpStream) -> std::io::Result<()> {
        // Perform handshake
        self.do_handshake(&mut stream).await?;

        // Send bitfield
        self.send_bitfield(&mut stream).await?;

        // If auto_unchoke is enabled, unchoke immediately
        if self.config.auto_unchoke {
            self.send_unchoke(&mut stream).await?;
        }

        // Handle messages
        loop {
            let msg = self.read_message(&mut stream).await?;
            match msg {
                PeerMessage::Interested => {
                    // Send unchoke if we haven't already
                    if !self.config.auto_unchoke {
                        self.send_unchoke(&mut stream).await?;
                    }
                }
                PeerMessage::Request {
                    index,
                    begin,
                    length,
                } => {
                    // Serve the requested block
                    if let Some(piece_data) = self.config.piece_data.get(&index) {
                        let end = (begin + length) as usize;
                        if end <= piece_data.len() {
                            let block = piece_data[begin as usize..end].to_vec();
                            self.send_piece(&mut stream, index, begin, block).await?;
                        }
                    }
                }
                PeerMessage::KeepAlive => {}
                PeerMessage::Extended { id, payload } => {
                    if id == 0 {
                        // Extension handshake
                        self.handle_extension_handshake(&mut stream, &payload)
                            .await?;
                    }
                }
                _ => {}
            }
        }
    }

    /// Perform BitTorrent handshake
    async fn do_handshake(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        // Read peer's handshake
        let mut handshake = [0u8; 68];
        stream.read_exact(&mut handshake).await?;

        // Verify protocol string
        if handshake[0] != 19 || &handshake[1..20] != PROTOCOL_STRING {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid protocol string",
            ));
        }

        // Verify info hash
        if &handshake[28..48] != &self.config.info_hash {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Info hash mismatch",
            ));
        }

        // Send our handshake
        let mut response = Vec::with_capacity(68);
        response.push(19);
        response.extend_from_slice(PROTOCOL_STRING);

        // Reserved bytes
        let mut reserved = [0u8; 8];
        if self.config.support_extensions {
            reserved[5] |= 0x10; // Extension protocol
        }
        response.extend_from_slice(&reserved);

        response.extend_from_slice(&self.config.info_hash);
        response.extend_from_slice(&self.config.peer_id);
        stream.write_all(&response).await?;

        Ok(())
    }

    /// Send bitfield message
    async fn send_bitfield(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        let bitfield = self.config.pieces.as_raw_slice();
        let len = 1 + bitfield.len() as u32;
        let mut msg = Vec::with_capacity(4 + len as usize);
        msg.extend_from_slice(&len.to_be_bytes());
        msg.push(5); // Bitfield message ID
        msg.extend_from_slice(bitfield);
        stream.write_all(&msg).await
    }

    /// Send unchoke message
    async fn send_unchoke(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        stream.write_all(&[0, 0, 0, 1, 1]).await
    }

    /// Send piece message
    async fn send_piece(
        &self,
        stream: &mut TcpStream,
        index: u32,
        begin: u32,
        block: Vec<u8>,
    ) -> std::io::Result<()> {
        let len = 9 + block.len() as u32;
        let mut msg = Vec::with_capacity(4 + len as usize);
        msg.extend_from_slice(&len.to_be_bytes());
        msg.push(7); // Piece message ID
        msg.extend_from_slice(&index.to_be_bytes());
        msg.extend_from_slice(&begin.to_be_bytes());
        msg.extend_from_slice(&block);
        stream.write_all(&msg).await
    }

    /// Read a peer message
    async fn read_message(&self, stream: &mut TcpStream) -> std::io::Result<PeerMessage> {
        // Read length prefix
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len == 0 {
            return Ok(PeerMessage::KeepAlive);
        }

        // Read message
        let mut data = vec![0u8; len];
        stream.read_exact(&mut data).await?;

        let id = data[0];
        let payload = &data[1..];

        Ok(match id {
            0 => PeerMessage::Choke,
            1 => PeerMessage::Unchoke,
            2 => PeerMessage::Interested,
            3 => PeerMessage::NotInterested,
            4 => PeerMessage::Have {
                piece_index: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
            },
            5 => PeerMessage::Bitfield {
                bitfield: payload.to_vec(),
            },
            6 => PeerMessage::Request {
                index: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
                begin: u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
                length: u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]),
            },
            7 => PeerMessage::Piece {
                index: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
                begin: u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
                block: payload[8..].to_vec(),
            },
            8 => PeerMessage::Cancel {
                index: u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]),
                begin: u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]),
                length: u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]),
            },
            20 => PeerMessage::Extended {
                id: payload[0],
                payload: payload[1..].to_vec(),
            },
            _ => PeerMessage::Unknown {
                id,
                payload: payload.to_vec(),
            },
        })
    }

    /// Handle extension handshake
    async fn handle_extension_handshake(
        &self,
        stream: &mut TcpStream,
        _payload: &[u8],
    ) -> std::io::Result<()> {
        // Send our extension handshake response
        // This is a simplified version - in production you'd properly parse and respond
        let mut response = b"d1:md11:ut_metadatai1eee".to_vec();

        if let Some(ref metadata) = self.config.metadata {
            // Add metadata_size if we have metadata
            response = format!(
                "d1:md11:ut_metadatai1ee13:metadata_sizei{}ee",
                metadata.len()
            )
            .into_bytes();
        }

        let len = 2 + response.len() as u32;
        let mut msg = Vec::with_capacity(4 + len as usize);
        msg.extend_from_slice(&len.to_be_bytes());
        msg.push(20); // Extended message
        msg.push(0); // Handshake
        msg.extend_from_slice(&response);
        stream.write_all(&msg).await
    }
}

/// Simplified peer message enum for mock peer
#[derive(Debug)]
enum PeerMessage {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have {
        piece_index: u32,
    },
    Bitfield {
        bitfield: Vec<u8>,
    },
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        index: u32,
        begin: u32,
        block: Vec<u8>,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    Extended {
        id: u8,
        payload: Vec<u8>,
    },
    Unknown {
        id: u8,
        payload: Vec<u8>,
    },
}

/// Helper to create test piece data with valid SHA1 hashes
pub fn create_test_piece_data(piece_length: usize) -> (Vec<u8>, [u8; 20]) {
    let data: Vec<u8> = (0..piece_length).map(|i| (i % 256) as u8).collect();
    let mut hasher = Sha1::new();
    hasher.update(&data);
    let hash: [u8; 20] = hasher.finalize().into();
    (data, hash)
}

/// Generate a random info hash for testing
pub fn random_info_hash() -> [u8; 20] {
    let mut hash = [0u8; 20];
    for byte in &mut hash {
        *byte = rand::random();
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_peer_creation() {
        let info_hash = random_info_hash();
        let config = MockPeerConfig::new(info_hash, 10);
        let peer = MockPeer::new(config).await.unwrap();

        let addr = peer.addr();
        assert!(addr.port() > 0);
    }

    #[test]
    fn test_create_piece_data() {
        let (data, hash) = create_test_piece_data(16384);
        assert_eq!(data.len(), 16384);

        // Verify hash
        let mut hasher = Sha1::new();
        hasher.update(&data);
        let computed: [u8; 20] = hasher.finalize().into();
        assert_eq!(hash, computed);
    }
}
