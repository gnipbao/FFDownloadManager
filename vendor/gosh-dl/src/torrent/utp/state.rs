//! uTP Connection State Machine
//!
//! This module defines the states a uTP connection can be in and
//! the transitions between them.

use std::time::Instant;

use super::packet::PacketType;

/// uTP connection states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Initial state, no connection
    Idle,

    /// SYN sent, waiting for SYN-ACK (initiator)
    SynSent,

    /// SYN received, waiting to send SYN-ACK (responder)
    SynRecv,

    /// Connection established, data transfer active
    Connected,

    /// FIN sent, waiting for ACK
    FinSent,

    /// Waiting for our FIN to be ACKed after receiving peer's FIN
    Closing,

    /// Connection closed normally
    Closed,

    /// Connection reset by peer
    Reset,

    /// Connection timed out
    TimedOut,
}

impl ConnectionState {
    /// Check if the connection is in a state where data can be sent
    pub fn can_send_data(&self) -> bool {
        matches!(self, Self::Connected | Self::FinSent)
    }

    /// Check if the connection is in a state where data can be received
    pub fn can_receive_data(&self) -> bool {
        matches!(
            self,
            Self::Connected | Self::SynRecv | Self::FinSent | Self::Closing
        )
    }

    /// Check if the connection is closed (terminal state)
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed | Self::Reset | Self::TimedOut)
    }

    /// Check if connection is in progress (handshake or connected)
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::SynSent | Self::SynRecv | Self::Connected | Self::FinSent | Self::Closing
        )
    }
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "IDLE"),
            Self::SynSent => write!(f, "SYN_SENT"),
            Self::SynRecv => write!(f, "SYN_RECV"),
            Self::Connected => write!(f, "CONNECTED"),
            Self::FinSent => write!(f, "FIN_SENT"),
            Self::Closing => write!(f, "CLOSING"),
            Self::Closed => write!(f, "CLOSED"),
            Self::Reset => write!(f, "RESET"),
            Self::TimedOut => write!(f, "TIMED_OUT"),
        }
    }
}

/// Pending packet for retransmission
#[derive(Debug, Clone)]
pub struct PendingPacket {
    /// Sequence number
    pub seq_nr: u16,

    /// Packet data (encoded)
    pub data: Vec<u8>,

    /// Original payload (for reconstruction)
    pub payload: Vec<u8>,

    /// Time the packet was originally sent
    pub first_sent: Instant,

    /// Time of last send attempt
    pub last_sent: Instant,

    /// Number of retransmissions
    pub retransmits: u32,

    /// Size for congestion control
    pub size: u32,

    /// Original packet type — a retransmitted FIN must stay a FIN
    pub packet_type: PacketType,
}

impl PendingPacket {
    pub fn new(seq_nr: u16, data: Vec<u8>, payload: Vec<u8>) -> Self {
        let now = Instant::now();
        let size = data.len() as u32;
        Self {
            seq_nr,
            data,
            payload,
            first_sent: now,
            last_sent: now,
            retransmits: 0,
            size,
            packet_type: PacketType::Data,
        }
    }

    pub fn mark_retransmit(&mut self) {
        self.last_sent = Instant::now();
        self.retransmits += 1;
    }
}

/// Connection statistics
#[derive(Debug, Default, Clone)]
pub struct ConnectionStats {
    /// Packets sent
    pub packets_sent: u64,

    /// Packets received
    pub packets_received: u64,

    /// Bytes sent (payload only)
    pub bytes_sent: u64,

    /// Bytes received (payload only)
    pub bytes_received: u64,

    /// Packets retransmitted
    pub retransmits: u64,

    /// Duplicate ACKs received
    pub duplicate_acks: u64,

    /// Timeouts
    pub timeouts: u64,

    /// Packets lost (inferred)
    pub packets_lost: u64,
}

impl ConnectionStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_sent(&mut self, bytes: u64) {
        self.packets_sent += 1;
        self.bytes_sent += bytes;
    }

    pub fn record_received(&mut self, bytes: u64) {
        self.packets_received += 1;
        self.bytes_received += bytes;
    }

    pub fn record_retransmit(&mut self) {
        self.retransmits += 1;
    }

    pub fn record_timeout(&mut self) {
        self.timeouts += 1;
    }

    pub fn record_duplicate_ack(&mut self) {
        self.duplicate_acks += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_state_transitions() {
        // Initial state
        let state = ConnectionState::Idle;
        assert!(!state.can_send_data());
        assert!(!state.is_closed());

        // After sending SYN
        let state = ConnectionState::SynSent;
        assert!(!state.can_send_data());
        assert!(state.is_active());

        // Connected
        let state = ConnectionState::Connected;
        assert!(state.can_send_data());
        assert!(state.can_receive_data());
        assert!(state.is_active());

        // Closed
        let state = ConnectionState::Closed;
        assert!(!state.can_send_data());
        assert!(state.is_closed());
    }

    #[test]
    fn test_pending_packet() {
        let pkt = PendingPacket::new(100, vec![1, 2, 3], vec![1]);
        assert_eq!(pkt.seq_nr, 100);
        assert_eq!(pkt.retransmits, 0);
    }

    #[test]
    fn test_connection_stats() {
        let mut stats = ConnectionStats::new();
        stats.record_sent(100);
        stats.record_received(200);

        assert_eq!(stats.packets_sent, 1);
        assert_eq!(stats.bytes_sent, 100);
        assert_eq!(stats.packets_received, 1);
        assert_eq!(stats.bytes_received, 200);
    }
}
