//! Local Peer Discovery (LPD) implementation (BEP 14).
//!
//! LPD uses UDP multicast to discover peers on the local network without
//! requiring internet access or trackers. This is useful for:
//! - Local network transfers (e.g., office/home LANs)
//! - Faster discovery of nearby peers
//! - Privacy (no external tracker communication)
//!
//! Message format (HTTP-like):
//! ```text
//! BT-SEARCH * HTTP/1.1\r\n
//! Host: 239.192.152.143:6771\r\n
//! Port: <listen_port>\r\n
//! Infohash: <40-char hex>\r\n
//! cookie: <optional>\r\n
//! \r\n
//! ```

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;

use crate::error::{EngineError, ProtocolErrorKind, Result};
use crate::torrent::metainfo::Sha1Hash;

/// LPD multicast address (IPv4).
pub const LPD_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 192, 152, 143);

/// LPD multicast port.
pub const LPD_PORT: u16 = 6771;

/// LPD multicast group as SocketAddr.
pub const LPD_MULTICAST_GROUP: SocketAddrV4 = SocketAddrV4::new(LPD_MULTICAST_ADDR, LPD_PORT);

/// Default announce interval (5 minutes).
pub const DEFAULT_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(300);

/// A discovered local peer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalPeer {
    /// Peer's address (IP from source, port from message).
    pub addr: SocketAddr,
    /// The torrent info_hash this peer announced.
    pub info_hash: Sha1Hash,
}

/// Local Peer Discovery service.
pub struct LpdService {
    /// UDP socket for multicast.
    socket: Arc<UdpSocket>,
    /// Port we're listening on for BitTorrent connections.
    listen_port: u16,
    /// Shutdown signal sender.
    shutdown_tx: broadcast::Sender<()>,
    /// Whether the service is running.
    running: Arc<AtomicBool>,
    /// Info hashes we're announcing.
    tracked: Arc<tokio::sync::RwLock<HashSet<Sha1Hash>>>,
}

impl LpdService {
    /// Create and start a new LPD service.
    ///
    /// # Arguments
    /// * `listen_port` - Port we're listening on for BitTorrent connections.
    ///
    /// # Returns
    /// A new LPD service, or an error if socket setup fails.
    pub async fn new(listen_port: u16) -> Result<Self> {
        let socket = create_multicast_socket()?;

        let (shutdown_tx, _) = broadcast::channel(1);

        Ok(Self {
            socket: Arc::new(socket),
            listen_port,
            shutdown_tx,
            running: Arc::new(AtomicBool::new(true)),
            tracked: Arc::new(tokio::sync::RwLock::new(HashSet::new())),
        })
    }

    /// Check if the service is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Start tracking an info_hash for announcements.
    pub async fn track(&self, info_hash: Sha1Hash) {
        let mut tracked = self.tracked.write().await;
        tracked.insert(info_hash);
    }

    /// Stop tracking an info_hash.
    pub async fn untrack(&self, info_hash: &Sha1Hash) {
        let mut tracked = self.tracked.write().await;
        tracked.remove(info_hash);
    }

    /// Get all tracked info hashes.
    pub async fn tracked_hashes(&self) -> Vec<Sha1Hash> {
        let tracked = self.tracked.read().await;
        tracked.iter().cloned().collect()
    }

    /// Send an LPD announcement for a specific torrent.
    ///
    /// # Arguments
    /// * `info_hash` - The torrent info_hash to announce.
    pub async fn announce(&self, info_hash: &Sha1Hash) -> Result<()> {
        if !self.is_running() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::LpdError,
                "LPD service is not running",
            ));
        }

        let message = build_announce_message(info_hash, self.listen_port);

        self.socket
            .send_to(message.as_bytes(), LPD_MULTICAST_GROUP)
            .await
            .map_err(|e| {
                EngineError::protocol(
                    ProtocolErrorKind::LpdError,
                    format!("Failed to send LPD announce: {}", e),
                )
            })?;

        Ok(())
    }

    /// Announce all tracked torrents.
    pub async fn announce_all(&self) -> Vec<Result<()>> {
        let tracked = self.tracked.read().await;
        let mut results = Vec::new();

        for info_hash in tracked.iter() {
            results.push(self.announce(info_hash).await);
        }

        results
    }

    /// Start listening for LPD announcements.
    ///
    /// Returns a receiver for discovered peers. The listener runs in a
    /// background task until shutdown.
    pub fn listen(&self) -> broadcast::Receiver<LocalPeer> {
        let (tx, rx) = broadcast::channel(100);
        let socket = self.socket.clone();
        let running = self.running.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let our_port = self.listen_port;

        tokio::spawn(async move {
            let mut buf = [0u8; 1024];

            loop {
                if !running.load(Ordering::SeqCst) {
                    break;
                }

                tokio::select! {
                    result = socket.recv_from(&mut buf) => {
                        match result {
                            Ok((len, source_addr)) => {
                                if let Some(peer) = parse_announce(&buf[..len], source_addr, our_port) {
                                    let _ = tx.send(peer);
                                }
                            }
                            Err(e) => {
                                // Log error but continue
                                tracing::debug!("LPD recv error: {}", e);
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                    // Periodic check in case shutdown was missed
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
        });

        rx
    }

    /// Get the listen port.
    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }

    /// Shutdown the LPD service.
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        let _ = self.shutdown_tx.send(());
    }
}

impl Drop for LpdService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Create a UDP socket configured for multicast.
fn create_multicast_socket() -> Result<UdpSocket> {
    // Use socket2 for cross-platform multicast setup
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| EngineError::protocol(ProtocolErrorKind::LpdError, e.to_string()))?;

    // Allow address reuse (important for multiple instances)
    socket
        .set_reuse_address(true)
        .map_err(|e| EngineError::protocol(ProtocolErrorKind::LpdError, e.to_string()))?;

    // On Unix, also set SO_REUSEPORT if available
    #[cfg(unix)]
    {
        socket
            .set_reuse_port(true)
            .map_err(|e| EngineError::protocol(ProtocolErrorKind::LpdError, e.to_string()))?;
    }

    // Bind to the multicast port
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, LPD_PORT);
    socket
        .bind(&bind_addr.into())
        .map_err(|e| EngineError::protocol(ProtocolErrorKind::LpdError, e.to_string()))?;

    // Join the multicast group
    socket
        .join_multicast_v4(&LPD_MULTICAST_ADDR, &Ipv4Addr::UNSPECIFIED)
        .map_err(|e| {
            EngineError::protocol(
                ProtocolErrorKind::LpdError,
                format!("Failed to join multicast group: {}", e),
            )
        })?;

    // Set non-blocking for async
    socket
        .set_nonblocking(true)
        .map_err(|e| EngineError::protocol(ProtocolErrorKind::LpdError, e.to_string()))?;

    // Convert to tokio UdpSocket
    let std_socket: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_socket)
        .map_err(|e| EngineError::protocol(ProtocolErrorKind::LpdError, e.to_string()))
}

/// Build an LPD announce message.
fn build_announce_message(info_hash: &Sha1Hash, port: u16) -> String {
    let hex_hash: String = info_hash.iter().map(|b| format!("{:02x}", b)).collect();

    format!(
        "BT-SEARCH * HTTP/1.1\r\n\
         Host: {}:{}\r\n\
         Port: {}\r\n\
         Infohash: {}\r\n\
         \r\n",
        LPD_MULTICAST_ADDR, LPD_PORT, port, hex_hash
    )
}

/// Parse an LPD announcement message.
///
/// # Arguments
/// * `data` - Raw message bytes
/// * `source` - Source address of the message
/// * `our_port` - Our own port (to filter self-announcements)
///
/// # Returns
/// Parsed LocalPeer if valid, None otherwise.
fn parse_announce(data: &[u8], source: SocketAddr, our_port: u16) -> Option<LocalPeer> {
    let text = std::str::from_utf8(data).ok()?;

    // Must be a BT-SEARCH message
    if !text.starts_with("BT-SEARCH") {
        return None;
    }

    let mut port: Option<u16> = None;
    let mut info_hash_hex: Option<&str> = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Port:") {
            port = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("Infohash:") {
            info_hash_hex = Some(value.trim());
        }
    }

    let peer_port = port?;
    let hash_str = info_hash_hex?;

    // Validate and parse hex info_hash
    if hash_str.len() != 40 {
        return None;
    }

    let mut info_hash = [0u8; 20];
    for i in 0..20 {
        info_hash[i] = u8::from_str_radix(&hash_str[i * 2..i * 2 + 2], 16).ok()?;
    }

    // Build peer address using source IP + announced port
    let peer_addr = match source {
        SocketAddr::V4(addr) => SocketAddr::V4(SocketAddrV4::new(*addr.ip(), peer_port)),
        SocketAddr::V6(addr) => {
            SocketAddr::V6(std::net::SocketAddrV6::new(*addr.ip(), peer_port, 0, 0))
        }
    };

    // Skip if this is our own announcement
    if peer_port == our_port && is_local_address(source.ip()) {
        return None;
    }

    Some(LocalPeer {
        addr: peer_addr,
        info_hash,
    })
}

/// Check if an IP address is a local address.
fn is_local_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(addr) => addr.is_loopback() || addr.is_private() || addr.is_link_local(),
        IpAddr::V6(addr) => addr.is_loopback(),
    }
}

/// LPD manager that handles periodic announcements.
pub struct LpdManager {
    service: Arc<LpdService>,
    /// Announce interval.
    announce_interval: Duration,
}

impl LpdManager {
    /// Create a new LPD manager.
    pub fn new(service: Arc<LpdService>) -> Self {
        Self {
            service,
            announce_interval: DEFAULT_ANNOUNCE_INTERVAL,
        }
    }

    /// Set the announce interval.
    pub fn set_announce_interval(&mut self, interval: Duration) {
        self.announce_interval = interval;
    }

    /// Start the announcement loop in the background.
    ///
    /// Returns a handle to the spawned task.
    pub fn start_announce_loop(&self) -> tokio::task::JoinHandle<()> {
        let service = self.service.clone();
        let interval = self.announce_interval;

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);

            loop {
                ticker.tick().await;

                if !service.is_running() {
                    break;
                }

                // Announce all tracked torrents
                let results = service.announce_all().await;
                for result in results {
                    if let Err(e) = result {
                        tracing::debug!("LPD announce error: {}", e);
                    }
                }
            }
        })
    }

    /// Get the underlying service.
    pub fn service(&self) -> &Arc<LpdService> {
        &self.service
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_announce_message() {
        let info_hash: Sha1Hash = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
        ];

        let message = build_announce_message(&info_hash, 6881);

        assert!(message.starts_with("BT-SEARCH * HTTP/1.1\r\n"));
        assert!(message.contains("Host: 239.192.152.143:6771\r\n"));
        assert!(message.contains("Port: 6881\r\n"));
        assert!(message.contains("Infohash: 0123456789abcdef0123456789abcdef01234567\r\n"));
        assert!(message.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_parse_announce() {
        let message = "BT-SEARCH * HTTP/1.1\r\n\
                       Host: 239.192.152.143:6771\r\n\
                       Port: 6882\r\n\
                       Infohash: 0123456789abcdef0123456789abcdef01234567\r\n\
                       \r\n";

        let source = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 100), 12345));
        let peer = parse_announce(message.as_bytes(), source, 6881);

        assert!(peer.is_some());
        let peer = peer.unwrap();

        // Port should be from the message, not the source
        assert_eq!(peer.addr.port(), 6882);
        // IP should be from the source
        if let SocketAddr::V4(addr) = peer.addr {
            assert_eq!(*addr.ip(), Ipv4Addr::new(192, 168, 1, 100));
        }

        // Verify info_hash
        let expected_hash: Sha1Hash = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
        ];
        assert_eq!(peer.info_hash, expected_hash);
    }

    #[test]
    fn test_parse_announce_invalid() {
        // Not a BT-SEARCH message
        let message = "HTTP/1.1 200 OK\r\n\r\n";
        let source = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 100), 12345));
        assert!(parse_announce(message.as_bytes(), source, 6881).is_none());

        // Missing port
        let message = "BT-SEARCH * HTTP/1.1\r\n\
                       Infohash: 0123456789abcdef0123456789abcdef01234567\r\n\
                       \r\n";
        assert!(parse_announce(message.as_bytes(), source, 6881).is_none());

        // Invalid info_hash length
        let message = "BT-SEARCH * HTTP/1.1\r\n\
                       Port: 6882\r\n\
                       Infohash: 0123456789\r\n\
                       \r\n";
        assert!(parse_announce(message.as_bytes(), source, 6881).is_none());
    }

    #[test]
    fn test_parse_announce_self_filter() {
        let message = "BT-SEARCH * HTTP/1.1\r\n\
                       Port: 6881\r\n\
                       Infohash: 0123456789abcdef0123456789abcdef01234567\r\n\
                       \r\n";

        // Should filter out our own announcements (same port + local address)
        let source = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 12345));
        let peer = parse_announce(message.as_bytes(), source, 6881);
        assert!(peer.is_none());

        // Should accept from different port
        let peer = parse_announce(message.as_bytes(), source, 6882);
        assert!(peer.is_some());
    }

    #[test]
    fn test_constants() {
        assert_eq!(LPD_MULTICAST_ADDR, Ipv4Addr::new(239, 192, 152, 143));
        assert_eq!(LPD_PORT, 6771);
    }
}
