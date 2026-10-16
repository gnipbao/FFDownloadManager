//! Peer Wire Protocol
//!
//! This module implements the BitTorrent peer wire protocol as defined in BEP 3.
//! It handles connections to peers, message encoding/decoding, and state management.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bitvec::prelude::*;
use bytes::BytesMut;
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::metainfo::Sha1Hash;
use super::mse::{connect_with_mse, MseConfig, PeerStream};
use super::pex::{self, PexMessage, PEX_EXTENSION_NAME};
use crate::error::{EngineError, NetworkErrorKind, ProtocolErrorKind, Result};

/// Protocol string for BitTorrent
const PROTOCOL_STRING: &[u8] = b"BitTorrent protocol";

/// Size of the handshake message
const HANDSHAKE_SIZE: usize = 68; // 1 + 19 + 8 + 20 + 20

/// Default timeout for operations
// Also the cadence at which an idle peer loop wakes to apply choking
// decisions and send keep-alives; keep it well under the keep-alive interval.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for initial TCP connection to a peer
const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum message size (16KB block + overhead)
const MAX_MESSAGE_SIZE: usize = 32 * 1024;

/// Default block size (16KB)
pub const BLOCK_SIZE: u32 = 16384;

/// Reserved bytes for extensions
#[derive(Debug, Clone, Copy, Default)]
pub struct ReservedBytes([u8; 8]);

impl ReservedBytes {
    /// Create reserved bytes with extension support flags
    pub fn with_extensions() -> Self {
        let mut reserved = [0u8; 8];
        // Bit 20 (from the right, byte 5 bit 4) = Extension Protocol (BEP 10)
        reserved[5] |= 0x10;
        // Bit 2 (from the right, byte 7 bit 2) = Fast Extension (BEP 6)
        reserved[7] |= 0x04;
        Self(reserved)
    }

    /// Check if Extension Protocol is supported
    pub fn supports_extension_protocol(&self) -> bool {
        (self.0[5] & 0x10) != 0
    }

    /// Check if DHT is supported (BEP 5)
    pub fn supports_dht(&self) -> bool {
        (self.0[7] & 0x01) != 0
    }

    /// Check if Fast Extension is supported (BEP 6)
    pub fn supports_fast(&self) -> bool {
        (self.0[7] & 0x04) != 0
    }
}

/// Peer connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connecting to peer
    Connecting,
    /// Handshake in progress
    Handshaking,
    /// Connected and ready
    Connected,
    /// Disconnected
    Disconnected,
}

/// Peer wire protocol message types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerMessage {
    /// Keep connection alive (no payload)
    KeepAlive,

    /// Choke the peer (stop sending data)
    Choke,

    /// Unchoke the peer (start sending data)
    Unchoke,

    /// Interested in peer's data
    Interested,

    /// Not interested in peer's data
    NotInterested,

    /// Have a specific piece
    Have { piece_index: u32 },

    /// Bitfield of pieces we have
    Bitfield { bitfield: Vec<u8> },

    /// Request a block
    Request { index: u32, begin: u32, length: u32 },

    /// Piece data (response to request)
    Piece {
        index: u32,
        begin: u32,
        block: Vec<u8>,
    },

    /// Cancel a pending request
    Cancel { index: u32, begin: u32, length: u32 },

    /// DHT port (BEP 5)
    Port { port: u16 },

    // BEP 6: Fast Extension messages
    /// Suggest a piece to download (BEP 6)
    SuggestPiece { piece_index: u32 },

    /// Peer has all pieces (BEP 6) - replaces Bitfield
    HaveAll,

    /// Peer has no pieces (BEP 6) - replaces Bitfield
    HaveNone,

    /// Reject a request (BEP 6) - peer won't send this block
    RejectRequest { index: u32, begin: u32, length: u32 },

    /// Piece is allowed to be requested while choked (BEP 6)
    AllowedFast { piece_index: u32 },

    /// Extension message (BEP 10)
    Extended { id: u8, payload: Vec<u8> },

    /// Unknown message type
    Unknown { id: u8, payload: Vec<u8> },
}

impl PeerMessage {
    /// Get the message ID
    pub fn id(&self) -> Option<u8> {
        match self {
            Self::KeepAlive => None,
            Self::Choke => Some(0),
            Self::Unchoke => Some(1),
            Self::Interested => Some(2),
            Self::NotInterested => Some(3),
            Self::Have { .. } => Some(4),
            Self::Bitfield { .. } => Some(5),
            Self::Request { .. } => Some(6),
            Self::Piece { .. } => Some(7),
            Self::Cancel { .. } => Some(8),
            Self::Port { .. } => Some(9),
            // BEP 6: Fast Extension
            Self::SuggestPiece { .. } => Some(0x0D),
            Self::HaveAll => Some(0x0E),
            Self::HaveNone => Some(0x0F),
            Self::RejectRequest { .. } => Some(0x10),
            Self::AllowedFast { .. } => Some(0x11),
            Self::Extended { .. } => Some(20),
            Self::Unknown { id, .. } => Some(*id),
        }
    }

    /// Encode the message to bytes
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::KeepAlive => {
                vec![0, 0, 0, 0] // Length prefix only, no payload
            }

            Self::Choke => {
                vec![0, 0, 0, 1, 0]
            }

            Self::Unchoke => {
                vec![0, 0, 0, 1, 1]
            }

            Self::Interested => {
                vec![0, 0, 0, 1, 2]
            }

            Self::NotInterested => {
                vec![0, 0, 0, 1, 3]
            }

            Self::Have { piece_index } => {
                let mut buf = vec![0, 0, 0, 5, 4];
                buf.extend_from_slice(&piece_index.to_be_bytes());
                buf
            }

            Self::Bitfield { bitfield } => {
                let len = 1 + bitfield.len() as u32;
                let mut buf = Vec::with_capacity(4 + len as usize);
                buf.extend_from_slice(&len.to_be_bytes());
                buf.push(5);
                buf.extend_from_slice(bitfield);
                buf
            }

            Self::Request {
                index,
                begin,
                length,
            } => {
                let mut buf = vec![0, 0, 0, 13, 6];
                buf.extend_from_slice(&index.to_be_bytes());
                buf.extend_from_slice(&begin.to_be_bytes());
                buf.extend_from_slice(&length.to_be_bytes());
                buf
            }

            Self::Piece {
                index,
                begin,
                block,
            } => {
                let len = 9 + block.len() as u32;
                let mut buf = Vec::with_capacity(4 + len as usize);
                buf.extend_from_slice(&len.to_be_bytes());
                buf.push(7);
                buf.extend_from_slice(&index.to_be_bytes());
                buf.extend_from_slice(&begin.to_be_bytes());
                buf.extend_from_slice(block);
                buf
            }

            Self::Cancel {
                index,
                begin,
                length,
            } => {
                let mut buf = vec![0, 0, 0, 13, 8];
                buf.extend_from_slice(&index.to_be_bytes());
                buf.extend_from_slice(&begin.to_be_bytes());
                buf.extend_from_slice(&length.to_be_bytes());
                buf
            }

            Self::Port { port } => {
                let mut buf = vec![0, 0, 0, 3, 9];
                buf.extend_from_slice(&port.to_be_bytes());
                buf
            }

            // BEP 6: Fast Extension messages
            Self::SuggestPiece { piece_index } => {
                let mut buf = vec![0, 0, 0, 5, 0x0D];
                buf.extend_from_slice(&piece_index.to_be_bytes());
                buf
            }

            Self::HaveAll => {
                vec![0, 0, 0, 1, 0x0E]
            }

            Self::HaveNone => {
                vec![0, 0, 0, 1, 0x0F]
            }

            Self::RejectRequest {
                index,
                begin,
                length,
            } => {
                let mut buf = vec![0, 0, 0, 13, 0x10];
                buf.extend_from_slice(&index.to_be_bytes());
                buf.extend_from_slice(&begin.to_be_bytes());
                buf.extend_from_slice(&length.to_be_bytes());
                buf
            }

            Self::AllowedFast { piece_index } => {
                let mut buf = vec![0, 0, 0, 5, 0x11];
                buf.extend_from_slice(&piece_index.to_be_bytes());
                buf
            }

            Self::Extended { id, payload } => {
                let len = 2 + payload.len() as u32;
                let mut buf = Vec::with_capacity(4 + len as usize);
                buf.extend_from_slice(&len.to_be_bytes());
                buf.push(20);
                buf.push(*id);
                buf.extend_from_slice(payload);
                buf
            }

            Self::Unknown { id, payload } => {
                let len = 1 + payload.len() as u32;
                let mut buf = Vec::with_capacity(4 + len as usize);
                buf.extend_from_slice(&len.to_be_bytes());
                buf.push(*id);
                buf.extend_from_slice(payload);
                buf
            }
        }
    }

    /// Decode a message from bytes (without length prefix)
    pub fn decode(data: &[u8]) -> Result<Self> {
        if data.is_empty() {
            return Ok(Self::KeepAlive);
        }

        let id = data[0];
        let payload = &data[1..];

        match id {
            0 => Ok(Self::Choke),
            1 => Ok(Self::Unchoke),
            2 => Ok(Self::Interested),
            3 => Ok(Self::NotInterested),

            4 => {
                if payload.len() < 4 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "Have message too short",
                    ));
                }
                let piece_index =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Self::Have { piece_index })
            }

            5 => Ok(Self::Bitfield {
                bitfield: payload.to_vec(),
            }),

            6 => {
                if payload.len() < 12 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "Request message too short",
                    ));
                }
                let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let length = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
                Ok(Self::Request {
                    index,
                    begin,
                    length,
                })
            }

            7 => {
                if payload.len() < 8 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "Piece message too short",
                    ));
                }
                let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let block = payload[8..].to_vec();
                Ok(Self::Piece {
                    index,
                    begin,
                    block,
                })
            }

            8 => {
                if payload.len() < 12 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "Cancel message too short",
                    ));
                }
                let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let length = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
                Ok(Self::Cancel {
                    index,
                    begin,
                    length,
                })
            }

            9 => {
                if payload.len() < 2 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "Port message too short",
                    ));
                }
                let port = u16::from_be_bytes([payload[0], payload[1]]);
                Ok(Self::Port { port })
            }

            // BEP 6: Fast Extension messages
            0x0D => {
                // SuggestPiece
                if payload.len() < 4 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "SuggestPiece message too short",
                    ));
                }
                let piece_index =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Self::SuggestPiece { piece_index })
            }

            0x0E => {
                // HaveAll
                Ok(Self::HaveAll)
            }

            0x0F => {
                // HaveNone
                Ok(Self::HaveNone)
            }

            0x10 => {
                // RejectRequest
                if payload.len() < 12 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "RejectRequest message too short",
                    ));
                }
                let index = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let begin = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                let length = u32::from_be_bytes([payload[8], payload[9], payload[10], payload[11]]);
                Ok(Self::RejectRequest {
                    index,
                    begin,
                    length,
                })
            }

            0x11 => {
                // AllowedFast
                if payload.len() < 4 {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "AllowedFast message too short",
                    ));
                }
                let piece_index =
                    u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Self::AllowedFast { piece_index })
            }

            20 => {
                if payload.is_empty() {
                    return Err(EngineError::protocol(
                        ProtocolErrorKind::PeerProtocol,
                        "Extended message too short",
                    ));
                }
                Ok(Self::Extended {
                    id: payload[0],
                    payload: payload[1..].to_vec(),
                })
            }

            _ => Ok(Self::Unknown {
                id,
                payload: payload.to_vec(),
            }),
        }
    }
}

/// Our extension ID for ut_pex (what we advertise to peers).
pub const OUR_PEX_EXTENSION_ID: u8 = 1;

/// Peer connection
pub struct PeerConnection {
    stream: PeerStream,
    addr: SocketAddr,
    info_hash: Sha1Hash,
    our_peer_id: [u8; 20],
    peer_id: Option<[u8; 20]>,
    reserved: ReservedBytes,
    peer_reserved: ReservedBytes,
    state: ConnectionState,

    // Protocol state
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,

    // Piece tracking
    peer_pieces: BitVec<u8, Msb0>,
    num_pieces: usize,

    // Statistics
    uploaded: u64,
    downloaded: u64,
    last_activity: Instant,

    // Buffer for reading
    read_buffer: BytesMut,

    // Extension protocol state (BEP 10)
    /// Whether extension handshake has been completed.
    extension_handshake_done: bool,
    /// Peer's extension ID mappings (extension name -> ID).
    peer_extensions: HashMap<String, u8>,
    /// Peer's client identification string.
    peer_client: Option<String>,
    /// Peer's listen port (from extension handshake).
    peer_listen_port: Option<u16>,

    /// Whether the connection is encrypted (MSE/PE).
    encrypted: bool,
}

impl PeerConnection {
    /// Connect to a peer and perform handshake (plaintext)
    pub async fn connect(
        addr: SocketAddr,
        info_hash: Sha1Hash,
        peer_id: [u8; 20],
        num_pieces: usize,
    ) -> Result<Self> {
        Self::connect_with_encryption(addr, info_hash, peer_id, num_pieces, None).await
    }

    /// Connect to a peer with optional MSE encryption
    pub async fn connect_with_encryption(
        addr: SocketAddr,
        info_hash: Sha1Hash,
        peer_id: [u8; 20],
        num_pieces: usize,
        mse_config: Option<&MseConfig>,
    ) -> Result<Self> {
        let tcp_stream = timeout(PEER_CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                EngineError::network(NetworkErrorKind::Timeout, "Peer connection timeout")
            })?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionRefused,
                    format!("Failed to connect: {}", e),
                )
            })?;

        // Optionally perform MSE handshake
        let (stream, encrypted) = if let Some(config) = mse_config {
            match connect_with_mse(tcp_stream, info_hash, config).await {
                Ok(peer_stream) => {
                    let is_encrypted = peer_stream.is_encrypted();
                    (peer_stream, is_encrypted)
                }
                Err(error)
                    if config.policy != super::mse::EncryptionPolicy::Required
                        && config.allow_plaintext =>
                {
                    tracing::debug!(
                        "MSE handshake with {} failed ({}), retrying plaintext",
                        addr,
                        error
                    );
                    let fallback_stream = timeout(PEER_CONNECT_TIMEOUT, TcpStream::connect(addr))
                        .await
                        .map_err(|_| {
                            EngineError::network(
                                NetworkErrorKind::Timeout,
                                "Peer connection timeout",
                            )
                        })?
                        .map_err(|e| {
                            EngineError::network(
                                NetworkErrorKind::ConnectionRefused,
                                format!("Failed to connect: {}", e),
                            )
                        })?;
                    (PeerStream::Plain(fallback_stream), false)
                }
                Err(error) => return Err(error),
            }
        } else {
            (PeerStream::Plain(tcp_stream), false)
        };

        let mut conn = Self {
            stream,
            addr,
            info_hash,
            our_peer_id: peer_id,
            peer_id: None,
            reserved: ReservedBytes::with_extensions(),
            peer_reserved: ReservedBytes::default(),
            state: ConnectionState::Connecting,

            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,

            peer_pieces: bitvec![u8, Msb0; 0; num_pieces],
            num_pieces,

            uploaded: 0,
            downloaded: 0,
            last_activity: Instant::now(),

            read_buffer: BytesMut::with_capacity(MAX_MESSAGE_SIZE),

            extension_handshake_done: false,
            peer_extensions: HashMap::new(),
            peer_client: None,
            peer_listen_port: None,
            encrypted,
        };

        conn.handshake().await?;

        Ok(conn)
    }

    /// Connect to a peer via uTP and perform handshake
    pub async fn connect_utp(
        socket: super::utp::UtpSocket,
        info_hash: Sha1Hash,
        peer_id: [u8; 20],
        num_pieces: usize,
    ) -> Result<Self> {
        let addr = socket
            .peer_addr()
            .map_err(|e| EngineError::network(NetworkErrorKind::Other, e.to_string()))?;

        let mut conn = Self {
            stream: PeerStream::Utp(socket),
            addr,
            info_hash,
            our_peer_id: peer_id,
            peer_id: None,
            reserved: ReservedBytes::with_extensions(),
            peer_reserved: ReservedBytes::default(),
            state: ConnectionState::Connecting,

            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,

            peer_pieces: bitvec![u8, Msb0; 0; num_pieces],
            num_pieces,

            uploaded: 0,
            downloaded: 0,
            last_activity: Instant::now(),

            read_buffer: BytesMut::with_capacity(MAX_MESSAGE_SIZE),

            extension_handshake_done: false,
            peer_extensions: HashMap::new(),
            peer_client: None,
            peer_listen_port: None,
            encrypted: false,
        };

        conn.handshake().await?;

        Ok(conn)
    }

    /// Accept an incoming connection and perform handshake (plaintext)
    pub async fn accept(
        stream: TcpStream,
        addr: SocketAddr,
        info_hash: Sha1Hash,
        peer_id: [u8; 20],
        num_pieces: usize,
    ) -> Result<Self> {
        Self::accept_with_encryption(stream, addr, info_hash, peer_id, num_pieces, None).await
    }

    /// Accept an incoming connection with optional MSE encryption
    pub async fn accept_with_encryption(
        stream: TcpStream,
        addr: SocketAddr,
        info_hash: Sha1Hash,
        peer_id: [u8; 20],
        num_pieces: usize,
        mse_config: Option<&MseConfig>,
    ) -> Result<Self> {
        // For incoming connections, MSE handshake would be initiated by the peer
        // For now, we accept plaintext and can add MSE responder later
        let (stream, encrypted) = if let Some(config) = mse_config {
            let peer_stream = connect_with_mse(stream, info_hash, config).await?;
            let is_encrypted = peer_stream.is_encrypted();
            (peer_stream, is_encrypted)
        } else {
            (PeerStream::Plain(stream), false)
        };

        let mut conn = Self {
            stream,
            addr,
            info_hash,
            our_peer_id: peer_id,
            peer_id: None,
            reserved: ReservedBytes::with_extensions(),
            peer_reserved: ReservedBytes::default(),
            state: ConnectionState::Connecting,

            am_choking: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,

            peer_pieces: bitvec![u8, Msb0; 0; num_pieces],
            num_pieces,

            uploaded: 0,
            downloaded: 0,
            last_activity: Instant::now(),

            read_buffer: BytesMut::with_capacity(MAX_MESSAGE_SIZE),

            extension_handshake_done: false,
            peer_extensions: HashMap::new(),
            peer_client: None,
            peer_listen_port: None,
            encrypted,
        };

        conn.handshake().await?;

        Ok(conn)
    }

    /// Perform the BitTorrent handshake
    async fn handshake(&mut self) -> Result<()> {
        self.state = ConnectionState::Handshaking;

        // Build handshake message
        let mut handshake = Vec::with_capacity(HANDSHAKE_SIZE);
        handshake.push(PROTOCOL_STRING.len() as u8);
        handshake.extend_from_slice(PROTOCOL_STRING);
        handshake.extend_from_slice(&self.reserved.0);
        handshake.extend_from_slice(&self.info_hash);
        handshake.extend_from_slice(&self.our_peer_id);

        // Send handshake (PeerStream handles encryption transparently)
        timeout(DEFAULT_TIMEOUT, self.stream.write_all(&handshake))
            .await
            .map_err(|_| EngineError::network(NetworkErrorKind::Timeout, "Handshake send timeout"))?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    format!("Handshake send failed: {}", e),
                )
            })?;

        // Receive handshake (PeerStream handles decryption transparently)
        let mut response = [0u8; HANDSHAKE_SIZE];
        timeout(DEFAULT_TIMEOUT, self.stream.read_exact(&mut response))
            .await
            .map_err(|_| {
                EngineError::network(NetworkErrorKind::Timeout, "Handshake receive timeout")
            })?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    format!("Handshake receive failed: {}", e),
                )
            })?;

        // Validate handshake
        let pstrlen = response[0] as usize;
        if pstrlen != PROTOCOL_STRING.len() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                format!("Invalid protocol string length: {}", pstrlen),
            ));
        }

        if &response[1..1 + pstrlen] != PROTOCOL_STRING {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                "Invalid protocol string",
            ));
        }

        // Store peer's reserved bytes
        self.peer_reserved.0.copy_from_slice(&response[20..28]);

        // Verify info_hash matches
        let mut peer_info_hash = [0u8; 20];
        peer_info_hash.copy_from_slice(&response[28..48]);

        if peer_info_hash != self.info_hash {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                "Info hash mismatch",
            ));
        }

        // Store peer ID
        let mut peer_id = [0u8; 20];
        peer_id.copy_from_slice(&response[48..68]);
        self.peer_id = Some(peer_id);

        self.state = ConnectionState::Connected;
        self.last_activity = Instant::now();

        Ok(())
    }

    /// Send a message to the peer
    pub async fn send(&mut self, msg: PeerMessage) -> Result<()> {
        let data = msg.encode();

        timeout(DEFAULT_TIMEOUT, self.stream.write_all(&data))
            .await
            .map_err(|_| EngineError::network(NetworkErrorKind::Timeout, "Send timeout"))?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    format!("Send failed: {}", e),
                )
            })?;

        if let PeerMessage::Piece { block, .. } = &msg {
            self.uploaded += block.len() as u64;
        }

        self.last_activity = Instant::now();
        Ok(())
    }

    /// Receive a message from the peer
    pub async fn recv(&mut self) -> Result<PeerMessage> {
        // Read length prefix (4 bytes)
        let mut len_buf = [0u8; 4];
        timeout(DEFAULT_TIMEOUT, self.stream.read_exact(&mut len_buf))
            .await
            .map_err(|_| EngineError::network(NetworkErrorKind::Timeout, "Receive timeout"))?
            .map_err(|e| {
                EngineError::network(
                    NetworkErrorKind::ConnectionReset,
                    format!("Receive failed: {}", e),
                )
            })?;

        let len = u32::from_be_bytes(len_buf) as usize;

        // Keep-alive message (length = 0)
        if len == 0 {
            self.last_activity = Instant::now();
            return Ok(PeerMessage::KeepAlive);
        }

        // Check for unreasonably large messages
        if len > MAX_MESSAGE_SIZE {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                format!("Message too large: {} bytes", len),
            ));
        }

        // Read message body
        self.read_buffer.resize(len, 0);
        timeout(
            DEFAULT_TIMEOUT,
            self.stream.read_exact(&mut self.read_buffer),
        )
        .await
        .map_err(|_| EngineError::network(NetworkErrorKind::Timeout, "Receive body timeout"))?
        .map_err(|e| {
            EngineError::network(
                NetworkErrorKind::ConnectionReset,
                format!("Receive body failed: {}", e),
            )
        })?;

        let msg = PeerMessage::decode(&self.read_buffer)?;

        // Update state based on message
        self.handle_message(&msg);

        self.last_activity = Instant::now();
        Ok(msg)
    }

    /// Handle incoming message state updates
    fn handle_message(&mut self, msg: &PeerMessage) {
        match msg {
            PeerMessage::Choke => {
                self.peer_choking = true;
            }
            PeerMessage::Unchoke => {
                self.peer_choking = false;
            }
            PeerMessage::Interested => {
                self.peer_interested = true;
            }
            PeerMessage::NotInterested => {
                self.peer_interested = false;
            }
            PeerMessage::Have { piece_index } if (*piece_index as usize) < self.num_pieces => {
                self.peer_pieces.set(*piece_index as usize, true);
            }
            PeerMessage::Bitfield { bitfield } => {
                // Validate bitfield size (should be ceil(num_pieces / 8) bytes)
                let expected_size = self.num_pieces.div_ceil(8);
                if bitfield.len() != expected_size {
                    tracing::warn!(
                        "Peer sent bitfield with wrong size: expected {} bytes, got {}",
                        expected_size,
                        bitfield.len()
                    );
                    // Still process it but only up to the valid portion
                }

                // Copy bitfield (limit to prevent DoS from oversized bitfields)
                let max_bytes = expected_size.min(bitfield.len());
                for (i, byte) in bitfield.iter().take(max_bytes).enumerate() {
                    for bit in 0..8 {
                        let piece_idx = i * 8 + bit;
                        if piece_idx < self.num_pieces {
                            self.peer_pieces.set(piece_idx, (byte & (0x80 >> bit)) != 0);
                        }
                    }
                }
            }
            PeerMessage::Piece { block, .. } => {
                self.downloaded += block.len() as u64;
            }
            PeerMessage::HaveAll => {
                // BEP 6: Peer has all pieces
                self.peer_pieces.fill(true);
            }
            PeerMessage::HaveNone => {
                // BEP 6: Peer has no pieces
                self.peer_pieces.fill(false);
            }
            _ => {}
        }
    }

    /// Send choke message
    pub async fn choke(&mut self) -> Result<()> {
        self.am_choking = true;
        self.send(PeerMessage::Choke).await
    }

    /// Send unchoke message
    pub async fn unchoke(&mut self) -> Result<()> {
        self.am_choking = false;
        self.send(PeerMessage::Unchoke).await
    }

    /// Send interested message
    pub async fn interested(&mut self) -> Result<()> {
        self.am_interested = true;
        self.send(PeerMessage::Interested).await
    }

    /// Send not interested message
    pub async fn not_interested(&mut self) -> Result<()> {
        self.am_interested = false;
        self.send(PeerMessage::NotInterested).await
    }

    /// Request a block from the peer
    pub async fn request_block(&mut self, index: u32, begin: u32, length: u32) -> Result<()> {
        self.send(PeerMessage::Request {
            index,
            begin,
            length,
        })
        .await
    }

    /// Cancel a pending block request
    pub async fn cancel_request(&mut self, index: u32, begin: u32, length: u32) -> Result<()> {
        self.send(PeerMessage::Cancel {
            index,
            begin,
            length,
        })
        .await
    }

    /// Send a have message
    pub async fn have(&mut self, piece_index: u32) -> Result<()> {
        self.send(PeerMessage::Have { piece_index }).await
    }

    /// Send our bitfield
    pub async fn send_bitfield(&mut self, bitfield: &BitVec<u8, Msb0>) -> Result<()> {
        let bytes: Vec<u8> = bitfield.as_raw_slice().to_vec();
        self.send(PeerMessage::Bitfield { bitfield: bytes }).await
    }

    /// Send a piece (block of data)
    pub async fn send_piece(&mut self, index: u32, begin: u32, block: Vec<u8>) -> Result<()> {
        self.send(PeerMessage::Piece {
            index,
            begin,
            block,
        })
        .await
    }

    /// Send keep-alive
    pub async fn keep_alive(&mut self) -> Result<()> {
        self.send(PeerMessage::KeepAlive).await
    }

    // Accessors

    /// Get the peer's address
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Get the peer's ID
    pub fn peer_id(&self) -> Option<&[u8; 20]> {
        self.peer_id.as_ref()
    }

    /// Get the connection state
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Check if we're choking the peer
    pub fn am_choking(&self) -> bool {
        self.am_choking
    }

    /// Check if we're interested in the peer
    pub fn am_interested(&self) -> bool {
        self.am_interested
    }

    /// Check if the peer is choking us
    pub fn peer_choking(&self) -> bool {
        self.peer_choking
    }

    /// Check if the peer is interested in us
    pub fn peer_interested(&self) -> bool {
        self.peer_interested
    }

    /// Get the peer's bitfield
    pub fn peer_pieces(&self) -> &BitVec<u8, Msb0> {
        &self.peer_pieces
    }

    /// Check if the peer has a specific piece
    pub fn peer_has_piece(&self, index: usize) -> bool {
        self.peer_pieces.get(index).map(|b| *b).unwrap_or(false)
    }

    /// Get bytes uploaded to this peer
    pub fn uploaded(&self) -> u64 {
        self.uploaded
    }

    /// Get bytes downloaded from this peer
    pub fn downloaded(&self) -> u64 {
        self.downloaded
    }

    /// Get time since last activity
    pub fn idle_time(&self) -> Duration {
        self.last_activity.elapsed()
    }

    /// Check if peer supports extension protocol
    pub fn supports_extensions(&self) -> bool {
        self.peer_reserved.supports_extension_protocol()
    }

    /// Check if peer supports DHT
    pub fn supports_dht(&self) -> bool {
        self.peer_reserved.supports_dht()
    }

    // Extension Protocol Methods (BEP 10)

    /// Send extension handshake to peer.
    ///
    /// Should be called after the regular handshake if both peers support extensions.
    pub async fn send_extension_handshake(
        &mut self,
        metadata_id: Option<u8>,
        listen_port: Option<u16>,
    ) -> Result<()> {
        if !self.supports_extensions() {
            return Ok(()); // Peer doesn't support extensions
        }

        let payload =
            pex::build_extension_handshake(OUR_PEX_EXTENSION_ID, metadata_id, listen_port);
        self.send(PeerMessage::Extended { id: 0, payload }).await
    }

    /// Handle received extension message.
    ///
    /// Returns discovered peers if this was a PEX message.
    pub fn handle_extension_message(
        &mut self,
        id: u8,
        payload: &[u8],
    ) -> Result<Option<Vec<SocketAddr>>> {
        if id == 0 {
            // Extension handshake
            self.handle_extension_handshake(payload)?;
            return Ok(None);
        }

        // Check if this is a PEX message
        if let Some(&pex_id) = self.peer_extensions.get(PEX_EXTENSION_NAME) {
            if id == pex_id {
                let pex_msg = PexMessage::parse(payload)?;
                return Ok(Some(pex_msg.all_added()));
            }
        }

        // Unknown extension - ignore
        Ok(None)
    }

    /// Handle extension handshake message.
    fn handle_extension_handshake(&mut self, payload: &[u8]) -> Result<()> {
        let handshake = pex::parse_extension_handshake(payload)?;

        self.peer_extensions = handshake.extensions;
        self.peer_client = handshake.client;
        self.peer_listen_port = handshake.listen_port;
        self.extension_handshake_done = true;

        Ok(())
    }

    /// Check if peer supports PEX.
    pub fn supports_pex(&self) -> bool {
        self.peer_extensions.contains_key(PEX_EXTENSION_NAME)
    }

    /// Get peer's PEX extension ID.
    pub fn peer_pex_id(&self) -> Option<u8> {
        self.peer_extensions.get(PEX_EXTENSION_NAME).copied()
    }

    /// Send a PEX message to the peer.
    pub async fn send_pex(&mut self, msg: &PexMessage) -> Result<()> {
        let pex_id = match self.peer_pex_id() {
            Some(id) => id,
            None => return Ok(()), // Peer doesn't support PEX
        };

        let payload = msg.encode();
        self.send(PeerMessage::Extended {
            id: pex_id,
            payload,
        })
        .await
    }

    /// Send a generic extension message to the peer.
    ///
    /// # Arguments
    /// * `extension_id` - The peer's extension ID for the message type
    /// * `payload` - The bencoded message payload
    pub async fn send_extension_message(
        &mut self,
        extension_id: u8,
        payload: Vec<u8>,
    ) -> Result<()> {
        self.send(PeerMessage::Extended {
            id: extension_id,
            payload,
        })
        .await
    }

    /// Check if extension handshake has been completed.
    pub fn extension_handshake_done(&self) -> bool {
        self.extension_handshake_done
    }

    /// Get peer's client identification.
    pub fn peer_client(&self) -> Option<&str> {
        self.peer_client.as_deref()
    }

    /// Get peer's listen port (from extension handshake).
    pub fn peer_listen_port(&self) -> Option<u16> {
        self.peer_listen_port
    }

    /// Get all peer extensions.
    pub fn peer_extensions(&self) -> &HashMap<String, u8> {
        &self.peer_extensions
    }

    /// Check if the connection is encrypted (MSE/PE).
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// Disconnect from the peer
    pub async fn disconnect(mut self) -> Result<()> {
        self.state = ConnectionState::Disconnected;
        self.stream.shutdown().await.ok();
        Ok(())
    }
}

/// Get the client name from a peer ID (Azureus-style)
pub fn peer_id_to_client(peer_id: &[u8; 20]) -> Option<String> {
    if peer_id[0] != b'-' || peer_id[7] != b'-' {
        return None;
    }

    let client_id = std::str::from_utf8(&peer_id[1..3]).ok()?;
    let version = std::str::from_utf8(&peer_id[3..7]).ok()?;

    let client_name = match client_id {
        "AZ" => "Azureus",
        "BC" => "BitComet",
        "BS" => "BTSlave",
        "DE" => "Deluge",
        "LT" => "libtorrent",
        "QD" => "QQDownload",
        "UT" => "uTorrent",
        "TR" => "Transmission",
        "GD" => "Gosh Downloader",
        "qB" => "qBittorrent",
        "AR" => "Arctic",
        "FD" => "Free Download Manager",
        _ => return Some(format!("{} {}", client_id, version)),
    };

    Some(format!("{} {}", client_name, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_encode_decode() {
        // Test keep-alive
        let msg = PeerMessage::KeepAlive;
        let encoded = msg.encode();
        assert_eq!(encoded, vec![0, 0, 0, 0]);

        // Test choke
        let msg = PeerMessage::Choke;
        let encoded = msg.encode();
        assert_eq!(encoded, vec![0, 0, 0, 1, 0]);
        let decoded = PeerMessage::decode(&[0]).unwrap();
        assert_eq!(decoded, PeerMessage::Choke);

        // Test have
        let msg = PeerMessage::Have { piece_index: 42 };
        let encoded = msg.encode();
        let decoded = PeerMessage::decode(&encoded[4..]).unwrap();
        assert_eq!(decoded, msg);

        // Test request
        let msg = PeerMessage::Request {
            index: 1,
            begin: 16384,
            length: 16384,
        };
        let encoded = msg.encode();
        let decoded = PeerMessage::decode(&encoded[4..]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_reserved_bytes() {
        let reserved = ReservedBytes::with_extensions();
        assert!(reserved.supports_extension_protocol());
    }

    #[test]
    fn test_peer_id_to_client() {
        let mut peer_id = [0u8; 20];
        peer_id[0..8].copy_from_slice(b"-UT4000-");
        let client = peer_id_to_client(&peer_id);
        assert_eq!(client, Some("uTorrent 4000".to_string()));

        let mut peer_id = [0u8; 20];
        peer_id[0..8].copy_from_slice(b"-GD0001-");
        let client = peer_id_to_client(&peer_id);
        assert_eq!(client, Some("Gosh Downloader 0001".to_string()));
    }

    #[test]
    fn test_bitfield_parsing() {
        // Simulate receiving a bitfield for 16 pieces where we have pieces 0, 2, 4, 6
        // Binary: 10101010 00000000
        let bitfield = vec![0b10101010, 0b00000000];
        let mut peer_pieces = bitvec![u8, Msb0; 0; 16];

        for (i, byte) in bitfield.iter().enumerate() {
            for bit in 0..8 {
                let piece_idx = i * 8 + bit;
                if piece_idx < 16 {
                    peer_pieces.set(piece_idx, (byte & (0x80 >> bit)) != 0);
                }
            }
        }

        assert!(peer_pieces[0]);
        assert!(!peer_pieces[1]);
        assert!(peer_pieces[2]);
        assert!(!peer_pieces[3]);
        assert!(peer_pieces[4]);
    }
}
