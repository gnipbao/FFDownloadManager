//! Peer Exchange (PEX) protocol implementation (BEP 11).
//!
//! PEX allows peers to exchange known peer addresses, reducing reliance on trackers.
//! It uses the BEP 10 extension protocol with extension name "ut_pex".
//!
//! Message format (bencoded dictionary):
//! - added: compact IPv4 peers (6 bytes each: 4 IP + 2 port)
//! - added.f: flags for added peers (1 byte each)
//! - dropped: compact IPv4 peers that disconnected
//! - added6: compact IPv6 peers (18 bytes each)
//! - dropped6: compact IPv6 peers that disconnected

use std::collections::{BTreeMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::{Duration, Instant};

use crate::error::{EngineError, ProtocolErrorKind, Result};
use crate::torrent::bencode::BencodeValue;

/// Extension name for PEX in BEP 10 handshake.
pub const PEX_EXTENSION_NAME: &str = "ut_pex";

/// PEX flag bits for the added.f field.
pub mod flags {
    /// Peer prefers encrypted connections.
    pub const PREFERS_ENCRYPTION: u8 = 0x01;
    /// Peer is a seeder (has all pieces).
    pub const IS_SEEDER: u8 = 0x02;
    /// Peer supports uTP (UDP transport).
    pub const SUPPORTS_UTP: u8 = 0x04;
    /// Peer supports holepunch extension.
    pub const SUPPORTS_HOLEPUNCH: u8 = 0x08;
    /// Connection was outgoing (we initiated).
    pub const IS_OUTGOING: u8 = 0x10;
}

/// A PEX message containing peer additions and removals.
#[derive(Debug, Clone, Default)]
pub struct PexMessage {
    /// Added IPv4 peers.
    pub added: Vec<SocketAddr>,
    /// Flags for added IPv4 peers (one byte per peer).
    pub added_flags: Vec<u8>,
    /// Dropped IPv4 peers.
    pub dropped: Vec<SocketAddr>,
    /// Added IPv6 peers.
    pub added6: Vec<SocketAddr>,
    /// Flags for added IPv6 peers.
    pub added6_flags: Vec<u8>,
    /// Dropped IPv6 peers.
    pub dropped6: Vec<SocketAddr>,
}

impl PexMessage {
    /// Create a new empty PEX message.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a PEX message from bencoded payload.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let value = BencodeValue::parse_exact(data)?;
        let dict = value.as_dict().ok_or_else(|| {
            EngineError::protocol(ProtocolErrorKind::PexError, "PEX message must be a dict")
        })?;

        // Parse IPv4 added peers
        let added = dict
            .get(b"added".as_slice())
            .and_then(|v| v.as_bytes())
            .map(parse_compact_peers_v4)
            .unwrap_or_default();

        // Parse added flags
        let added_flags = dict
            .get(b"added.f".as_slice())
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec())
            .unwrap_or_default();

        // Parse IPv4 dropped peers
        let dropped = dict
            .get(b"dropped".as_slice())
            .and_then(|v| v.as_bytes())
            .map(parse_compact_peers_v4)
            .unwrap_or_default();

        // Parse IPv6 added peers
        let added6 = dict
            .get(b"added6".as_slice())
            .and_then(|v| v.as_bytes())
            .map(parse_compact_peers_v6)
            .unwrap_or_default();

        // Parse added6 flags
        let added6_flags = dict
            .get(b"added6.f".as_slice())
            .and_then(|v| v.as_bytes())
            .map(|b| b.to_vec())
            .unwrap_or_default();

        // Parse IPv6 dropped peers
        let dropped6 = dict
            .get(b"dropped6".as_slice())
            .and_then(|v| v.as_bytes())
            .map(parse_compact_peers_v6)
            .unwrap_or_default();

        Ok(Self {
            added,
            added_flags,
            dropped,
            added6,
            added6_flags,
            dropped6,
        })
    }

    /// Encode the PEX message to bencoded bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut dict = BTreeMap::new();

        // Separate peers by IP version
        let (v4_added, v6_added): (Vec<_>, Vec<_>) = self.added.iter().partition(|a| a.is_ipv4());
        let (v4_dropped, v6_dropped): (Vec<_>, Vec<_>) =
            self.dropped.iter().partition(|a| a.is_ipv4());

        // Encode IPv4 added peers
        if !v4_added.is_empty() {
            dict.insert(
                b"added".to_vec(),
                BencodeValue::Bytes(encode_compact_peers_v4(&v4_added)),
            );
        }

        // Encode added flags (must match added count)
        if !self.added_flags.is_empty() {
            dict.insert(
                b"added.f".to_vec(),
                BencodeValue::Bytes(self.added_flags.clone()),
            );
        }

        // Encode IPv4 dropped peers
        if !v4_dropped.is_empty() {
            dict.insert(
                b"dropped".to_vec(),
                BencodeValue::Bytes(encode_compact_peers_v4(&v4_dropped)),
            );
        }

        // Encode IPv6 added peers (including any from added6)
        let mut all_v6_added: Vec<_> = v6_added.into_iter().cloned().collect();
        all_v6_added.extend(self.added6.iter().cloned());
        if !all_v6_added.is_empty() {
            dict.insert(
                b"added6".to_vec(),
                BencodeValue::Bytes(encode_compact_peers_v6(
                    &all_v6_added.iter().collect::<Vec<_>>(),
                )),
            );
        }

        // Encode added6 flags
        if !self.added6_flags.is_empty() {
            dict.insert(
                b"added6.f".to_vec(),
                BencodeValue::Bytes(self.added6_flags.clone()),
            );
        }

        // Encode IPv6 dropped peers
        let mut all_v6_dropped: Vec<_> = v6_dropped.into_iter().cloned().collect();
        all_v6_dropped.extend(self.dropped6.iter().cloned());
        if !all_v6_dropped.is_empty() {
            dict.insert(
                b"dropped6".to_vec(),
                BencodeValue::Bytes(encode_compact_peers_v6(
                    &all_v6_dropped.iter().collect::<Vec<_>>(),
                )),
            );
        }

        BencodeValue::Dict(dict).encode()
    }

    /// Check if the message is empty (no peers to share).
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.dropped.is_empty()
            && self.added6.is_empty()
            && self.dropped6.is_empty()
    }

    /// Get all added peers (IPv4 and IPv6).
    pub fn all_added(&self) -> Vec<SocketAddr> {
        let mut all = self.added.clone();
        all.extend(self.added6.iter().cloned());
        all
    }

    /// Get all dropped peers (IPv4 and IPv6).
    pub fn all_dropped(&self) -> Vec<SocketAddr> {
        let mut all = self.dropped.clone();
        all.extend(self.dropped6.iter().cloned());
        all
    }
}

/// Parse compact IPv4 peers (6 bytes per peer: 4 IP + 2 port big-endian).
fn parse_compact_peers_v4(data: &[u8]) -> Vec<SocketAddr> {
    data.chunks_exact(6)
        .map(|chunk| {
            let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
            let port = u16::from_be_bytes([chunk[4], chunk[5]]);
            SocketAddr::V4(SocketAddrV4::new(ip, port))
        })
        .collect()
}

/// Parse compact IPv6 peers (18 bytes per peer: 16 IP + 2 port big-endian).
fn parse_compact_peers_v6(data: &[u8]) -> Vec<SocketAddr> {
    data.chunks_exact(18)
        .map(|chunk| {
            let ip_bytes: [u8; 16] = chunk[0..16].try_into().unwrap();
            let ip = Ipv6Addr::from(ip_bytes);
            let port = u16::from_be_bytes([chunk[16], chunk[17]]);
            SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0))
        })
        .collect()
}

/// Encode peers to compact IPv4 format.
fn encode_compact_peers_v4(peers: &[&SocketAddr]) -> Vec<u8> {
    let mut data = Vec::with_capacity(peers.len() * 6);
    for peer in peers {
        if let SocketAddr::V4(addr) = peer {
            data.extend_from_slice(&addr.ip().octets());
            data.extend_from_slice(&addr.port().to_be_bytes());
        }
    }
    data
}

/// Encode peers to compact IPv6 format.
fn encode_compact_peers_v6(peers: &[&SocketAddr]) -> Vec<u8> {
    let mut data = Vec::with_capacity(peers.len() * 18);
    for peer in peers {
        if let SocketAddr::V6(addr) = peer {
            data.extend_from_slice(&addr.ip().octets());
            data.extend_from_slice(&addr.port().to_be_bytes());
        }
    }
    data
}

/// PEX state tracker for a single peer connection.
///
/// Tracks which peers we've shared with this peer to compute diffs.
pub struct PexState {
    /// Peers we've previously shared with this peer.
    shared_peers: HashSet<SocketAddr>,
    /// Last time we sent a PEX message.
    last_send: Instant,
    /// Minimum interval between PEX messages.
    min_interval: Duration,
    /// Our extension ID for ut_pex (what the remote peer should use).
    pub our_extension_id: u8,
    /// Remote peer's extension ID for ut_pex (what we should use when sending).
    pub peer_extension_id: Option<u8>,
}

impl PexState {
    /// Create a new PEX state with default settings.
    ///
    /// # Arguments
    /// * `our_extension_id` - The extension ID we advertise for ut_pex.
    pub fn new(our_extension_id: u8) -> Self {
        Self {
            shared_peers: HashSet::new(),
            // Allow immediate first send
            last_send: Instant::now() - Duration::from_secs(120),
            min_interval: Duration::from_secs(60),
            our_extension_id,
            peer_extension_id: None,
        }
    }

    /// Set the peer's extension ID for ut_pex after handshake.
    pub fn set_peer_extension_id(&mut self, id: u8) {
        self.peer_extension_id = Some(id);
    }

    /// Check if PEX is supported with this peer.
    pub fn is_supported(&self) -> bool {
        self.peer_extension_id.is_some()
    }

    /// Check if enough time has passed to send a new PEX message.
    pub fn can_send(&self) -> bool {
        self.last_send.elapsed() >= self.min_interval
    }

    /// Build a PEX message with changes since last send.
    ///
    /// Returns None if not enough time has passed or no changes.
    pub fn build_message(&mut self, current_peers: &HashSet<SocketAddr>) -> Option<PexMessage> {
        if !self.can_send() {
            return None;
        }

        // Calculate diff
        let added: Vec<_> = current_peers
            .difference(&self.shared_peers)
            .cloned()
            .collect();
        let dropped: Vec<_> = self
            .shared_peers
            .difference(current_peers)
            .cloned()
            .collect();

        if added.is_empty() && dropped.is_empty() {
            return None;
        }

        // Update state
        self.shared_peers = current_peers.clone();
        self.last_send = Instant::now();

        // Separate by IP version
        let (v4_added, v6_added): (Vec<_>, Vec<_>) = added.into_iter().partition(|a| a.is_ipv4());
        let (v4_dropped, v6_dropped): (Vec<_>, Vec<_>) =
            dropped.into_iter().partition(|a| a.is_ipv4());

        Some(PexMessage {
            added: v4_added,
            added_flags: vec![], // Default flags (could be enhanced)
            dropped: v4_dropped,
            added6: v6_added,
            added6_flags: vec![],
            dropped6: v6_dropped,
        })
    }

    /// Process received PEX message and return new peers to connect to.
    ///
    /// Filters out peers we already know about.
    pub fn process_received(
        &self,
        msg: &PexMessage,
        known_peers: &HashSet<SocketAddr>,
    ) -> Vec<SocketAddr> {
        msg.all_added()
            .into_iter()
            .filter(|addr| !known_peers.contains(addr))
            .collect()
    }

    /// Reset state (e.g., after reconnection).
    pub fn reset(&mut self) {
        self.shared_peers.clear();
        self.last_send = Instant::now() - Duration::from_secs(120);
    }
}

/// Build the extension handshake dictionary for advertising PEX support.
///
/// Returns the bencoded handshake message to send as Extended message id=0.
pub fn build_extension_handshake(
    pex_id: u8,
    metadata_id: Option<u8>,
    listen_port: Option<u16>,
) -> Vec<u8> {
    let mut m = BTreeMap::new();
    m.insert(b"ut_pex".to_vec(), BencodeValue::Integer(pex_id as i64));
    if let Some(metadata_id) = metadata_id {
        m.insert(
            b"ut_metadata".to_vec(),
            BencodeValue::Integer(metadata_id as i64),
        );
    }

    let mut dict = BTreeMap::new();
    dict.insert(b"m".to_vec(), BencodeValue::Dict(m));

    // Optional: advertise our listen port
    if let Some(port) = listen_port {
        dict.insert(b"p".to_vec(), BencodeValue::Integer(port as i64));
    }

    // Optional: client identification
    dict.insert(
        b"v".to_vec(),
        BencodeValue::Bytes(b"gosh-dl/0.1.0".to_vec()),
    );

    BencodeValue::Dict(dict).encode()
}

/// Parse the extension handshake to extract supported extensions.
///
/// Returns a map of extension name to extension ID.
pub fn parse_extension_handshake(data: &[u8]) -> Result<ExtensionHandshake> {
    let value = BencodeValue::parse_exact(data)?;
    let dict = value.as_dict().ok_or_else(|| {
        EngineError::protocol(
            ProtocolErrorKind::PexError,
            "Extension handshake must be a dict",
        )
    })?;

    let mut extensions = std::collections::HashMap::new();

    // Parse the "m" dictionary containing extension mappings
    if let Some(m) = dict.get(b"m".as_slice()).and_then(|v| v.as_dict()) {
        for (key, value) in m {
            if let Some(id) = value.as_uint() {
                let name = String::from_utf8_lossy(key).to_string();
                extensions.insert(name, id as u8);
            }
        }
    }

    // Parse optional fields
    let listen_port = dict
        .get(b"p".as_slice())
        .and_then(|v| v.as_uint())
        .map(|p| p as u16);

    let client = dict
        .get(b"v".as_slice())
        .and_then(|v| v.as_string())
        .map(|s| s.to_string());

    let reqq = dict
        .get(b"reqq".as_slice())
        .and_then(|v| v.as_uint())
        .map(|r| r as usize);

    Ok(ExtensionHandshake {
        extensions,
        listen_port,
        client,
        request_queue_size: reqq,
    })
}

/// Parsed extension handshake information.
#[derive(Debug, Clone)]
pub struct ExtensionHandshake {
    /// Map of extension name to extension ID.
    pub extensions: std::collections::HashMap<String, u8>,
    /// Peer's listen port (if advertised).
    pub listen_port: Option<u16>,
    /// Client identification string.
    pub client: Option<String>,
    /// Maximum number of outstanding requests.
    pub request_queue_size: Option<usize>,
}

impl ExtensionHandshake {
    /// Get the extension ID for ut_pex.
    pub fn pex_id(&self) -> Option<u8> {
        self.extensions.get(PEX_EXTENSION_NAME).copied()
    }

    /// Check if peer supports PEX.
    pub fn supports_pex(&self) -> bool {
        self.pex_id().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_compact_peers_v4() {
        // Two peers: 127.0.0.1:6881 and 192.168.1.1:51413
        let data = [
            127, 0, 0, 1, 0x1a, 0xe1, // 127.0.0.1:6881
            192, 168, 1, 1, 0xc8, 0xd5, // 192.168.1.1:51413
        ];
        let peers = parse_compact_peers_v4(&data);
        assert_eq!(peers.len(), 2);
        assert_eq!(
            peers[0],
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 6881))
        );
        assert_eq!(
            peers[1],
            SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 51413))
        );
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut msg = PexMessage::new();
        msg.added.push(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            6881,
        )));
        msg.added.push(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 2),
            6882,
        )));
        msg.dropped.push(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 3),
            6883,
        )));

        let encoded = msg.encode();
        let decoded = PexMessage::parse(&encoded).unwrap();

        assert_eq!(decoded.added.len(), 2);
        assert_eq!(decoded.dropped.len(), 1);
        assert!(decoded.added.contains(&SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            6881
        ))));
    }

    #[test]
    fn test_extension_handshake() {
        let handshake = build_extension_handshake(1, None, Some(6881));
        let parsed = parse_extension_handshake(&handshake).unwrap();

        assert!(parsed.supports_pex());
        assert_eq!(parsed.pex_id(), Some(1));
        assert_eq!(parsed.listen_port, Some(6881));
        assert_eq!(parsed.client, Some("gosh-dl/0.1.0".to_string()));
    }

    #[test]
    fn test_pex_state_build_message() {
        let mut state = PexState::new(1);

        let mut current = HashSet::new();
        current.insert(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            6881,
        )));
        current.insert(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 2),
            6882,
        )));

        // First call should produce a message
        let msg = state.build_message(&current).unwrap();
        assert_eq!(msg.added.len(), 2);
        assert_eq!(msg.dropped.len(), 0);

        // Immediate second call should return None (interval not passed)
        assert!(state.build_message(&current).is_none());
    }

    #[test]
    fn test_pex_state_diff() {
        let mut state = PexState::new(1);
        state.min_interval = Duration::from_millis(0); // Disable for test

        // Initial peers
        let mut current = HashSet::new();
        current.insert(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            6881,
        )));
        current.insert(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 2),
            6882,
        )));

        let _ = state.build_message(&current);

        // Change peers: remove one, add one
        current.remove(&SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            6881,
        )));
        current.insert(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 3),
            6883,
        )));

        let msg = state.build_message(&current).unwrap();
        assert_eq!(msg.added.len(), 1);
        assert_eq!(msg.dropped.len(), 1);
        assert!(msg.added.contains(&SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 3),
            6883
        ))));
        assert!(msg.dropped.contains(&SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            6881
        ))));
    }

    #[test]
    fn test_pex_message_empty() {
        let msg = PexMessage::new();
        assert!(msg.is_empty());

        let mut msg2 = PexMessage::new();
        msg2.added.push(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(10, 0, 0, 1),
            6881,
        )));
        assert!(!msg2.is_empty());
    }
}
