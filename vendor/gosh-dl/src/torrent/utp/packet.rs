//! uTP Packet Encoding/Decoding (BEP 29)
//!
//! This module implements the uTP packet format as defined in BEP 29.
//! uTP uses a 20-byte header followed by optional extensions and payload.

use std::io;

/// uTP packet header size
pub const HEADER_SIZE: usize = 20;

/// Maximum uTP packet size (MTU - IP header - UDP header)
/// Typically 1400 bytes to avoid fragmentation
pub const MAX_PACKET_SIZE: usize = 1400;

/// Maximum payload size per packet
pub const MAX_PAYLOAD_SIZE: usize = MAX_PACKET_SIZE - HEADER_SIZE;

/// uTP protocol version
pub const UTP_VERSION: u8 = 1;

/// Packet type values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    /// Regular data packet
    Data = 0,
    /// Connection teardown
    Fin = 1,
    /// Acknowledgment (no payload)
    State = 2,
    /// Connection reset
    Reset = 3,
    /// Connection initiation
    Syn = 4,
}

impl TryFrom<u8> for PacketType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Data),
            1 => Ok(Self::Fin),
            2 => Ok(Self::State),
            3 => Ok(Self::Reset),
            4 => Ok(Self::Syn),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Invalid packet type: {}", value),
            )),
        }
    }
}

/// Extension type values
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExtensionType {
    /// No extension / end of extensions
    None = 0,
    /// Selective ACK extension
    SelectiveAck = 1,
}

impl TryFrom<u8> for ExtensionType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::SelectiveAck),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unknown extension type: {}", value),
            )),
        }
    }
}

/// Selective ACK extension data
#[derive(Debug, Clone, Default)]
pub struct SelectiveAck {
    /// Bitmask of received packets after ack_nr
    /// Each bit represents a packet, bit 0 = ack_nr + 2, bit 1 = ack_nr + 3, etc.
    pub bitmask: Vec<u8>,
}

impl SelectiveAck {
    /// Create a new SelectiveAck with the given bitmask
    pub fn new(bitmask: Vec<u8>) -> Self {
        Self { bitmask }
    }

    /// Check if a specific packet (relative to ack_nr) is acknowledged
    /// packet_offset is relative to ack_nr + 2
    pub fn is_acked(&self, packet_offset: u16) -> bool {
        let byte_idx = packet_offset as usize / 8;
        let bit_idx = packet_offset as usize % 8;

        if byte_idx >= self.bitmask.len() {
            return false;
        }

        // Bits are in reverse order within each byte (MSB first)
        (self.bitmask[byte_idx] & (0x80 >> bit_idx)) != 0
    }

    /// Set a packet as acknowledged
    pub fn set_acked(&mut self, packet_offset: u16) {
        let byte_idx = packet_offset as usize / 8;
        let bit_idx = packet_offset as usize % 8;

        // Extend bitmask if needed
        while self.bitmask.len() <= byte_idx {
            self.bitmask.push(0);
        }

        self.bitmask[byte_idx] |= 0x80 >> bit_idx;
    }

    /// Encode the extension data
    pub fn encode(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(2 + self.bitmask.len());
        data.push(ExtensionType::None as u8); // Next extension (none)
        data.push(self.bitmask.len() as u8); // Length
        data.extend_from_slice(&self.bitmask);
        data
    }
}

/// uTP packet header and data
#[derive(Debug, Clone)]
pub struct Packet {
    /// Packet type (4 bits) combined with version (4 bits)
    pub packet_type: PacketType,

    /// Connection ID (identifies the connection)
    pub connection_id: u16,

    /// Microsecond timestamp
    pub timestamp_us: u32,

    /// Timestamp difference (remote timestamp - our timestamp)
    pub timestamp_diff_us: u32,

    /// Receive window size (in bytes)
    pub wnd_size: u32,

    /// Sequence number
    pub seq_nr: u16,

    /// Acknowledgment number (last received seq_nr)
    pub ack_nr: u16,

    /// Optional selective ACK extension
    pub selective_ack: Option<SelectiveAck>,

    /// Packet payload
    pub payload: Vec<u8>,
}

impl Packet {
    /// Create a new packet
    pub fn new(packet_type: PacketType, connection_id: u16, seq_nr: u16, ack_nr: u16) -> Self {
        Self {
            packet_type,
            connection_id,
            timestamp_us: 0,
            timestamp_diff_us: 0,
            wnd_size: 0,
            seq_nr,
            ack_nr,
            selective_ack: None,
            payload: Vec::new(),
        }
    }

    /// Create a SYN packet to initiate a connection
    pub fn syn(connection_id: u16, seq_nr: u16) -> Self {
        Self::new(PacketType::Syn, connection_id, seq_nr, 0)
    }

    /// Create a STATE packet (acknowledgment)
    pub fn state(connection_id: u16, seq_nr: u16, ack_nr: u16) -> Self {
        Self::new(PacketType::State, connection_id, seq_nr, ack_nr)
    }

    /// Create a DATA packet
    pub fn data(connection_id: u16, seq_nr: u16, ack_nr: u16, payload: Vec<u8>) -> Self {
        let mut pkt = Self::new(PacketType::Data, connection_id, seq_nr, ack_nr);
        pkt.payload = payload;
        pkt
    }

    /// Create a FIN packet
    pub fn fin(connection_id: u16, seq_nr: u16, ack_nr: u16) -> Self {
        Self::new(PacketType::Fin, connection_id, seq_nr, ack_nr)
    }

    /// Create a RESET packet
    pub fn reset(connection_id: u16, seq_nr: u16, ack_nr: u16) -> Self {
        Self::new(PacketType::Reset, connection_id, seq_nr, ack_nr)
    }

    /// Set timestamps
    pub fn with_timestamps(mut self, timestamp_us: u32, timestamp_diff_us: u32) -> Self {
        self.timestamp_us = timestamp_us;
        self.timestamp_diff_us = timestamp_diff_us;
        self
    }

    /// Set window size
    pub fn with_window(mut self, wnd_size: u32) -> Self {
        self.wnd_size = wnd_size;
        self
    }

    /// Set selective ACK
    pub fn with_selective_ack(mut self, sack: SelectiveAck) -> Self {
        self.selective_ack = Some(sack);
        self
    }

    /// Encode the packet to bytes
    pub fn encode(&self) -> Vec<u8> {
        let has_ext = self.selective_ack.is_some();
        let ext_type = if has_ext {
            ExtensionType::SelectiveAck as u8
        } else {
            ExtensionType::None as u8
        };

        let mut buf = Vec::with_capacity(HEADER_SIZE + self.payload.len() + 32);

        // Byte 0: type (4 bits) | version (4 bits)
        buf.push((self.packet_type as u8) << 4 | UTP_VERSION);

        // Byte 1: extension type
        buf.push(ext_type);

        // Bytes 2-3: connection_id
        buf.extend_from_slice(&self.connection_id.to_be_bytes());

        // Bytes 4-7: timestamp_us
        buf.extend_from_slice(&self.timestamp_us.to_be_bytes());

        // Bytes 8-11: timestamp_diff_us
        buf.extend_from_slice(&self.timestamp_diff_us.to_be_bytes());

        // Bytes 12-15: wnd_size
        buf.extend_from_slice(&self.wnd_size.to_be_bytes());

        // Bytes 16-17: seq_nr
        buf.extend_from_slice(&self.seq_nr.to_be_bytes());

        // Bytes 18-19: ack_nr
        buf.extend_from_slice(&self.ack_nr.to_be_bytes());

        // Extensions
        if let Some(ref sack) = self.selective_ack {
            buf.extend_from_slice(&sack.encode());
        }

        // Payload
        buf.extend_from_slice(&self.payload);

        buf
    }

    /// Decode a packet from bytes
    pub fn decode(data: &[u8]) -> io::Result<Self> {
        if data.len() < HEADER_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Packet too short: {} bytes", data.len()),
            ));
        }

        // Parse header
        let type_ver = data[0];
        let packet_type = PacketType::try_from(type_ver >> 4)?;
        let version = type_ver & 0x0F;

        if version != UTP_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Unsupported uTP version: {}", version),
            ));
        }

        let ext_type = data[1];
        let connection_id = u16::from_be_bytes([data[2], data[3]]);
        let timestamp_us = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let timestamp_diff_us = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let wnd_size = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
        let seq_nr = u16::from_be_bytes([data[16], data[17]]);
        let ack_nr = u16::from_be_bytes([data[18], data[19]]);

        // Parse extensions
        let mut offset = HEADER_SIZE;
        let mut selective_ack = None;
        let mut next_ext = ext_type;

        while next_ext != ExtensionType::None as u8 && offset + 2 <= data.len() {
            let ext = next_ext;
            next_ext = data[offset];
            let ext_len = data[offset + 1] as usize;
            offset += 2;

            if offset + ext_len > data.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Extension data truncated",
                ));
            }

            // BEP 29 requires tolerating unknown extension types: the
            // (type, length) framing lets us skip what we don't understand.
            if ext == ExtensionType::SelectiveAck as u8 {
                selective_ack = Some(SelectiveAck::new(data[offset..offset + ext_len].to_vec()));
            }

            offset += ext_len;
        }

        // Remaining data is payload
        let payload = if offset < data.len() {
            data[offset..].to_vec()
        } else {
            Vec::new()
        };

        Ok(Self {
            packet_type,
            connection_id,
            timestamp_us,
            timestamp_diff_us,
            wnd_size,
            seq_nr,
            ack_nr,
            selective_ack,
            payload,
        })
    }

    /// Check if this is a SYN packet
    pub fn is_syn(&self) -> bool {
        self.packet_type == PacketType::Syn
    }

    /// Check if this is a FIN packet
    pub fn is_fin(&self) -> bool {
        self.packet_type == PacketType::Fin
    }

    /// Check if this is a RESET packet
    pub fn is_reset(&self) -> bool {
        self.packet_type == PacketType::Reset
    }

    /// Check if this is a STATE (ACK) packet
    pub fn is_state(&self) -> bool {
        self.packet_type == PacketType::State
    }

    /// Check if this is a DATA packet
    pub fn is_data(&self) -> bool {
        self.packet_type == PacketType::Data
    }
}

/// Get current timestamp in microseconds (truncated to 32 bits)
pub fn timestamp_us() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (duration.as_micros() & 0xFFFFFFFF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_encode_decode() {
        let pkt = Packet::syn(12345, 100)
            .with_timestamps(1000000, 500)
            .with_window(65535);

        let encoded = pkt.encode();
        let decoded = Packet::decode(&encoded).unwrap();

        assert_eq!(decoded.packet_type, PacketType::Syn);
        assert_eq!(decoded.connection_id, 12345);
        assert_eq!(decoded.seq_nr, 100);
        assert_eq!(decoded.timestamp_us, 1000000);
        assert_eq!(decoded.timestamp_diff_us, 500);
        assert_eq!(decoded.wnd_size, 65535);
    }

    #[test]
    fn test_data_packet() {
        let payload = b"Hello, uTP!".to_vec();
        let pkt = Packet::data(1234, 5, 3, payload.clone());

        let encoded = pkt.encode();
        let decoded = Packet::decode(&encoded).unwrap();

        assert_eq!(decoded.packet_type, PacketType::Data);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn test_selective_ack() {
        let mut sack = SelectiveAck::default();
        sack.set_acked(0); // ack_nr + 2
        sack.set_acked(2); // ack_nr + 4
        sack.set_acked(8); // ack_nr + 10

        assert!(sack.is_acked(0));
        assert!(!sack.is_acked(1));
        assert!(sack.is_acked(2));
        assert!(sack.is_acked(8));
        assert!(!sack.is_acked(9));
    }

    #[test]
    fn test_packet_with_sack() {
        let mut sack = SelectiveAck::default();
        sack.set_acked(0);
        sack.set_acked(2);

        let pkt = Packet::state(1234, 10, 5).with_selective_ack(sack);

        let encoded = pkt.encode();
        let decoded = Packet::decode(&encoded).unwrap();

        assert!(decoded.selective_ack.is_some());
        let decoded_sack = decoded.selective_ack.unwrap();
        assert!(decoded_sack.is_acked(0));
        assert!(!decoded_sack.is_acked(1));
        assert!(decoded_sack.is_acked(2));
    }

    #[test]
    fn test_packet_type_conversion() {
        assert_eq!(PacketType::try_from(0).unwrap(), PacketType::Data);
        assert_eq!(PacketType::try_from(1).unwrap(), PacketType::Fin);
        assert_eq!(PacketType::try_from(2).unwrap(), PacketType::State);
        assert_eq!(PacketType::try_from(3).unwrap(), PacketType::Reset);
        assert_eq!(PacketType::try_from(4).unwrap(), PacketType::Syn);
        assert!(PacketType::try_from(5).is_err());
    }
}
