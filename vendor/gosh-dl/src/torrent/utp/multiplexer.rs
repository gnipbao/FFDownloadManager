//! uTP Socket Multiplexer
//!
//! This module manages a single UDP socket shared by multiple uTP
//! connections. It demultiplexes incoming packets to the correct connection
//! based on (remote_addr, connection_id).
//!
//! Registration uses each socket's **receive** connection ID (see the
//! connection-ID rules in [`super::socket`]): every non-SYN packet a peer
//! sends carries the peer's send ID, which equals our receive ID, so routing
//! is an exact-key lookup.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};

use parking_lot::RwLock;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::packet::{Packet, HEADER_SIZE};
use super::socket::{PacketSender, UtpConfig, UtpSocket};

/// Bound on unaccepted incoming connections (SYN backlog)
const MAX_PENDING_INCOMING: usize = 32;

/// Per-connection inbound packet channel capacity
const CONN_CHANNEL_CAPACITY: usize = 256;

/// Key for identifying a connection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ConnectionKey {
    remote_addr: SocketAddr,
    conn_id: u16,
}

/// Pending incoming connection
struct PendingConnection {
    remote_addr: SocketAddr,
    packet_tx: mpsc::Sender<Packet>,
    packet_rx: Option<mpsc::Receiver<Packet>>,
    syn_packet: Packet,
}

type ConnectionMap = Arc<RwLock<HashMap<ConnectionKey, mpsc::Sender<Packet>>>>;

/// uTP Socket Multiplexer
///
/// Manages a shared UDP socket and routes packets to individual uTP
/// connections.
pub struct UtpMux {
    /// Bound UDP socket
    socket: Arc<UdpSocket>,

    /// Local address
    local_addr: SocketAddr,

    /// Active connections: (addr, our recv id) -> packet sender
    connections: ConnectionMap,

    /// Pending incoming connections waiting to be accepted,
    /// keyed by (addr, SYN connection id) for retransmit dedupe
    pending_incoming: Arc<RwLock<HashMap<ConnectionKey, PendingConnection>>>,

    /// Channel for sending packets from connections to the UDP socket
    send_tx: PacketSender,

    /// Next connection ID to use
    next_conn_id: Arc<RwLock<u16>>,

    /// Configuration
    config: UtpConfig,

    /// Background tasks
    recv_task: Option<JoinHandle<()>>,
    send_task: Option<JoinHandle<()>>,
}

impl UtpMux {
    /// Create a new multiplexer bound to the given address
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        Self::bind_with_config(addr, UtpConfig::default()).await
    }

    /// Create with custom config
    pub async fn bind_with_config(addr: SocketAddr, config: UtpConfig) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let local_addr = socket.local_addr()?;
        let socket = Arc::new(socket);

        let (send_tx, send_rx) = mpsc::channel(1024);

        let mut mux = Self {
            socket,
            local_addr,
            connections: Arc::new(RwLock::new(HashMap::new())),
            pending_incoming: Arc::new(RwLock::new(HashMap::new())),
            send_tx,
            next_conn_id: Arc::new(RwLock::new(rand::random())),
            config,
            recv_task: None,
            send_task: None,
        };

        mux.start_tasks(send_rx);

        Ok(mux)
    }

    /// Get local address
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Start background receive and send tasks
    fn start_tasks(&mut self, mut send_rx: mpsc::Receiver<(Vec<u8>, SocketAddr)>) {
        // Receive task
        let socket = self.socket.clone();
        let connections = self.connections.clone();
        let pending = self.pending_incoming.clone();
        let send_tx = self.send_tx.clone();

        let recv_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];

            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((len, remote_addr)) => {
                        if len < HEADER_SIZE {
                            continue; // Too short
                        }

                        match Packet::decode(&buf[..len]) {
                            Ok(pkt) => {
                                Self::route_packet(
                                    pkt,
                                    remote_addr,
                                    &connections,
                                    &pending,
                                    &send_tx,
                                );
                            }
                            Err(e) => {
                                tracing::debug!("Failed to decode uTP packet: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("UDP receive error: {}", e);
                        break;
                    }
                }
            }
        });

        // Send task
        let socket = self.socket.clone();
        let send_task = tokio::spawn(async move {
            while let Some((data, addr)) = send_rx.recv().await {
                if let Err(e) = socket.send_to(&data, addr).await {
                    tracing::warn!("Failed to send UDP packet to {}: {}", addr, e);
                }
            }
        });

        self.recv_task = Some(recv_task);
        self.send_task = Some(send_task);
    }

    /// Route an incoming packet to the correct connection.
    ///
    /// Never blocks: per-connection channels use `try_send`, and a full
    /// channel drops the packet — UDP semantics; retransmission recovers.
    fn route_packet(
        pkt: Packet,
        remote_addr: SocketAddr,
        connections: &ConnectionMap,
        pending: &RwLock<HashMap<ConnectionKey, PendingConnection>>,
        send_tx: &PacketSender,
    ) {
        if pkt.is_syn() {
            // The SYN carries the initiator's receive ID C; an accepted
            // connection is registered under our receive ID C + 1.
            let conn_key = ConnectionKey {
                remote_addr,
                conn_id: pkt.connection_id.wrapping_add(1),
            };
            if let Some(tx) = connections.read().get(&conn_key).cloned() {
                // Retransmitted SYN for an accepted connection: forward so
                // the socket can re-ack.
                let _ = tx.try_send(pkt);
                return;
            }

            let pending_key = ConnectionKey {
                remote_addr,
                conn_id: pkt.connection_id,
            };
            let mut pending = pending.write();
            if pending.contains_key(&pending_key) {
                return; // SYN retransmit while still in the backlog
            }
            if pending.len() >= MAX_PENDING_INCOMING {
                tracing::debug!("uTP SYN backlog full, dropping SYN from {}", remote_addr);
                return;
            }
            let (packet_tx, packet_rx) = mpsc::channel(CONN_CHANNEL_CAPACITY);
            pending.insert(
                pending_key,
                PendingConnection {
                    remote_addr,
                    packet_tx,
                    packet_rx: Some(packet_rx),
                    syn_packet: pkt,
                },
            );
            return;
        }

        // Non-SYN packets carry the sender's send ID == our receive ID.
        // A RESET echoes whatever ID the resetter last saw, which may be our
        // send ID instead — try the neighbors for those.
        let candidate_ids: &[u16] = if pkt.is_reset() {
            &[
                pkt.connection_id,
                pkt.connection_id.wrapping_add(1),
                pkt.connection_id.wrapping_sub(1),
            ]
        } else {
            &[pkt.connection_id]
        };
        for conn_id in candidate_ids {
            let key = ConnectionKey {
                remote_addr,
                conn_id: *conn_id,
            };
            if let Some(tx) = connections.read().get(&key).cloned() {
                if tx.try_send(pkt).is_err() {
                    tracing::trace!(
                        "uTP channel full/closed; dropping packet from {}",
                        remote_addr
                    );
                }
                return;
            }
        }

        if !pkt.is_reset() {
            // Unknown connection: tell the peer to go away
            let reset = Packet::reset(pkt.connection_id, 0, pkt.seq_nr);
            let _ = send_tx.try_send((reset.encode(), remote_addr));
        }
    }

    /// Build the cleanup hook that deregisters a connection when its driver
    /// task ends.
    fn cleanup_for(&self, key: ConnectionKey) -> Box<dyn FnOnce() + Send> {
        let connections: Weak<RwLock<HashMap<ConnectionKey, mpsc::Sender<Packet>>>> =
            Arc::downgrade(&self.connections);
        Box::new(move || {
            if let Some(connections) = connections.upgrade() {
                connections.write().remove(&key);
            }
        })
    }

    /// Connect to a remote peer
    pub async fn connect(&self, addr: SocketAddr) -> io::Result<UtpSocket> {
        // Our receive ID; the SYN carries it and we send with +1.
        let conn_id = {
            let mut id = self.next_conn_id.write();
            let current = *id;
            *id = id.wrapping_add(2);
            current
        };

        let (packet_tx, packet_rx) = mpsc::channel(CONN_CHANNEL_CAPACITY);
        let key = ConnectionKey {
            remote_addr: addr,
            conn_id,
        };
        self.connections.write().insert(key, packet_tx);

        let socket = UtpSocket::new_outgoing(
            addr,
            conn_id,
            self.send_tx.clone(),
            packet_rx,
            self.config.clone(),
            Some(self.cleanup_for(key)),
        );

        match socket.connect().await {
            Ok(()) => Ok(socket),
            Err(e) => {
                self.connections.write().remove(&key);
                Err(e)
            }
        }
    }

    /// Accept an incoming connection
    pub async fn accept(&self) -> io::Result<UtpSocket> {
        loop {
            let pending_conn = {
                let mut pending = self.pending_incoming.write();
                let key = pending.keys().next().copied();
                key.and_then(|k| pending.remove(&k))
            };

            if let Some(mut conn) = pending_conn {
                let syn = &conn.syn_packet;
                let syn_conn_id = syn.connection_id;
                let peer_seq_nr = syn.seq_nr;
                let remote_addr = conn.remote_addr;

                let packet_rx = conn.packet_rx.take().unwrap();

                // We receive on SYN id + 1 (the initiator's send id)
                let key = ConnectionKey {
                    remote_addr,
                    conn_id: syn_conn_id.wrapping_add(1),
                };
                self.connections.write().insert(key, conn.packet_tx);

                let socket = UtpSocket::new_incoming(
                    remote_addr,
                    syn_conn_id,
                    peer_seq_nr,
                    self.send_tx.clone(),
                    packet_rx,
                    self.config.clone(),
                    Some(self.cleanup_for(key)),
                );

                socket.accept().await?;

                return Ok(socket);
            }

            // Wait a bit before checking again
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Close the multiplexer
    pub async fn close(&mut self) {
        if let Some(task) = self.recv_task.take() {
            task.abort();
        }
        if let Some(task) = self.send_task.take() {
            task.abort();
        }

        self.connections.write().clear();
        self.pending_incoming.write().clear();
    }
}

impl Drop for UtpMux {
    fn drop(&mut self) {
        if let Some(task) = self.recv_task.take() {
            task.abort();
        }
        if let Some(task) = self.send_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bind() {
        let mux = UtpMux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let local_addr = mux.local_addr();
        assert!(local_addr.port() > 0);
    }

    #[tokio::test]
    async fn test_syn_creates_single_pending_connection() {
        let mux = UtpMux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let mux_addr = mux.local_addr();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender_addr = sender.local_addr().unwrap();

        // SYN plus a retransmit of the same SYN
        let syn = Packet::syn(100, 1);
        sender.send_to(&syn.encode(), mux_addr).await.unwrap();
        sender.send_to(&syn.encode(), mux_addr).await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        {
            let pending = mux.pending_incoming.read();
            assert_eq!(pending.len(), 1, "SYN retransmit must not duplicate");
            let conn = pending.values().next().unwrap();
            assert_eq!(conn.remote_addr, sender_addr);
        }
    }

    /// Full loopback: two muxes, connect, exchange data both ways.
    #[tokio::test]
    async fn test_loopback_bidirectional_transfer() {
        let mux_a = Arc::new(UtpMux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let mux_b = Arc::new(UtpMux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let addr_b = mux_b.local_addr();

        let payload_ab: Vec<u8> = (0..512 * 1024u32).map(|i| (i % 251) as u8).collect();
        let payload_ba: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 241) as u8).collect();

        let expected_ab = payload_ab.clone();
        let expected_ba = payload_ba.clone();

        let server = {
            let mux_b = Arc::clone(&mux_b);
            tokio::spawn(async move {
                let sock = tokio::time::timeout(std::time::Duration::from_secs(10), mux_b.accept())
                    .await
                    .expect("accept timed out")
                    .expect("accept failed");

                let mut received = vec![0u8; expected_ab.len()];
                sock.read_exact(&mut received).await.expect("server read");
                assert_eq!(received, expected_ab, "A->B data corrupted");

                sock.write_all(&payload_ba).await.expect("server write");
                // Keep the socket alive until the client is done reading
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            })
        };

        let sock = tokio::time::timeout(std::time::Duration::from_secs(10), mux_a.connect(addr_b))
            .await
            .expect("connect timed out")
            .expect("connect failed");

        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sock.write_all(&payload_ab),
        )
        .await
        .expect("client write stalled")
        .expect("client write failed");

        let mut received = vec![0u8; expected_ba.len()];
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            sock.read_exact(&mut received),
        )
        .await
        .expect("client read stalled")
        .expect("client read failed");
        assert_eq!(received, expected_ba, "B->A data corrupted");

        sock.shutdown().await.ok();
        server.await.unwrap();
    }

    /// Transfer through a lossy UDP proxy: retransmissions must recover.
    #[tokio::test]
    async fn test_transfer_survives_packet_loss() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let mux_a = Arc::new(UtpMux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let mux_b = Arc::new(UtpMux::bind("127.0.0.1:0".parse().unwrap()).await.unwrap());
        let addr_a = mux_a.local_addr();
        let addr_b = mux_b.local_addr();

        // Lossy proxy: forwards between A and B, dropping every 9th packet.
        let proxy = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let dropped = Arc::new(AtomicU64::new(0));
        let proxy_task = {
            let dropped = Arc::clone(&dropped);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                let mut counter = 0u64;
                loop {
                    let Ok((len, from)) = proxy.recv_from(&mut buf).await else {
                        break;
                    };
                    counter += 1;
                    if counter % 9 == 0 {
                        dropped.fetch_add(1, Ordering::Relaxed);
                        continue; // drop
                    }
                    // A's packets go to B, everything else back to A
                    let target = if from == addr_a { addr_b } else { addr_a };
                    let _ = proxy.send_to(&buf[..len], target).await;
                }
            })
        };

        let payload: Vec<u8> = (0..128 * 1024u32).map(|i| (i % 239) as u8).collect();
        let expected = payload.clone();

        let server = {
            let mux_b = Arc::clone(&mux_b);
            tokio::spawn(async move {
                let sock = tokio::time::timeout(std::time::Duration::from_secs(15), mux_b.accept())
                    .await
                    .expect("accept timed out")
                    .expect("accept failed");
                let mut received = vec![0u8; expected.len()];
                sock.read_exact(&mut received).await.expect("read");
                received
            })
        };

        // Connect to B *via the proxy*
        let sock = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            mux_a.connect(proxy_addr),
        )
        .await
        .expect("connect timed out")
        .expect("connect failed (SYN loss should be retransmitted)");

        tokio::time::timeout(std::time::Duration::from_secs(60), sock.write_all(&payload))
            .await
            .expect("write stalled under loss")
            .expect("write failed");

        let received = tokio::time::timeout(std::time::Duration::from_secs(60), server)
            .await
            .expect("server stalled under loss")
            .unwrap();
        assert_eq!(received, payload, "data corrupted under packet loss");
        assert!(
            dropped.load(Ordering::Relaxed) > 0,
            "proxy dropped nothing — test proved nothing"
        );

        proxy_task.abort();
    }
}
