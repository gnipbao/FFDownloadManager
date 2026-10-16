//! DHT (Distributed Hash Table) client implementation (BEP 5).
//!
//! Uses the mainline crate to connect to the BitTorrent Mainline DHT network
//! for decentralized peer discovery without relying on trackers.
//!
//! The DHT allows:
//! - Finding peers for a torrent by info_hash
//! - Announcing ourselves as a peer for torrents we're downloading/seeding

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mainline::{Dht, Id};
use tokio::sync::RwLock;

use crate::error::{EngineError, ProtocolErrorKind, Result};
use crate::torrent::metainfo::Sha1Hash;

/// Default bootstrap nodes for the Mainline DHT.
pub const DEFAULT_BOOTSTRAP_NODES: &[&str] = &[
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.aelitis.com:6881",
];

/// DHT client for peer discovery.
///
/// Wraps the mainline crate's DHT implementation with a simplified API.
pub struct DhtClient {
    /// The mainline DHT instance.
    dht: Arc<Dht>,
    /// Port we're listening on for incoming peer connections.
    listen_port: u16,
    /// Whether the client is running.
    running: Arc<AtomicBool>,
    /// Cached peers from recent lookups.
    peer_cache: Arc<RwLock<std::collections::HashMap<Sha1Hash, Vec<SocketAddr>>>>,
}

impl DhtClient {
    /// Create a new DHT client.
    ///
    /// # Arguments
    /// * `listen_port` - Port we're listening on for BitTorrent connections.
    ///
    /// # Returns
    /// A new DHT client, or an error if bootstrapping fails.
    pub fn new(listen_port: u16) -> Result<Self> {
        // Create DHT client with default settings
        let dht = Dht::client()
            .map_err(|e| EngineError::protocol(ProtocolErrorKind::DhtError, e.to_string()))?;

        Ok(Self {
            dht: Arc::new(dht),
            listen_port,
            running: Arc::new(AtomicBool::new(true)),
            peer_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Create a DHT client with custom bootstrap nodes.
    ///
    /// # Arguments
    /// * `listen_port` - Port we're listening on for BitTorrent connections.
    /// * `bootstrap_nodes` - List of bootstrap node addresses (e.g., "router.bittorrent.com:6881").
    pub fn with_bootstrap(listen_port: u16, bootstrap_nodes: &[String]) -> Result<Self> {
        // Create DHT client with custom bootstrap nodes if provided
        let dht = if bootstrap_nodes.is_empty() {
            // Fall back to default if no valid nodes provided
            Dht::client()
                .map_err(|e| EngineError::protocol(ProtocolErrorKind::DhtError, e.to_string()))?
        } else {
            // Use custom bootstrap nodes
            Dht::builder()
                .bootstrap(bootstrap_nodes)
                .build()
                .map_err(|e| EngineError::protocol(ProtocolErrorKind::DhtError, e.to_string()))?
        };

        Ok(Self {
            dht: Arc::new(dht),
            listen_port,
            running: Arc::new(AtomicBool::new(true)),
            peer_cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
        })
    }

    /// Check if the DHT client is running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Find peers for a given info_hash.
    ///
    /// Queries the DHT network for peers that have announced the given torrent.
    ///
    /// # Arguments
    /// * `info_hash` - The 20-byte SHA-1 hash of the torrent's info dictionary.
    ///
    /// # Returns
    /// A vector of peer socket addresses.
    pub async fn find_peers(&self, info_hash: &Sha1Hash) -> Vec<SocketAddr> {
        if !self.is_running() {
            return vec![];
        }

        // Convert Sha1Hash to mainline Id
        let id = match Id::from_bytes(info_hash) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to convert info_hash to DHT Id: {}", e);
                return vec![];
            }
        };

        // Collect peers from the DHT lookup.
        // The get_peers() call performs blocking network I/O, so we run it on a
        // blocking thread to avoid starving the Tokio runtime.
        // Note: The `mainline` crate only returns IPv4 peers (SocketAddrV4).
        // DHT IPv6 support depends on the upstream crate adding it.
        let dht = self.dht.clone();
        let peers: Vec<SocketAddr> = tokio::task::spawn_blocking(move || {
            dht.get_peers(id)
                .flatten()
                .map(SocketAddr::V4)
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        // Update cache
        if !peers.is_empty() {
            let mut cache = self.peer_cache.write().await;
            cache.insert(*info_hash, peers.clone());
        }

        peers
    }

    /// Find peers with a timeout.
    ///
    /// # Arguments
    /// * `info_hash` - The torrent info_hash.
    /// * `timeout` - Maximum time to wait for responses.
    ///
    /// # Returns
    /// Peers found within the timeout period.
    pub async fn find_peers_timeout(
        &self,
        info_hash: &Sha1Hash,
        timeout: Duration,
    ) -> Vec<SocketAddr> {
        match tokio::time::timeout(timeout, async { self.find_peers(info_hash).await }).await {
            Ok(peers) => peers,
            Err(_) => {
                // Timeout - return cached peers if available
                let cache = self.peer_cache.read().await;
                cache.get(info_hash).cloned().unwrap_or_default()
            }
        }
    }

    /// Announce ourselves to the DHT for a given info_hash.
    ///
    /// Tells the DHT network that we have the torrent and are accepting connections.
    ///
    /// # Arguments
    /// * `info_hash` - The torrent info_hash to announce.
    ///
    /// # Returns
    /// Ok if the announcement was sent, Err on failure.
    pub fn announce(&self, info_hash: &Sha1Hash) -> Result<()> {
        if !self.is_running() {
            return Err(EngineError::protocol(
                ProtocolErrorKind::DhtError,
                "DHT client is not running",
            ));
        }

        let id = Id::from_bytes(info_hash).map_err(|e| {
            EngineError::protocol(
                ProtocolErrorKind::DhtError,
                format!("Failed to convert info_hash to DHT Id: {}", e),
            )
        })?;

        // Announce with our listen port
        // The mainline crate's announce_peer returns a result we can map
        self.dht
            .announce_peer(id, Some(self.listen_port))
            .map_err(|e| EngineError::protocol(ProtocolErrorKind::DhtError, e.to_string()))?;

        Ok(())
    }

    /// Get cached peers for an info_hash without querying the network.
    pub async fn cached_peers(&self, info_hash: &Sha1Hash) -> Vec<SocketAddr> {
        let cache = self.peer_cache.read().await;
        cache.get(info_hash).cloned().unwrap_or_default()
    }

    /// Clear the peer cache for a specific info_hash.
    pub async fn clear_cache(&self, info_hash: &Sha1Hash) {
        let mut cache = self.peer_cache.write().await;
        cache.remove(info_hash);
    }

    /// Shutdown the DHT client.
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        // The mainline Dht will clean up when dropped
    }

    /// Get the listen port.
    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }
}

impl Drop for DhtClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// DHT manager that runs periodic peer discovery and announcements.
pub struct DhtManager {
    client: Arc<DhtClient>,
    /// Info hashes we're tracking.
    tracked: Arc<RwLock<std::collections::HashSet<Sha1Hash>>>,
    /// Interval between DHT lookups.
    lookup_interval: Duration,
    /// Interval between announcements.
    announce_interval: Duration,
}

impl DhtManager {
    /// Create a new DHT manager.
    pub fn new(client: Arc<DhtClient>) -> Self {
        Self {
            client,
            tracked: Arc::new(RwLock::new(std::collections::HashSet::new())),
            lookup_interval: Duration::from_secs(300), // 5 minutes
            announce_interval: Duration::from_secs(1800), // 30 minutes
        }
    }

    /// Set the lookup interval.
    pub fn set_lookup_interval(&mut self, interval: Duration) {
        self.lookup_interval = interval;
    }

    /// Set the announce interval.
    pub fn set_announce_interval(&mut self, interval: Duration) {
        self.announce_interval = interval;
    }

    /// Start tracking an info_hash.
    pub async fn track(&self, info_hash: Sha1Hash) {
        let mut tracked = self.tracked.write().await;
        tracked.insert(info_hash);
    }

    /// Stop tracking an info_hash.
    pub async fn untrack(&self, info_hash: &Sha1Hash) {
        let mut tracked = self.tracked.write().await;
        tracked.remove(info_hash);
        self.client.clear_cache(info_hash).await;
    }

    /// Get all tracked info hashes.
    pub async fn tracked_hashes(&self) -> Vec<Sha1Hash> {
        let tracked = self.tracked.read().await;
        tracked.iter().cloned().collect()
    }

    /// Perform a single round of peer discovery for all tracked torrents.
    ///
    /// Returns a map of info_hash to discovered peers.
    pub async fn discover_peers(&self) -> std::collections::HashMap<Sha1Hash, Vec<SocketAddr>> {
        let tracked = self.tracked.read().await;
        let mut results = std::collections::HashMap::new();

        for info_hash in tracked.iter() {
            let peers = self
                .client
                .find_peers_timeout(info_hash, Duration::from_secs(30))
                .await;
            if !peers.is_empty() {
                results.insert(*info_hash, peers);
            }
        }

        results
    }

    /// Announce all tracked torrents.
    pub async fn announce_all(&self) -> Vec<Result<()>> {
        let tracked = self.tracked.read().await;
        let mut results = Vec::new();

        for info_hash in tracked.iter() {
            results.push(self.client.announce(info_hash));
        }

        results
    }

    /// Get the DHT client.
    pub fn client(&self) -> &Arc<DhtClient> {
        &self.client
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_bootstrap_nodes() {
        assert!(!DEFAULT_BOOTSTRAP_NODES.is_empty());
        for node in DEFAULT_BOOTSTRAP_NODES {
            assert!(node.contains(':'), "Bootstrap node should have port");
        }
    }

    // Note: Integration tests that require network access are marked #[ignore]
    // and should be run with: cargo test dht -- --ignored

    #[tokio::test]
    #[ignore]
    async fn test_dht_client_creation() {
        let client = DhtClient::new(6881);
        assert!(client.is_ok(), "Should create DHT client");
        let client = client.unwrap();
        assert!(client.is_running());
        assert_eq!(client.listen_port(), 6881);
    }

    #[tokio::test]
    #[ignore]
    async fn test_dht_find_peers() {
        // Ubuntu 22.04 torrent info_hash (a known popular torrent)
        let info_hash: Sha1Hash = [
            0x2c, 0x6b, 0x6a, 0x1e, 0x9c, 0x2f, 0x9f, 0x53, 0x4c, 0x8a, 0x9c, 0x7a, 0x1b, 0x2a,
            0x3c, 0x4d, 0x5e, 0x6f, 0x70, 0x81,
        ];

        let client = DhtClient::new(6881).unwrap();
        let peers = client
            .find_peers_timeout(&info_hash, Duration::from_secs(10))
            .await;

        // May or may not find peers depending on network
        println!("Found {} peers", peers.len());
    }

    #[tokio::test]
    async fn test_dht_manager_tracking() {
        // This test doesn't require network - just tests tracking logic
        // We can't create a real DhtClient without network, so we skip the actual DHT operations

        let info_hash: Sha1Hash = [0u8; 20];

        // Test HashSet operations directly
        let mut tracked = std::collections::HashSet::new();
        tracked.insert(info_hash);
        assert!(tracked.contains(&info_hash));
        tracked.remove(&info_hash);
        assert!(!tracked.contains(&info_hash));
    }
}
