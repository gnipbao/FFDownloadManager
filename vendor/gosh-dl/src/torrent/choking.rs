//! Choking algorithm for BitTorrent peer management.
//!
//! Implements the standard BitTorrent choking algorithm:
//! - Unchoke top N peers by download rate (reciprocity)
//! - Maintain one optimistic unchoke slot for discovering new fast peers
//! - Recalculate every 10 seconds, rotate optimistic every 30 seconds

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Choking algorithm configuration.
#[derive(Debug, Clone)]
pub struct ChokingConfig {
    /// Number of peers to unchoke based on performance (default: 4).
    pub unchoke_slots: usize,
    /// Interval for recalculating unchokes (default: 10 seconds).
    pub recalculate_interval: Duration,
    /// Interval for rotating optimistic unchoke (default: 30 seconds).
    pub optimistic_interval: Duration,
}

impl Default for ChokingConfig {
    fn default() -> Self {
        Self {
            unchoke_slots: 4,
            recalculate_interval: Duration::from_secs(10),
            optimistic_interval: Duration::from_secs(30),
        }
    }
}

/// Peer statistics used for choking decisions.
#[derive(Debug, Clone)]
pub struct PeerStats {
    /// Peer address.
    pub addr: SocketAddr,
    /// Download rate from this peer (bytes/sec).
    pub download_rate: u64,
    /// Upload rate to this peer (bytes/sec).
    pub upload_rate: u64,
    /// Is this peer interested in our pieces?
    pub peer_interested: bool,
    /// Are we interested in this peer's pieces?
    pub am_interested: bool,
    /// Is this peer currently unchoked by us?
    pub is_unchoked: bool,
    /// Is this peer a seeder?
    pub is_seeder: bool,
}

/// Decision to choke or unchoke a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChokingDecision {
    /// Unchoke this peer.
    Unchoke(SocketAddr),
    /// Choke this peer.
    Choke(SocketAddr),
}

/// Manages the choking algorithm for a torrent.
pub struct ChokingManager {
    config: ChokingConfig,
    /// Last time we recalculated unchokes.
    last_recalculate: Instant,
    /// Last time we rotated the optimistic unchoke.
    last_optimistic_rotate: Instant,
    /// Current optimistic unchoke peer.
    optimistic_peer: Option<SocketAddr>,
    /// RNG for optimistic selection.
    rng_counter: u64,
}

impl ChokingManager {
    /// Create a new choking manager with the given configuration.
    pub fn new(config: ChokingConfig) -> Self {
        Self {
            config,
            last_recalculate: Instant::now(),
            // Allow immediate first optimistic selection
            last_optimistic_rotate: Instant::now() - Duration::from_secs(60),
            optimistic_peer: None,
            rng_counter: 0,
        }
    }

    /// Create a choking manager with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(ChokingConfig::default())
    }

    /// Check if it's time to recalculate unchokes.
    pub fn should_recalculate(&self) -> bool {
        self.last_recalculate.elapsed() >= self.config.recalculate_interval
    }

    /// Recalculate which peers should be unchoked.
    ///
    /// Returns a list of choking decisions (peers to unchoke and choke).
    ///
    /// # Arguments
    /// * `peers` - Map of peer addresses to their statistics
    /// * `is_seeding` - Whether we are seeding (complete) or downloading
    pub fn recalculate(
        &mut self,
        peers: &HashMap<SocketAddr, PeerStats>,
        is_seeding: bool,
    ) -> Vec<ChokingDecision> {
        let now = Instant::now();

        // Only recalculate if interval has passed
        if now.duration_since(self.last_recalculate) < self.config.recalculate_interval {
            return vec![];
        }
        self.last_recalculate = now;

        // Collect interested peers (only unchoke peers that want our data)
        let mut interested_peers: Vec<_> = peers
            .iter()
            .filter(|(_, stats)| stats.peer_interested)
            .collect();

        // Sort by performance metric
        if is_seeding {
            // When seeding: prefer peers we upload fastest to
            // This encourages fast distribution
            interested_peers.sort_by_key(|peer| std::cmp::Reverse(peer.1.upload_rate));
        } else {
            // When downloading: prefer peers we download fastest from (reciprocity)
            // This encourages tit-for-tat behavior
            interested_peers.sort_by_key(|peer| std::cmp::Reverse(peer.1.download_rate));
        }

        // Take top N peers for regular unchoke slots
        let mut to_unchoke: Vec<SocketAddr> = interested_peers
            .iter()
            .take(self.config.unchoke_slots)
            .map(|(addr, _)| **addr)
            .collect();

        // Handle optimistic unchoke rotation
        if now.duration_since(self.last_optimistic_rotate) >= self.config.optimistic_interval {
            self.last_optimistic_rotate = now;
            self.rotate_optimistic(&interested_peers, &to_unchoke);
        }

        // Add optimistic peer to unchoke list if set
        if let Some(opt_peer) = self.optimistic_peer {
            if peers.contains_key(&opt_peer) && !to_unchoke.contains(&opt_peer) {
                to_unchoke.push(opt_peer);
            }
        }

        // Build decision list
        let mut decisions = Vec::new();

        // Unchoke peers that need unchoking
        for addr in &to_unchoke {
            if let Some(stats) = peers.get(addr) {
                if !stats.is_unchoked {
                    decisions.push(ChokingDecision::Unchoke(*addr));
                }
            }
        }

        // Choke peers that were unchoked but shouldn't be anymore
        for (addr, stats) in peers {
            if stats.is_unchoked && !to_unchoke.contains(addr) {
                decisions.push(ChokingDecision::Choke(*addr));
            }
        }

        decisions
    }

    /// Rotate the optimistic unchoke slot.
    fn rotate_optimistic(
        &mut self,
        interested_peers: &[(&SocketAddr, &PeerStats)],
        currently_unchoked: &[SocketAddr],
    ) {
        // Find candidates: interested peers not already in unchoke slots
        let candidates: Vec<_> = interested_peers
            .iter()
            .filter(|(addr, _)| !currently_unchoked.contains(addr))
            .collect();

        if candidates.is_empty() {
            self.optimistic_peer = None;
            return;
        }

        // Simple pseudo-random selection using counter
        self.rng_counter = self.rng_counter.wrapping_add(1);
        let idx = (self.rng_counter as usize) % candidates.len();
        self.optimistic_peer = Some(*candidates[idx].0);
    }

    /// Get the current optimistic unchoke peer.
    pub fn optimistic_peer(&self) -> Option<SocketAddr> {
        self.optimistic_peer
    }

    /// Check if a peer is the optimistic unchoke.
    pub fn is_optimistic(&self, addr: &SocketAddr) -> bool {
        self.optimistic_peer.as_ref() == Some(addr)
    }

    /// Force recalculation on next call (useful after peer disconnect).
    pub fn invalidate(&mut self) {
        self.last_recalculate = Instant::now() - self.config.recalculate_interval;
    }

    /// Remove a peer from optimistic slot if it disconnects.
    pub fn peer_disconnected(&mut self, addr: &SocketAddr) {
        if self.optimistic_peer.as_ref() == Some(addr) {
            self.optimistic_peer = None;
            // Allow immediate re-selection
            self.last_optimistic_rotate = Instant::now() - self.config.optimistic_interval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn make_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    fn make_peer_stats(
        port: u16,
        download_rate: u64,
        interested: bool,
        unchoked: bool,
    ) -> (SocketAddr, PeerStats) {
        let addr = make_addr(port);
        (
            addr,
            PeerStats {
                addr,
                download_rate,
                upload_rate: 0,
                peer_interested: interested,
                am_interested: true,
                is_unchoked: unchoked,
                is_seeder: false,
            },
        )
    }

    #[test]
    fn test_default_config() {
        let config = ChokingConfig::default();
        assert_eq!(config.unchoke_slots, 4);
        assert_eq!(config.recalculate_interval, Duration::from_secs(10));
        assert_eq!(config.optimistic_interval, Duration::from_secs(30));
    }

    #[test]
    fn test_unchoke_top_performers() {
        let config = ChokingConfig {
            unchoke_slots: 2,
            recalculate_interval: Duration::from_millis(0), // Immediate
            optimistic_interval: Duration::from_secs(1000), // Disable for test
        };
        let mut manager = ChokingManager::new(config);

        let peers: HashMap<_, _> = vec![
            make_peer_stats(1000, 100, true, false), // Low rate
            make_peer_stats(1001, 500, true, false), // High rate
            make_peer_stats(1002, 300, true, false), // Medium rate
            make_peer_stats(1003, 50, true, false),  // Lowest rate
        ]
        .into_iter()
        .collect();

        let decisions = manager.recalculate(&peers, false);

        // Should unchoke the two highest performers (500 and 300)
        let unchoked: Vec<_> = decisions
            .iter()
            .filter_map(|d| match d {
                ChokingDecision::Unchoke(addr) => Some(addr.port()),
                _ => None,
            })
            .collect();

        assert!(unchoked.contains(&1001), "Should unchoke highest (500)");
        assert!(
            unchoked.contains(&1002),
            "Should unchoke second highest (300)"
        );
        assert!(
            !unchoked.contains(&1000),
            "Should not unchoke low performer"
        );
        assert!(
            !unchoked.contains(&1003),
            "Should not unchoke lowest performer"
        );
    }

    #[test]
    fn test_only_interested_peers() {
        let config = ChokingConfig {
            unchoke_slots: 4,
            recalculate_interval: Duration::from_millis(0),
            optimistic_interval: Duration::from_secs(1000),
        };
        let mut manager = ChokingManager::new(config);

        let peers: HashMap<_, _> = vec![
            make_peer_stats(1000, 500, true, false),   // Interested
            make_peer_stats(1001, 1000, false, false), // Not interested (high rate)
            make_peer_stats(1002, 300, true, false),   // Interested
        ]
        .into_iter()
        .collect();

        let decisions = manager.recalculate(&peers, false);

        let unchoked: Vec<_> = decisions
            .iter()
            .filter_map(|d| match d {
                ChokingDecision::Unchoke(addr) => Some(addr.port()),
                _ => None,
            })
            .collect();

        // Should only unchoke interested peers
        assert!(unchoked.contains(&1000));
        assert!(unchoked.contains(&1002));
        assert!(
            !unchoked.contains(&1001),
            "Should not unchoke uninterested peer"
        );
    }

    #[test]
    fn test_choke_previously_unchoked() {
        let config = ChokingConfig {
            unchoke_slots: 1,
            recalculate_interval: Duration::from_millis(0),
            optimistic_interval: Duration::from_secs(1000),
        };
        let mut manager = ChokingManager::new(config);

        let peers: HashMap<_, _> = vec![
            make_peer_stats(1000, 500, true, true), // Currently unchoked, high rate
            make_peer_stats(1001, 100, true, true), // Currently unchoked, low rate
        ]
        .into_iter()
        .collect();

        let decisions = manager.recalculate(&peers, false);

        // Should choke the low performer
        let choked: Vec<_> = decisions
            .iter()
            .filter_map(|d| match d {
                ChokingDecision::Choke(addr) => Some(addr.port()),
                _ => None,
            })
            .collect();

        assert!(choked.contains(&1001), "Should choke low performer");
        assert!(!choked.contains(&1000), "Should not choke high performer");
    }

    #[test]
    fn test_seeding_mode_prefers_upload_rate() {
        let config = ChokingConfig {
            unchoke_slots: 1,
            recalculate_interval: Duration::from_millis(0),
            optimistic_interval: Duration::from_secs(1000),
        };
        let mut manager = ChokingManager::new(config);

        let addr1 = make_addr(1000);
        let addr2 = make_addr(1001);

        let peers: HashMap<_, _> = vec![
            (
                addr1,
                PeerStats {
                    addr: addr1,
                    download_rate: 1000, // High download from them
                    upload_rate: 100,    // Low upload to them
                    peer_interested: true,
                    am_interested: false,
                    is_unchoked: false,
                    is_seeder: false,
                },
            ),
            (
                addr2,
                PeerStats {
                    addr: addr2,
                    download_rate: 100, // Low download from them
                    upload_rate: 1000,  // High upload to them
                    peer_interested: true,
                    am_interested: false,
                    is_unchoked: false,
                    is_seeder: false,
                },
            ),
        ]
        .into_iter()
        .collect();

        let decisions = manager.recalculate(&peers, true); // Seeding mode

        let unchoked: Vec<_> = decisions
            .iter()
            .filter_map(|d| match d {
                ChokingDecision::Unchoke(addr) => Some(addr.port()),
                _ => None,
            })
            .collect();

        // In seeding mode, should prefer peer we upload fastest to
        assert!(
            unchoked.contains(&1001),
            "Should unchoke peer with high upload rate"
        );
    }

    #[test]
    fn test_peer_disconnected() {
        let mut manager = ChokingManager::with_defaults();
        let addr = make_addr(1000);

        manager.optimistic_peer = Some(addr);
        assert_eq!(manager.optimistic_peer(), Some(addr));

        manager.peer_disconnected(&addr);
        assert_eq!(manager.optimistic_peer(), None);
    }
}
