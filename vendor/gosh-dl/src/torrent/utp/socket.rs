//! uTP Socket Implementation
//!
//! This module implements a single uTP connection with reliable,
//! ordered delivery over UDP (BEP 29).
//!
//! # Architecture
//!
//! Each socket owns a background **driver task** that processes every
//! incoming packet (delivered by the [`UtpMux`]), sends ACKs, runs the
//! retransmission timer, and feeds delay samples into the LEDBAT congestion
//! controller. `read()`/`write_all()` only interact with shared state and a
//! [`Notify`], so writes make progress while nobody is reading and vice
//! versa — the failure mode of the previous design, where ACKs were only
//! processed inside `read()`.
//!
//! # Connection IDs (BEP 29)
//!
//! The initiator picks `conn_id_recv = C` (random) and `conn_id_send =
//! C + 1`. The SYN carries `C`; every later packet a side sends carries its
//! own `conn_id_send`. The accepting side therefore uses `conn_id_recv =
//! C + 1`, `conn_id_send = C`.
//!
//! [`UtpMux`]: super::multiplexer::UtpMux

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::timeout;

use super::congestion::LedbatController;
use super::congestion::MSS;
use super::packet::{timestamp_us, Packet, PacketType, SelectiveAck, MAX_PAYLOAD_SIZE};
use super::state::{ConnectionState, ConnectionStats, PendingPacket};

/// Maximum number of retransmissions before giving up
const MAX_RETRANSMITS: u32 = 8;

/// Receive buffer size
const RECV_BUFFER_SIZE: usize = 1024 * 1024; // 1MB

/// Maximum out-of-order packets to buffer
const MAX_OOO_PACKETS: usize = 256;

/// Driver tick interval (retransmit checks, delayed ACKs)
const DRIVER_TICK: Duration = Duration::from_millis(50);

/// Overall inactivity timeout for a blocked write
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// uTP socket configuration
#[derive(Debug, Clone)]
pub struct UtpConfig {
    /// Enable selective ACK extension
    pub enable_sack: bool,

    /// Maximum window size (bytes)
    pub max_window_size: u32,

    /// Initial receive window (bytes)
    pub recv_window: u32,

    /// LEDBAT target delay in microseconds
    pub target_delay_us: u32,
}

impl Default for UtpConfig {
    fn default() -> Self {
        Self {
            enable_sack: true,
            max_window_size: 1024 * 1024,
            recv_window: 1024 * 1024,
            target_delay_us: super::congestion::TARGET_DELAY_US,
        }
    }
}

/// Channel for sending packets to the multiplexer
pub type PacketSender = mpsc::Sender<(Vec<u8>, SocketAddr)>;

/// Channel for receiving packets from the multiplexer
pub type PacketReceiver = mpsc::Receiver<Packet>;

/// Internal state for a uTP socket
pub struct UtpSocketInner {
    /// Remote peer address
    pub remote_addr: SocketAddr,

    /// Connection ID for sending (remote expects this)
    pub send_conn_id: u16,

    /// Connection ID for receiving (we expect this)
    pub recv_conn_id: u16,

    /// Current connection state
    pub state: ConnectionState,

    /// Our sequence number (next to send)
    pub seq_nr: u16,

    /// Their last in-order sequence number (what we acknowledge)
    pub ack_nr: u16,

    /// Last ACK value we sent (to detect when a new ACK is due)
    pub last_ack_sent: u16,

    /// Congestion controller
    pub congestion: LedbatController,

    /// Packets awaiting acknowledgment
    pub pending_packets: BTreeMap<u16, PendingPacket>,

    /// Out-of-order received packets
    pub ooo_packets: BTreeMap<u16, Vec<u8>>,

    /// Receive buffer (ordered data ready for reading)
    pub recv_buffer: VecDeque<u8>,

    /// Receive window size we advertise
    pub recv_window: u32,

    /// Remote's advertised window size
    pub remote_window: u32,

    /// Last computed "their timestamp minus ours" to echo back as
    /// timestamp_diff_us — the peer's LEDBAT needs it, as ours needs theirs
    pub reply_micro: u32,

    /// FIN seq observed but not yet reached in order
    pub pending_fin: Option<u16>,

    /// Time of last received packet
    pub last_recv_time: Instant,

    /// Time of last sent packet
    pub last_send_time: Instant,

    /// Connection statistics
    pub stats: ConnectionStats,

    /// Configuration
    pub config: UtpConfig,

    /// Channel to send packets
    pub packet_tx: PacketSender,

    /// FIN received from peer (and reached in order)
    pub fin_received: bool,

    /// FIN sent to peer
    pub fin_sent: bool,
}

impl UtpSocketInner {
    /// Create a new socket for an outgoing connection.
    ///
    /// `conn_id` is our receive ID (the SYN carries it); we send with
    /// `conn_id + 1`.
    pub fn new_outgoing(
        remote_addr: SocketAddr,
        conn_id: u16,
        packet_tx: PacketSender,
        config: UtpConfig,
    ) -> Self {
        Self::new(
            remote_addr,
            conn_id.wrapping_add(1),
            conn_id,
            ConnectionState::Idle,
            1,
            0,
            packet_tx,
            config,
        )
    }

    /// Create a new socket for an incoming connection.
    ///
    /// `syn_conn_id` is the connection ID carried by the SYN (the
    /// initiator's receive ID): we send with it and receive on it + 1.
    pub fn new_incoming(
        remote_addr: SocketAddr,
        syn_conn_id: u16,
        peer_seq_nr: u16,
        packet_tx: PacketSender,
        config: UtpConfig,
    ) -> Self {
        Self::new(
            remote_addr,
            syn_conn_id,
            syn_conn_id.wrapping_add(1),
            ConnectionState::SynRecv,
            rand::random::<u16>().max(1),
            peer_seq_nr,
            packet_tx,
            config,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        remote_addr: SocketAddr,
        send_conn_id: u16,
        recv_conn_id: u16,
        state: ConnectionState,
        seq_nr: u16,
        ack_nr: u16,
        packet_tx: PacketSender,
        config: UtpConfig,
    ) -> Self {
        let now = Instant::now();
        let mut congestion = LedbatController::new();
        congestion.set_params(config.max_window_size, config.target_delay_us);
        Self {
            remote_addr,
            send_conn_id,
            recv_conn_id,
            state,
            seq_nr,
            ack_nr,
            last_ack_sent: ack_nr,
            congestion,
            pending_packets: BTreeMap::new(),
            ooo_packets: BTreeMap::new(),
            recv_buffer: VecDeque::with_capacity(RECV_BUFFER_SIZE),
            recv_window: config.recv_window,
            // Optimistic until the peer's first packet tells us its window;
            // send_data still clamps to the congestion window.
            remote_window: 64 * 1024,
            reply_micro: 0,
            pending_fin: None,
            last_recv_time: now,
            last_send_time: now,
            stats: ConnectionStats::new(),
            config,
            packet_tx,
            fin_received: false,
            fin_sent: false,
        }
    }

    /// Start the connection (send SYN)
    pub async fn connect(&mut self) -> io::Result<()> {
        if self.state != ConnectionState::Idle {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Connection already started",
            ));
        }

        self.state = ConnectionState::SynSent;
        let pkt = self.build_packet(PacketType::Syn, Vec::new());
        self.send_packet(pkt).await
    }

    /// Accept an incoming connection (send SYN-ACK)
    pub async fn accept(&mut self) -> io::Result<()> {
        if self.state != ConnectionState::SynRecv {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "No incoming connection to accept",
            ));
        }

        let pkt = self.build_packet(PacketType::State, Vec::new());
        self.send_packet_direct(pkt).await?;
        self.state = ConnectionState::Connected;
        Ok(())
    }

    /// Process a received packet
    pub async fn process_packet(&mut self, pkt: Packet) -> io::Result<()> {
        self.last_recv_time = Instant::now();
        if pkt.wnd_size > 0 || pkt.is_state() || pkt.is_data() {
            self.remote_window = pkt.wnd_size;
        }
        self.stats.packets_received += 1;

        // Echo material for the peer's LEDBAT: our current clock minus the
        // packet's send timestamp.
        if pkt.timestamp_us != 0 {
            self.reply_micro = timestamp_us().wrapping_sub(pkt.timestamp_us);
        }

        match self.state {
            ConnectionState::SynSent => {
                // Expecting SYN-ACK (STATE packet acking our SYN)
                if pkt.is_state() {
                    // The STATE's seq_nr is the peer's *next* data sequence
                    // number; nothing has been received yet, so our ack_nr
                    // must be one less or the first data packet is dropped
                    // as a duplicate.
                    self.ack_nr = pkt.seq_nr.wrapping_sub(1);
                    self.last_ack_sent = self.ack_nr;
                    self.process_acks(
                        pkt.ack_nr,
                        pkt.selective_ack.as_ref(),
                        pkt.timestamp_diff_us,
                    );
                    self.state = ConnectionState::Connected;
                } else if pkt.is_reset() {
                    self.state = ConnectionState::Reset;
                }
            }

            ConnectionState::Connected | ConnectionState::FinSent => {
                if pkt.is_reset() {
                    self.state = ConnectionState::Reset;
                    return Ok(());
                }

                if pkt.is_syn() {
                    // Retransmitted SYN (our SYN-ACK was lost): re-ack it.
                    let ack = self.build_packet(PacketType::State, Vec::new());
                    self.send_packet_direct(ack).await?;
                    return Ok(());
                }

                // Process ACKs
                self.process_acks(
                    pkt.ack_nr,
                    pkt.selective_ack.as_ref(),
                    pkt.timestamp_diff_us,
                );

                // Process data
                let is_data = pkt.is_data();
                let is_fin = pkt.is_fin();
                let seq_nr = pkt.seq_nr;
                let had_payload = !pkt.payload.is_empty();
                if (is_data || is_fin) && had_payload {
                    self.receive_data(seq_nr, pkt.payload)?;
                }

                if is_fin {
                    // A FIN is ordered like data: it only takes effect once
                    // every sequence number before it has been received.
                    if had_payload {
                        self.pending_fin = Some(seq_nr);
                    } else if seq_nr == self.ack_nr.wrapping_add(1) {
                        self.ack_nr = seq_nr;
                        self.fin_received = true;
                    } else {
                        self.pending_fin = Some(seq_nr);
                    }
                }

                // A buffered FIN becomes effective once in order
                if let Some(fin_seq) = self.pending_fin {
                    if fin_seq == self.ack_nr.wrapping_add(1) || fin_seq == self.ack_nr {
                        if fin_seq == self.ack_nr.wrapping_add(1) {
                            self.ack_nr = fin_seq;
                        }
                        self.fin_received = true;
                        self.pending_fin = None;
                    }
                }

                if self.fin_received {
                    // ACK the FIN promptly
                    let ack = self.build_packet(PacketType::State, Vec::new());
                    self.send_packet_direct(ack).await?;
                    if self.fin_sent {
                        self.state = ConnectionState::Closed;
                    } else {
                        self.state = ConnectionState::Closing;
                    }
                }

                if is_data && self.state.can_receive_data() {
                    // Re-ack duplicates when an earlier ACK was lost, and
                    // report SACK gaps even if the cumulative ACK is unchanged.
                    let ack = self.build_packet(PacketType::State, Vec::new());
                    self.send_packet_direct(ack).await?;
                }

                // Our FIN fully acknowledged?
                if self.fin_sent
                    && self.state == ConnectionState::FinSent
                    && self.pending_packets.is_empty()
                {
                    self.state = if self.fin_received {
                        ConnectionState::Closed
                    } else {
                        // Keep the socket alive to re-ack until dropped
                        ConnectionState::FinSent
                    };
                }
            }

            ConnectionState::Closing => {
                if pkt.is_reset() {
                    self.state = ConnectionState::Reset;
                    return Ok(());
                }
                self.process_acks(
                    pkt.ack_nr,
                    pkt.selective_ack.as_ref(),
                    pkt.timestamp_diff_us,
                );
                if self.pending_packets.is_empty() && self.fin_sent {
                    self.state = ConnectionState::Closed;
                }
            }

            _ => {}
        }

        Ok(())
    }

    /// Process acknowledgments; `delay_us` is the packet's timestamp_diff —
    /// the peer's measurement of our one-way delay, i.e. our LEDBAT input.
    fn process_acks(&mut self, ack_nr: u16, sack: Option<&SelectiveAck>, delay_us: u32) {
        // Remove all packets up to and including ack_nr
        let to_remove: Vec<u16> = self
            .pending_packets
            .keys()
            .copied()
            .filter(|&seq| seq_before_eq(seq, ack_nr))
            .collect();

        for seq in to_remove {
            if let Some(pkt) = self.pending_packets.remove(&seq) {
                let rtt = pkt.first_sent.elapsed().as_micros() as u32;
                // Only un-retransmitted packets give clean RTT samples
                // (Karn's algorithm).
                let rtt = if pkt.retransmits == 0 {
                    Some(rtt)
                } else {
                    None
                };
                self.congestion.on_ack(pkt.size, delay_us, rtt);
            }
        }

        // Process selective ACKs
        if let Some(sack) = sack {
            // SACK bitmap starts at ack_nr + 2
            for i in 0..sack.bitmask.len() * 8 {
                if sack.is_acked(i as u16) {
                    let seq = ack_nr.wrapping_add(2).wrapping_add(i as u16);
                    if let Some(pkt) = self.pending_packets.remove(&seq) {
                        self.congestion.on_ack(pkt.size, delay_us, None);
                    }
                }
            }
        }
    }

    /// Receive data into buffer, handling out-of-order
    fn receive_data(&mut self, seq_nr: u16, payload: Vec<u8>) -> io::Result<()> {
        let expected = self.ack_nr.wrapping_add(1);

        if seq_nr == expected {
            // In-order packet
            self.stats.bytes_received += payload.len() as u64;
            self.recv_buffer.extend(&payload);
            self.ack_nr = seq_nr;

            // Deliver any buffered out-of-order packets
            loop {
                let next = self.ack_nr.wrapping_add(1);
                if let Some(data) = self.ooo_packets.remove(&next) {
                    self.stats.bytes_received += data.len() as u64;
                    self.recv_buffer.extend(&data);
                    self.ack_nr = next;
                } else {
                    break;
                }
            }
        } else if seq_after(seq_nr, expected) && self.ooo_packets.len() < MAX_OOO_PACKETS {
            // Out-of-order packet - buffer it
            self.ooo_packets.insert(seq_nr, payload);
        }
        // Else: duplicate or too old, ignore (the ACK we send re-informs)

        Ok(())
    }

    /// Send data (returns amount actually queued)
    pub async fn send_data(&mut self, data: &[u8]) -> io::Result<usize> {
        if !self.state.can_send_data() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Cannot send in current state",
            ));
        }

        let mut sent = 0;

        // Respect both the congestion window and the peer's receive window
        // (an unknown/zero remote window still admits one MSS so the
        // window-update feedback loop can start).
        while sent < data.len()
            && self.congestion.can_send()
            && self.congestion.bytes_in_flight() < self.remote_window.max(MSS)
        {
            let end = (sent + MAX_PAYLOAD_SIZE).min(data.len());
            let chunk = data[sent..end].to_vec();

            let pkt = self.build_packet(PacketType::Data, chunk);
            self.send_packet(pkt).await?;
            sent = end;
        }

        Ok(sent)
    }

    /// Read data from receive buffer
    pub fn read_data(&mut self, buf: &mut [u8]) -> usize {
        let len = buf.len().min(self.recv_buffer.len());
        for (i, byte) in self.recv_buffer.drain(..len).enumerate() {
            buf[i] = byte;
        }
        len
    }

    /// Send a FIN to close the connection
    pub async fn close(&mut self) -> io::Result<()> {
        if self.state == ConnectionState::Connected {
            self.fin_sent = true;
            let pkt = self.build_packet(PacketType::Fin, Vec::new());
            self.send_packet(pkt).await?;
            self.state = ConnectionState::FinSent;
        } else if self.state == ConnectionState::Closing && !self.fin_sent {
            self.fin_sent = true;
            let pkt = self.build_packet(PacketType::Fin, Vec::new());
            self.send_packet(pkt).await?;
        }
        Ok(())
    }

    /// Build a packet with current state
    fn build_packet(&mut self, pkt_type: PacketType, payload: Vec<u8>) -> Packet {
        // BEP 29: the SYN carries the receive ID; everything else the send ID
        let conn_id = if pkt_type == PacketType::Syn {
            self.recv_conn_id
        } else {
            self.send_conn_id
        };

        let seq_nr = self.seq_nr;
        if pkt_type == PacketType::Data
            || pkt_type == PacketType::Syn
            || pkt_type == PacketType::Fin
        {
            self.seq_nr = self.seq_nr.wrapping_add(1);
        }

        let mut pkt = Packet::new(pkt_type, conn_id, seq_nr, self.ack_nr)
            .with_timestamps(timestamp_us(), self.reply_micro)
            .with_window(self.available_recv_window());

        // Add selective ACK if we have out-of-order packets
        if self.config.enable_sack && !self.ooo_packets.is_empty() && pkt_type == PacketType::State
        {
            let mut sack = SelectiveAck::default();
            for &seq in self.ooo_packets.keys() {
                let offset = seq.wrapping_sub(self.ack_nr).wrapping_sub(2);
                if offset < 256 {
                    sack.set_acked(offset);
                }
            }
            pkt = pkt.with_selective_ack(sack);
        }

        pkt.payload = payload;
        pkt
    }

    /// Send a packet and track it for retransmission
    async fn send_packet(&mut self, pkt: Packet) -> io::Result<()> {
        let data = pkt.encode();
        let payload = pkt.payload.clone();
        let seq_nr = pkt.seq_nr;
        let packet_type = pkt.packet_type;
        let track = pkt.is_data() || pkt.is_syn() || pkt.is_fin();

        self.send_packet_direct(pkt).await?;

        // Track for retransmission if it consumes a sequence number
        if track {
            let mut pending = PendingPacket::new(seq_nr, data, payload);
            pending.packet_type = packet_type;
            self.congestion.on_send(pending.size);
            self.pending_packets.insert(seq_nr, pending);
        }

        Ok(())
    }

    /// Send a packet without tracking
    async fn send_packet_direct(&mut self, pkt: Packet) -> io::Result<()> {
        let data = pkt.encode();

        self.packet_tx
            .send((data, self.remote_addr))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::ConnectionReset, "Send channel closed"))?;

        self.stats.packets_sent += 1;
        if !pkt.payload.is_empty() {
            self.stats.bytes_sent += pkt.payload.len() as u64;
        }
        self.last_send_time = Instant::now();
        if pkt.is_state() || pkt.is_data() || pkt.is_fin() {
            self.last_ack_sent = pkt.ack_nr;
        }

        Ok(())
    }

    /// Check and perform retransmissions
    pub async fn check_retransmits(&mut self) -> io::Result<()> {
        let rto = self.congestion.rto();
        let now = Instant::now();

        let to_retransmit: Vec<u16> = self
            .pending_packets
            .iter()
            .filter(|(_, p)| now.duration_since(p.last_sent) > rto)
            .map(|(seq, _)| *seq)
            .collect();

        let had_timeouts = !to_retransmit.is_empty();

        for seq in to_retransmit {
            let (packet_type, pkt_seq_nr, payload, max_exceeded) = {
                let pending = match self.pending_packets.get(&seq) {
                    Some(p) => p,
                    None => continue,
                };
                (
                    pending.packet_type,
                    pending.seq_nr,
                    pending.payload.clone(),
                    pending.retransmits >= MAX_RETRANSMITS,
                )
            };

            if max_exceeded {
                self.state = ConnectionState::TimedOut;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Max retransmits exceeded",
                ));
            }

            // Rebuild the packet as its ORIGINAL type (a retransmitted FIN
            // must stay a FIN, a SYN a SYN) with fresh timestamps/ack.
            let conn_id = if packet_type == PacketType::Syn {
                self.recv_conn_id
            } else {
                self.send_conn_id
            };
            let recv_window = self.available_recv_window();
            let mut pkt = Packet::new(packet_type, conn_id, pkt_seq_nr, self.ack_nr)
                .with_timestamps(timestamp_us(), self.reply_micro)
                .with_window(recv_window);
            pkt.payload = payload;

            self.send_packet_direct(pkt).await?;

            if let Some(pending) = self.pending_packets.get_mut(&seq) {
                pending.mark_retransmit();
            }
            self.stats.retransmits += 1;
        }

        if had_timeouts {
            // One backoff per timer pass, not per packet
            self.congestion.on_timeout();
        }

        Ok(())
    }

    /// Send an ACK if new data has arrived since the last one
    pub async fn maybe_send_ack(&mut self) -> io::Result<()> {
        if self.ack_nr != self.last_ack_sent && self.state.can_receive_data() {
            let pkt = self.build_packet(PacketType::State, Vec::new());
            self.send_packet_direct(pkt).await?;
        }
        Ok(())
    }

    /// Calculate available receive window
    fn available_recv_window(&self) -> u32 {
        let used = self.recv_buffer.len() as u32;
        self.recv_window.saturating_sub(used)
    }

    /// Get connection state
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Get statistics
    pub fn stats(&self) -> &ConnectionStats {
        &self.stats
    }

    /// Check if there's data available to read
    pub fn has_data(&self) -> bool {
        !self.recv_buffer.is_empty()
    }

    /// Get amount of data available to read
    pub fn available_data(&self) -> usize {
        self.recv_buffer.len()
    }
}

/// Check if seq_a comes before or equals seq_b (handles wrapping)
fn seq_before_eq(seq_a: u16, seq_b: u16) -> bool {
    let diff = seq_b.wrapping_sub(seq_a);
    diff == 0 || diff < 32768
}

/// Check if seq_a comes after seq_b (handles wrapping)
fn seq_after(seq_a: u16, seq_b: u16) -> bool {
    let diff = seq_a.wrapping_sub(seq_b);
    diff > 0 && diff < 32768
}

/// Callback the socket driver runs when the connection ends, used by the
/// multiplexer to deregister the connection.
pub type CleanupFn = Box<dyn FnOnce() + Send>;

/// High-level uTP socket
pub struct UtpSocket {
    inner: Arc<Mutex<UtpSocketInner>>,
    /// Signalled by the driver whenever state, buffers, or windows change
    wakeup: Arc<Notify>,
    driver: tokio::task::JoinHandle<()>,
    remote_addr: SocketAddr,
}

impl UtpSocket {
    /// Create a new outgoing socket. `conn_id` is our receive ID (the ID
    /// the multiplexer must route to us).
    pub fn new_outgoing(
        remote_addr: SocketAddr,
        conn_id: u16,
        packet_tx: PacketSender,
        packet_rx: PacketReceiver,
        config: UtpConfig,
        cleanup: Option<CleanupFn>,
    ) -> Self {
        let inner = Arc::new(Mutex::new(UtpSocketInner::new_outgoing(
            remote_addr,
            conn_id,
            packet_tx,
            config,
        )));
        Self::with_driver(inner, packet_rx, remote_addr, cleanup)
    }

    /// Create a new incoming socket from a received SYN.
    pub fn new_incoming(
        remote_addr: SocketAddr,
        syn_conn_id: u16,
        peer_seq_nr: u16,
        packet_tx: PacketSender,
        packet_rx: PacketReceiver,
        config: UtpConfig,
        cleanup: Option<CleanupFn>,
    ) -> Self {
        let inner = Arc::new(Mutex::new(UtpSocketInner::new_incoming(
            remote_addr,
            syn_conn_id,
            peer_seq_nr,
            packet_tx,
            config,
        )));
        Self::with_driver(inner, packet_rx, remote_addr, cleanup)
    }

    fn with_driver(
        inner: Arc<Mutex<UtpSocketInner>>,
        mut packet_rx: PacketReceiver,
        remote_addr: SocketAddr,
        cleanup: Option<CleanupFn>,
    ) -> Self {
        let wakeup = Arc::new(Notify::new());

        let driver_inner = Arc::clone(&inner);
        let driver_wakeup = Arc::clone(&wakeup);
        let driver = tokio::spawn(async move {
            let mut tick = tokio::time::interval(DRIVER_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                tokio::select! {
                    pkt = packet_rx.recv() => {
                        let mut guard = driver_inner.lock().await;
                        match pkt {
                            Some(pkt) => {
                                let _ = guard.process_packet(pkt).await;
                                let _ = guard.maybe_send_ack().await;
                            }
                            None => {
                                // Multiplexer gone
                                if !guard.state.is_closed() {
                                    guard.state = ConnectionState::Reset;
                                }
                            }
                        }
                        let done = guard.state.is_closed();
                        drop(guard);
                        driver_wakeup.notify_waiters();
                        if done {
                            break;
                        }
                    }
                    _ = tick.tick() => {
                        let mut guard = driver_inner.lock().await;
                        let retransmit_result = guard.check_retransmits().await;
                        let _ = guard.maybe_send_ack().await;
                        let done = guard.state.is_closed() || retransmit_result.is_err();
                        drop(guard);
                        if done {
                            driver_wakeup.notify_waiters();
                            break;
                        }
                        // Wake writers periodically too: the congestion
                        // window refills as ACKs are processed above.
                        driver_wakeup.notify_waiters();
                    }
                }
            }
            driver_wakeup.notify_waiters();
            if let Some(cleanup) = cleanup {
                cleanup();
            }
        });

        Self {
            inner,
            wakeup,
            driver,
            remote_addr,
        }
    }

    /// Connect to remote peer (send SYN, wait for the handshake)
    pub async fn connect(&self) -> io::Result<()> {
        {
            let mut inner = self.inner.lock().await;
            inner.connect().await?;
        }

        let result = timeout(Duration::from_secs(15), async {
            loop {
                {
                    let inner = self.inner.lock().await;
                    if inner.state == ConnectionState::Connected {
                        return Ok(());
                    }
                    if inner.state.is_closed() {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionRefused,
                            format!("Connection failed: {}", inner.state),
                        ));
                    }
                }
                self.wakeup.notified().await;
            }
        })
        .await;

        match result {
            Ok(r) => r,
            Err(_) => {
                let mut inner = self.inner.lock().await;
                inner.state = ConnectionState::TimedOut;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Connection timeout",
                ))
            }
        }
    }

    /// Accept an incoming connection (send SYN-ACK)
    pub async fn accept(&self) -> io::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.accept().await
    }

    /// Read data from the socket
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            // Register for wakeups BEFORE checking state so a packet that
            // arrives in between is not missed.
            let notified = self.wakeup.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let mut inner = self.inner.lock().await;
                if inner.has_data() {
                    return Ok(inner.read_data(buf));
                }
                if inner.fin_received {
                    return Ok(0); // EOF
                }
                if inner.state.is_closed() {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        format!("Connection closed: {}", inner.state),
                    ));
                }
            }

            notified.await;
        }
    }

    /// Read exactly len bytes
    pub async fn read_exact(&self, buf: &mut [u8]) -> io::Result<()> {
        let mut total = 0;
        while total < buf.len() {
            let n = self.read(&mut buf[total..]).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Unexpected EOF",
                ));
            }
            total += n;
        }
        Ok(())
    }

    /// Write all data to the socket, waiting on the congestion window as
    /// needed. The driver task keeps processing ACKs meanwhile, so this
    /// cannot livelock against an idle reader.
    pub async fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let notified = self.wakeup.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let sent = {
                let mut inner = self.inner.lock().await;
                if inner.state.is_closed() {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        format!("Connection closed: {}", inner.state),
                    ));
                }
                inner.send_data(&data[offset..]).await?
            };
            offset += sent;

            if sent == 0 {
                // Window full: wait for the driver to free it up.
                if timeout(WRITE_STALL_TIMEOUT, notified).await.is_err() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "uTP write stalled (window never opened)",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Flush (no-op: packets are sent as they are queued)
    pub async fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    /// Shutdown the socket (send FIN)
    pub async fn shutdown(&self) -> io::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.close().await
    }

    /// Get peer address
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.remote_addr)
    }

    /// Get connection state
    pub async fn state(&self) -> ConnectionState {
        self.inner.lock().await.state
    }

    /// Get inner for direct access (used in tests)
    pub fn inner(&self) -> Arc<Mutex<UtpSocketInner>> {
        self.inner.clone()
    }
}

impl Drop for UtpSocket {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_seq_comparison() {
        // Normal case
        assert!(seq_before_eq(10, 20));
        assert!(seq_before_eq(10, 10));
        assert!(!seq_before_eq(20, 10));

        // Wrap around
        assert!(seq_before_eq(65530, 5));
        assert!(!seq_before_eq(5, 65530));

        assert!(seq_after(20, 10));
        assert!(!seq_after(10, 10));
        assert!(seq_after(5, 65530));
    }

    #[test]
    fn test_connection_id_orientation() {
        let (tx, _rx) = mpsc::channel(4);
        let outgoing = UtpSocketInner::new_outgoing(
            "127.0.0.1:1".parse().unwrap(),
            100,
            tx.clone(),
            UtpConfig::default(),
        );
        // BEP 29: initiator receives on C, sends with C + 1
        assert_eq!(outgoing.recv_conn_id, 100);
        assert_eq!(outgoing.send_conn_id, 101);

        let incoming = UtpSocketInner::new_incoming(
            "127.0.0.1:1".parse().unwrap(),
            100, // the SYN's connection id
            1,
            tx,
            UtpConfig::default(),
        );
        // Acceptor: sends with C, receives on C + 1
        assert_eq!(incoming.send_conn_id, 100);
        assert_eq!(incoming.recv_conn_id, 101);
    }

    #[tokio::test]
    async fn test_duplicate_data_replaces_lost_ack_and_reports_gaps() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut sock = UtpSocketInner::new_incoming(
            "127.0.0.1:1".parse().unwrap(),
            100,
            1,
            tx,
            UtpConfig::default(),
        );
        sock.state = ConnectionState::Connected;
        let data = Packet::data(101, 2, 0, b"hello".to_vec());
        sock.process_packet(data.clone()).await.unwrap();
        sock.maybe_send_ack().await.unwrap();
        let first_ack = Packet::decode(&rx.try_recv().unwrap().0).unwrap();
        assert_eq!(first_ack.ack_nr, 2);

        // Discard that ACK as if it were lost, then retransmit the same data.
        sock.process_packet(data).await.unwrap();
        sock.maybe_send_ack().await.unwrap();
        let replacement = rx.try_recv().expect("lost ACK must be sent again");
        let replacement = Packet::decode(&replacement.0).unwrap();
        assert!(replacement.is_state());
        assert_eq!(replacement.ack_nr, 2);
        assert_eq!(sock.available_data(), 5, "duplicate data was delivered");

        // Out-of-order data also needs feedback even without a new cumulative ACK.
        sock.process_packet(Packet::data(101, 4, 0, b"later".to_vec()))
            .await
            .unwrap();
        sock.maybe_send_ack().await.unwrap();
        let gap_ack = rx
            .try_recv()
            .expect("out-of-order data must be acknowledged");
        let gap_ack = Packet::decode(&gap_ack.0).unwrap();
        assert_eq!(gap_ack.ack_nr, 2);
        assert!(gap_ack.selective_ack.unwrap().is_acked(0));
        assert_eq!(sock.available_data(), 5, "gap was exposed to the reader");
    }

    #[tokio::test]
    async fn test_syn_carries_recv_id_and_data_carries_send_id() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut sock = UtpSocketInner::new_outgoing(
            "127.0.0.1:1".parse().unwrap(),
            500,
            tx,
            UtpConfig::default(),
        );

        sock.connect().await.unwrap();
        let (syn_bytes, _) = rx.recv().await.unwrap();
        let syn = Packet::decode(&syn_bytes).unwrap();
        assert!(syn.is_syn());
        assert_eq!(syn.connection_id, 500, "SYN must carry the receive ID");

        // Fake the SYN-ACK: peer's seq starts at 700, acks our SYN (seq 1)
        sock.process_packet(Packet::state(501, 700, 1).with_window(64 * 1024))
            .await
            .unwrap();
        assert_eq!(sock.state, ConnectionState::Connected);
        assert_eq!(
            sock.ack_nr, 699,
            "ack_nr must be syn-ack seq minus one, or the first data packet is dropped"
        );

        sock.send_data(b"hello").await.unwrap();
        let (data_bytes, _) = rx.recv().await.unwrap();
        let data = Packet::decode(&data_bytes).unwrap();
        assert!(data.is_data());
        assert_eq!(data.connection_id, 501, "data must carry the send ID");

        // Peer's first data packet (seq 700) must be accepted in order
        sock.process_packet(Packet::data(501, 700, 2, b"world".to_vec()))
            .await
            .unwrap();
        assert_eq!(
            sock.available_data(),
            5,
            "first peer data packet was dropped"
        );
    }
}
