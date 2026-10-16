#![cfg(feature = "torrent")]
//! Torrent Integration Tests
//!
//! These tests exercise the BitTorrent downloading stack using MockPeer and
//! TestTorrentBuilder infrastructure. They cover peer handshake, piece
//! downloading, hash verification, state transitions, and engine-level
//! torrent management.

mod mock_peer;
mod test_helpers;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gosh_dl::torrent::{Metainfo, PeerConnection, PieceManager, TorrentConfig, TorrentDownloader};
use gosh_dl::{DownloadEngine, DownloadEvent, DownloadId, DownloadOptions, EngineConfig};
use sha1::{Digest, Sha1};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;

use mock_peer::{MockPeer, MockPeerConfig};
use test_helpers::TestTorrentBuilder;

// =============================================================================
// Helpers
// =============================================================================

/// Create a TorrentConfig with all network discovery disabled (no DHT/LPD/PEX).
fn test_torrent_config() -> TorrentConfig {
    TorrentConfig {
        max_peers: 10,
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        seed_ratio: Some(0.0), // Stop immediately after download
        tick_interval_ms: 50,
        connect_interval_secs: 1,
        choking_interval_secs: 5,
        request_timeout: Duration::from_secs(10),
        ..TorrentConfig::default()
    }
}

/// Build test torrent data and return (torrent_bytes, metainfo, piece_data_vec).
///
/// `piece_length` should be a multiple of BLOCK_SIZE (16384) for simplicity.
/// `num_pieces` determines how many pieces the torrent has.
fn build_test_torrent(
    name: &str,
    piece_length: usize,
    num_pieces: usize,
) -> (Vec<u8>, Metainfo, Vec<Vec<u8>>) {
    let total_size = piece_length * num_pieces;
    let content: Vec<u8> = (0..total_size).map(|i| (i % 256) as u8).collect();

    // Use the name directly as both the torrent name and filename.
    // For single-file torrents, PieceManager saves to save_dir/metainfo.info.name.
    let (torrent_data, _piece_hashes) = TestTorrentBuilder::new(name)
        .piece_length(piece_length as u64)
        .add_file(name, content.clone())
        .build();

    let metainfo = Metainfo::parse(&torrent_data)
        .expect("TestTorrentBuilder should produce valid torrent data");

    // Slice content into piece data
    let mut pieces = Vec::with_capacity(num_pieces);
    for i in 0..num_pieces {
        let start = i * piece_length;
        let end = (start + piece_length).min(content.len());
        pieces.push(content[start..end].to_vec());
    }

    (torrent_data, metainfo, pieces)
}

/// Create a MockPeer that has all pieces of the given torrent.
async fn create_seeder(info_hash: [u8; 20], piece_data: &[Vec<u8>]) -> Arc<MockPeer> {
    let num_pieces = piece_data.len();
    let mut config = MockPeerConfig::new(info_hash, num_pieces);
    config.auto_unchoke = true;
    config.support_extensions = true;

    for (i, data) in piece_data.iter().enumerate() {
        config = config.with_piece(i as u32, data.clone());
    }

    let peer = MockPeer::new(config).await.expect("MockPeer should bind");
    let peer = Arc::new(peer);
    peer.clone().start_accepting();
    peer
}

/// Create a TorrentDownloader, inject peers, start and run it.
/// Returns when the download completes or the timeout expires.
async fn run_torrent_download(
    metainfo: Metainfo,
    peers: &[SocketAddr],
    save_dir: &std::path::Path,
    timeout_secs: u64,
) -> Arc<TorrentDownloader> {
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, _event_rx) = broadcast::channel(64);

    // Override announce URL to empty so start() doesn't try to contact real trackers
    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(id, metainfo, save_dir.to_path_buf(), config, event_tx)
            .expect("TorrentDownloader creation should succeed"),
    );

    // start() verifies existing files and sets state
    Arc::clone(&downloader)
        .start()
        .await
        .expect("start() should succeed with no trackers");

    // Inject mock peer addresses
    downloader.add_known_peers(peers.iter().cloned());

    // Run the peer loop in a background task
    let dl = Arc::clone(&downloader);
    let loop_handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    // Poll for completion
    let start = std::time::Instant::now();
    let deadline = Duration::from_secs(timeout_secs);
    while start.elapsed() < deadline {
        if downloader.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Stop the peer loop
    downloader.pause();
    tokio::time::sleep(Duration::from_millis(200)).await;
    loop_handle.abort();

    downloader
}

// =============================================================================
// Metainfo Parsing Tests
// =============================================================================

#[test]
fn test_metainfo_parse_from_builder() {
    let piece_length = 16384;
    let num_pieces = 2;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("parse-test", piece_length, num_pieces);

    // Verify basic metainfo fields
    assert_eq!(metainfo.info.name, "parse-test");
    assert_eq!(metainfo.info.piece_length, piece_length as u64);
    assert_eq!(metainfo.info.pieces.len(), num_pieces);
    assert_eq!(metainfo.info.total_size, (piece_length * num_pieces) as u64);
    assert!(metainfo.info.is_single_file);
    assert!(!metainfo.info.private);

    // Verify piece hashes match the actual data
    for (i, data) in piece_data.iter().enumerate() {
        let mut hasher = Sha1::new();
        hasher.update(data);
        let expected_hash: [u8; 20] = hasher.finalize().into();
        assert_eq!(
            metainfo.info.pieces[i], expected_hash,
            "Piece {} hash should match computed hash",
            i
        );
    }
}

#[test]
fn test_metainfo_parse_multi_file() {
    let (torrent_data, _) = TestTorrentBuilder::multi_file("multi-test").build();
    let metainfo = Metainfo::parse(&torrent_data).expect("Should parse multi-file torrent");

    assert_eq!(metainfo.info.name, "multi-test");
    assert!(!metainfo.info.is_single_file);
    assert!(
        metainfo.info.files.len() >= 3,
        "Should have at least 3 files"
    );
    assert!(metainfo.info.total_size > 0);
}

#[test]
fn test_engine_rejects_invalid_torrent_data() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let temp_dir = TempDir::new().unwrap();
        let config = EngineConfig {
            download_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };
        let engine = DownloadEngine::new(config).await.unwrap();

        // Empty data
        let result = engine.add_torrent(b"", DownloadOptions::default()).await;
        assert!(result.is_err(), "Should reject empty torrent data");

        // Invalid bencode
        let result = engine
            .add_torrent(b"not valid bencode", DownloadOptions::default())
            .await;
        assert!(result.is_err(), "Should reject invalid bencode");

        // Valid bencode but missing info dict
        let result = engine
            .add_torrent(b"d8:announcei42ee", DownloadOptions::default())
            .await;
        assert!(result.is_err(), "Should reject torrent without info dict");

        engine.shutdown().await.ok();
    });
}

// =============================================================================
// Peer Connection Tests
// =============================================================================

#[tokio::test]
async fn test_peer_handshake_with_mock_peer() {
    let piece_length = 16384;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("handshake-test", piece_length, 1);
    let info_hash = metainfo.info_hash;

    // Create and start the mock peer
    let mock_peer = create_seeder(info_hash, &piece_data).await;
    let peer_addr = mock_peer.addr();

    // Connect with PeerConnection
    let our_peer_id = *b"-GO0001-testhandshak";
    let conn = PeerConnection::connect(peer_addr, info_hash, our_peer_id, 1).await;

    assert!(conn.is_ok(), "Should successfully handshake with MockPeer");

    let conn = conn.unwrap();
    assert_eq!(conn.addr(), peer_addr);
    assert!(
        conn.peer_id().is_some(),
        "Should have peer ID after handshake"
    );
}

#[tokio::test]
async fn test_peer_handshake_wrong_info_hash() {
    let piece_length = 16384;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("wrong-hash-test", piece_length, 1);
    let info_hash = metainfo.info_hash;

    let mock_peer = create_seeder(info_hash, &piece_data).await;
    let peer_addr = mock_peer.addr();

    // Try to connect with a different info_hash
    let wrong_hash = [0xFFu8; 20];
    let our_peer_id = *b"-GO0001-wronghashtes";
    let conn = PeerConnection::connect(peer_addr, wrong_hash, our_peer_id, 1).await;

    assert!(conn.is_err(), "Should fail with mismatched info_hash");
}

// =============================================================================
// Single Piece Download Tests
// =============================================================================

#[tokio::test]
async fn test_single_piece_download() {
    let piece_length = 16384; // Exactly one block
    let (_torrent_data, metainfo, piece_data) = build_test_torrent("single-piece", piece_length, 1);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let mock_peer = create_seeder(info_hash, &piece_data).await;

    let downloader = run_torrent_download(metainfo, &[mock_peer.addr()], temp_dir.path(), 30).await;

    assert!(
        downloader.is_complete(),
        "Single-piece download should complete"
    );

    // Verify the downloaded file
    let file_path = temp_dir.path().join("single-piece");
    assert!(file_path.exists(), "Downloaded file should exist");

    let content = tokio::fs::read(&file_path).await.unwrap();
    assert_eq!(
        content, piece_data[0],
        "File content should match piece data"
    );
}

#[tokio::test]
async fn test_multi_piece_download() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("multi-piece", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let mock_peer = create_seeder(info_hash, &piece_data).await;

    let downloader = run_torrent_download(metainfo, &[mock_peer.addr()], temp_dir.path(), 30).await;

    assert!(
        downloader.is_complete(),
        "Multi-piece download should complete"
    );

    // Verify the file content
    let file_path = temp_dir.path().join("multi-piece");
    assert!(file_path.exists(), "Downloaded file should exist");

    let content = tokio::fs::read(&file_path).await.unwrap();
    let expected: Vec<u8> = piece_data.into_iter().flatten().collect();
    assert_eq!(
        content.len(),
        expected.len(),
        "File size should match total torrent size"
    );
    assert_eq!(content, expected, "File content should match");
}

// =============================================================================
// Multiple Peer Tests
// =============================================================================

#[tokio::test]
async fn test_download_from_multiple_peers() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("multi-peer", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();

    // Create 3 seeders, all with full data
    let peer1 = create_seeder(info_hash, &piece_data).await;
    let peer2 = create_seeder(info_hash, &piece_data).await;
    let peer3 = create_seeder(info_hash, &piece_data).await;

    let peer_addrs = vec![peer1.addr(), peer2.addr(), peer3.addr()];

    let downloader = run_torrent_download(metainfo, &peer_addrs, temp_dir.path(), 30).await;

    assert!(
        downloader.is_complete(),
        "Download from multiple peers should complete"
    );

    // Verify content
    let file_path = temp_dir.path().join("multi-peer");
    let content = tokio::fs::read(&file_path).await.unwrap();
    let expected: Vec<u8> = piece_data.into_iter().flatten().collect();
    assert_eq!(content, expected);
}

// =============================================================================
// State Transition Tests
// =============================================================================

#[tokio::test]
async fn test_torrent_state_transitions() {
    let piece_length = 16384;
    let (_torrent_data, metainfo, piece_data) = build_test_torrent("state-test", piece_length, 2);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, _rx) = broadcast::channel(64);

    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            id,
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx,
        )
        .unwrap(),
    );

    // Initial state should be Checking
    assert_eq!(
        downloader.state(),
        gosh_dl::torrent::TorrentState::Checking,
        "Initial state should be Checking"
    );

    // After start(), state should be Downloading (since no files exist)
    let mock_peer = create_seeder(info_hash, &piece_data).await;
    downloader.add_known_peers(std::iter::once(mock_peer.addr()));

    Arc::clone(&downloader).start().await.unwrap();

    assert_eq!(
        downloader.state(),
        gosh_dl::torrent::TorrentState::Downloading,
        "State should be Downloading after start()"
    );

    // Run peer loop until complete
    let dl = Arc::clone(&downloader);
    let handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if downloader.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(downloader.is_complete(), "Download should complete");

    // After completion, state should be Seeding (or Stopped if seed_ratio=0)
    let final_state = downloader.state();
    assert!(
        final_state == gosh_dl::torrent::TorrentState::Seeding
            || final_state == gosh_dl::torrent::TorrentState::Stopped,
        "Final state should be Seeding or Stopped, got {:?}",
        final_state
    );

    downloader.pause();
    handle.abort();
}

// =============================================================================
// Pause/Resume Tests
// =============================================================================

#[tokio::test]
async fn test_torrent_pause() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("pause-test", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, _rx) = broadcast::channel(64);

    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            id,
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx,
        )
        .unwrap(),
    );

    let mock_peer = create_seeder(info_hash, &piece_data).await;
    downloader.add_known_peers(std::iter::once(mock_peer.addr()));

    Arc::clone(&downloader).start().await.unwrap();

    // Start peer loop
    let dl = Arc::clone(&downloader);
    let handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    // Let it start downloading
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Pause
    downloader.pause();
    assert_eq!(
        downloader.state(),
        gosh_dl::torrent::TorrentState::Paused,
        "Should be paused"
    );

    // The peer loop should exit due to Paused state
    let _ = timeout(Duration::from_secs(5), handle).await;

    // Resume
    downloader.resume();
    let state = downloader.state();
    assert!(
        state == gosh_dl::torrent::TorrentState::Downloading
            || state == gosh_dl::torrent::TorrentState::Seeding,
        "Should resume to Downloading or Seeding, got {:?}",
        state
    );
}

// =============================================================================
// Piece Manager Tests
// =============================================================================

#[tokio::test]
async fn test_piece_manager_verify_existing() {
    let piece_length = 16384;
    let num_pieces = 2;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("verify-test", piece_length, num_pieces);

    let temp_dir = TempDir::new().unwrap();

    // Write piece data to disk as if it was already downloaded.
    // PieceManager expects the file at save_dir/metainfo.info.name.
    let file_path = temp_dir.path().join(&metainfo.info.name);
    let full_content: Vec<u8> = piece_data.iter().flatten().copied().collect();
    tokio::fs::write(&file_path, &full_content).await.unwrap();

    // Create PieceManager and verify
    let metainfo = Arc::new(metainfo);
    let pm = PieceManager::new(metainfo, temp_dir.path().to_path_buf());

    let valid_count = pm.verify_existing().await.unwrap();
    assert_eq!(
        valid_count, num_pieces,
        "Should verify all {} pieces",
        num_pieces
    );
    assert!(
        pm.is_complete(),
        "Should be complete after verifying all pieces"
    );
}

#[tokio::test]
async fn test_piece_manager_detects_corrupt_data() {
    let piece_length = 16384;
    let num_pieces = 2;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("corrupt-test", piece_length, num_pieces);

    let temp_dir = TempDir::new().unwrap();

    // Write corrupt data (flip some bytes in piece 1)
    let mut corrupt_content: Vec<u8> = piece_data.iter().flatten().copied().collect();
    let midpoint = piece_length + piece_length / 2;
    corrupt_content[midpoint] ^= 0xFF; // Flip a byte in piece 1

    let file_path = temp_dir.path().join(&metainfo.info.name);
    tokio::fs::write(&file_path, &corrupt_content)
        .await
        .unwrap();

    let metainfo = Arc::new(metainfo);
    let pm = PieceManager::new(metainfo, temp_dir.path().to_path_buf());

    let valid_count = pm.verify_existing().await.unwrap();
    assert_eq!(
        valid_count, 1,
        "Should verify only piece 0 (piece 1 is corrupt)"
    );
    assert!(
        !pm.is_complete(),
        "Should not be complete with corrupt piece"
    );
}

// =============================================================================
// Event System Tests
// =============================================================================

#[tokio::test]
async fn test_torrent_events() {
    let piece_length = 16384;
    let (_torrent_data, metainfo, piece_data) = build_test_torrent("events-test", piece_length, 1);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, mut event_rx) = broadcast::channel(64);

    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            id,
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx.clone(),
        )
        .unwrap(),
    );

    let mock_peer = create_seeder(info_hash, &piece_data).await;
    downloader.add_known_peers(std::iter::once(mock_peer.addr()));

    Arc::clone(&downloader).start().await.unwrap();

    // Manually emit a Started event (the engine normally does this)
    let _ = event_tx.send(DownloadEvent::Started { id });

    let dl = Arc::clone(&downloader);
    let handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    // Collect events
    let mut received_started = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        match timeout(Duration::from_millis(100), event_rx.recv()).await {
            Ok(Ok(DownloadEvent::Started { id: eid })) if eid == id => {
                received_started = true;
            }
            _ => {}
        }
        if downloader.is_complete() {
            break;
        }
    }

    assert!(received_started, "Should receive Started event");
    assert!(downloader.is_complete(), "Download should complete");

    downloader.pause();
    handle.abort();
}

// =============================================================================
// Engine-Level Torrent Tests
// =============================================================================

#[tokio::test]
async fn test_engine_add_torrent_creates_status() {
    let temp_dir = TempDir::new().unwrap();
    let piece_length = 16384;
    let (torrent_data, _metainfo, _piece_data) = build_test_torrent("engine-test", piece_length, 2);

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_concurrent_downloads: 4,
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config).await.unwrap();

    let id = engine
        .add_torrent(&torrent_data, DownloadOptions::default())
        .await
        .expect("Should accept valid torrent data");

    // Check status was created
    let status = engine.status(id);
    assert!(status.is_some(), "Should have status after add_torrent");

    let status = status.unwrap();
    assert_eq!(status.kind, gosh_dl::DownloadKind::Torrent);
    assert!(
        status.torrent_info.is_some(),
        "Should have torrent info in status"
    );

    // Verify the torrent info includes correct metadata
    let torrent_info = status.torrent_info.unwrap();
    assert_eq!(torrent_info.pieces_count, 2);

    // Verify download is listed
    let list = engine.list();
    assert!(
        list.iter().any(|s| s.id == id),
        "Download should appear in list"
    );

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_engine_add_torrent_with_options() {
    let temp_dir = TempDir::new().unwrap();
    // Build a multi-file torrent
    let (torrent_data, _) = TestTorrentBuilder::multi_file("options-test").build();

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config).await.unwrap();

    // Add with sequential mode
    let options = DownloadOptions {
        sequential: Some(true),
        ..Default::default()
    };

    let id = engine
        .add_torrent(&torrent_data, options)
        .await
        .expect("Should add torrent with options");

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.kind, gosh_dl::DownloadKind::Torrent);

    engine.shutdown().await.ok();
}

// =============================================================================
// Progress Tracking Tests
// =============================================================================

#[tokio::test]
async fn test_torrent_progress_tracking() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("progress-test", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, _rx) = broadcast::channel(64);

    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            id,
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx,
        )
        .unwrap(),
    );

    // Before start: progress should be empty
    let progress = downloader.progress();
    assert_eq!(progress.completed_size, 0, "Should start with 0 completed");

    let mock_peer = create_seeder(info_hash, &piece_data).await;
    downloader.add_known_peers(std::iter::once(mock_peer.addr()));

    Arc::clone(&downloader).start().await.unwrap();

    // After start with no existing files: completed should still be 0
    let progress = downloader.progress();
    assert_eq!(
        progress.total_size,
        Some((piece_length * num_pieces) as u64),
        "Total size should be set"
    );
    assert!(
        progress.completed_size <= progress.total_size.unwrap(),
        "Torrent progress must not exceed total size"
    );

    // Run download
    let dl = Arc::clone(&downloader);
    let handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if downloader.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(downloader.is_complete(), "Download should complete");

    let final_progress = downloader.progress();
    assert_eq!(
        final_progress.completed_size,
        final_progress.total_size.unwrap(),
        "Completed torrent progress should equal total size"
    );
    assert!(
        final_progress.percentage() <= 100.0 + f64::EPSILON,
        "Torrent percentage must not exceed 100%"
    );

    downloader.pause();
    handle.abort();
}

// =============================================================================
// Larger Download Tests
// =============================================================================

#[tokio::test]
async fn test_download_multi_block_pieces() {
    // Each piece = 2 blocks (32768 bytes)
    let piece_length = 16384 * 2; // 32KB
    let num_pieces = 3;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("multi-block", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let mock_peer = create_seeder(info_hash, &piece_data).await;

    let downloader = run_torrent_download(metainfo, &[mock_peer.addr()], temp_dir.path(), 30).await;

    assert!(
        downloader.is_complete(),
        "Multi-block piece download should complete"
    );

    let file_path = temp_dir.path().join("multi-block");
    let content = tokio::fs::read(&file_path).await.unwrap();
    let expected: Vec<u8> = piece_data.into_iter().flatten().collect();
    assert_eq!(content.len(), expected.len());
    assert_eq!(content, expected);
}

// =============================================================================
// Resume Tests
// =============================================================================

#[tokio::test]
async fn test_resume_partial_download() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("resume-test", piece_length, num_pieces);

    let temp_dir = TempDir::new().unwrap();

    // Pre-write the first 2 pieces (simulating a partial download)
    let file_path = temp_dir.path().join(&metainfo.info.name);
    let mut partial_content = piece_data[0].clone();
    partial_content.extend_from_slice(&piece_data[1]);
    // Pad to full size so piece manager can verify piece boundaries
    partial_content.resize(piece_length * num_pieces, 0);
    tokio::fs::write(&file_path, &partial_content)
        .await
        .unwrap();

    // Create PieceManager and verify — should find 2 valid pieces
    let metainfo_arc = Arc::new(metainfo.clone());
    let pm = PieceManager::new(metainfo_arc, temp_dir.path().to_path_buf());
    let valid = pm.verify_existing().await.unwrap();
    assert_eq!(valid, 2, "Should verify 2 pre-existing pieces");

    // Now do a full download to get the remaining pieces
    let info_hash = metainfo.info_hash;
    let mock_peer = create_seeder(info_hash, &piece_data).await;

    let downloader = run_torrent_download(metainfo, &[mock_peer.addr()], temp_dir.path(), 30).await;

    assert!(downloader.is_complete(), "Resumed download should complete");

    // Verify complete file
    let content = tokio::fs::read(&file_path).await.unwrap();
    let expected: Vec<u8> = piece_data.into_iter().flatten().collect();
    assert_eq!(content, expected, "Final content should match");
}

// =============================================================================
// Inbound Seeding Tests
// =============================================================================

/// A raw inbound leecher can connect to the seeder's listener, handshake,
/// get unchoked, and download a block — end-to-end proof that the advertised
/// port actually accepts and serves peers.
#[tokio::test]
async fn test_inbound_peer_downloads_from_seeder() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let piece_length = 16384;
    let (_torrent_data, mut metainfo, piece_data) =
        build_test_torrent("inbound-seed", piece_length, 1);
    let info_hash = metainfo.info_hash;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    // Pre-place the complete file so the downloader starts as a seeder
    let temp_dir = TempDir::new().unwrap();
    tokio::fs::write(temp_dir.path().join("inbound-seed"), &piece_data[0])
        .await
        .unwrap();

    let config = TorrentConfig {
        // Port 0: let the OS pick a free port
        listen_port_range: (0, 0),
        seed_ratio: None,
        choking_interval_secs: 2,
        ..test_torrent_config()
    };
    let (event_tx, _event_rx) = broadcast::channel(64);
    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            DownloadId::new(),
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx,
        )
        .unwrap(),
    );
    Arc::clone(&downloader).start().await.unwrap();
    let port = downloader
        .bound_listen_port()
        .expect("inbound listener should be bound");

    // The peer loop computes choking decisions
    let loop_handle = tokio::spawn(Arc::clone(&downloader).run_peer_loop());

    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect to seeder listener");

        // BitTorrent handshake
        let mut hs = Vec::with_capacity(68);
        hs.push(19u8);
        hs.extend_from_slice(b"BitTorrent protocol");
        hs.extend_from_slice(&[0u8; 8]);
        hs.extend_from_slice(&info_hash);
        hs.extend_from_slice(b"-TEST01-121212121212");
        sock.write_all(&hs).await.unwrap();

        let mut resp = [0u8; 68];
        sock.read_exact(&mut resp).await.unwrap();
        assert_eq!(&resp[28..48], &info_hash, "handshake info_hash mismatch");

        // Tell the seeder we're interested
        sock.write_all(&[0, 0, 0, 1, 2]).await.unwrap();

        // Read messages until Unchoke (id 1), sending keep-alives to drive
        // the seeder's per-peer loop.
        let mut unchoked = false;
        while !unchoked {
            let msg =
                tokio::time::timeout(Duration::from_secs(3), read_wire_message(&mut sock)).await;
            match msg {
                Ok((id, _payload)) => {
                    if id == Some(1) {
                        unchoked = true;
                    }
                }
                Err(_) => {
                    // Idle: nudge the seeder's loop with a keep-alive
                    sock.write_all(&[0, 0, 0, 0]).await.unwrap();
                }
            }
        }

        // Request piece 0, offset 0, full block
        let mut req = Vec::with_capacity(17);
        req.extend_from_slice(&13u32.to_be_bytes());
        req.push(6);
        req.extend_from_slice(&0u32.to_be_bytes());
        req.extend_from_slice(&0u32.to_be_bytes());
        req.extend_from_slice(&(piece_length as u32).to_be_bytes());
        sock.write_all(&req).await.unwrap();

        // Read until the Piece message (id 7) arrives
        loop {
            let (id, payload) = read_wire_message(&mut sock).await;
            if id == Some(7) {
                assert_eq!(payload.len(), 8 + piece_length, "piece payload size");
                assert_eq!(
                    &payload[8..],
                    &piece_data[0][..],
                    "served block should match the seeded content"
                );
                break;
            }
        }
    })
    .await;

    loop_handle.abort();
    downloader.stop().await.ok();
    result.expect("inbound download should complete within the timeout");
}

/// Read one length-prefixed wire message; returns (message id, full payload
/// after the id byte). A keep-alive returns (None, empty).
async fn read_wire_message(sock: &mut tokio::net::TcpStream) -> (Option<u8>, Vec<u8>) {
    use tokio::io::AsyncReadExt;

    let mut len_buf = [0u8; 4];
    sock.read_exact(&mut len_buf).await.expect("read length");
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return (None, Vec::new());
    }
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).await.expect("read body");
    (Some(body[0]), body[1..].to_vec())
}

// =============================================================================
// Streaming Reader Tests
// =============================================================================

/// Contiguous availability follows verified pieces, stopping at gaps.
#[tokio::test]
async fn test_contiguous_available_from() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("contig-test", piece_length, num_pieces);

    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join(&metainfo.info.name);
    let mut content: Vec<u8> = piece_data.iter().flatten().copied().collect();
    // Corrupt piece 2 so verification rejects it: pieces 0,1,3 verified.
    content[2 * piece_length] ^= 0xFF;
    tokio::fs::write(&file_path, &content).await.unwrap();

    let pm = PieceManager::new(Arc::new(metainfo), temp_dir.path().to_path_buf());
    let valid = pm.verify_existing().await.unwrap();
    assert_eq!(valid, 3, "pieces 0, 1, 3 should verify");

    let plen = piece_length as u64;
    assert_eq!(pm.contiguous_available_from(0), 2 * plen);
    assert_eq!(pm.contiguous_available_from(plen), plen);
    assert_eq!(pm.contiguous_available_from(plen + 100), plen - 100);
    assert_eq!(pm.contiguous_available_from(2 * plen), 0, "gap at piece 2");
    assert_eq!(pm.contiguous_available_from(3 * plen), plen);
    assert_eq!(pm.contiguous_available_from(4 * plen), 0, "past the end");
}

/// An active stream position overrides rarest-first so the reader's next
/// pieces are fetched first.
#[tokio::test]
async fn test_stream_position_prioritizes_pieces() {
    use bitvec::prelude::*;

    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, _piece_data) =
        build_test_torrent("stream-prio", piece_length, num_pieces);

    let temp_dir = TempDir::new().unwrap();
    let pm = PieceManager::new(Arc::new(metainfo), temp_dir.path().to_path_buf());

    let peer_has: BitVec<u8, Msb0> = bitvec![u8, Msb0; 1; num_pieces];

    // Without a stream position, ties break by index → piece 0.
    assert_eq!(pm.select_piece(&peer_has), Some(0));

    // With the reader at piece 2, pieces 2.. come first.
    pm.set_stream_position(Some(2));
    assert_eq!(pm.select_piece(&peer_has), Some(2));

    // Clearing restores normal selection.
    pm.set_stream_position(None);
    assert_eq!(pm.select_piece(&peer_has), Some(0));
}

/// The engine-level reader streams a torrent's full content.
#[tokio::test]
async fn test_reader_streams_completed_torrent() {
    use tokio::io::AsyncReadExt;

    let piece_length = 16384;
    let num_pieces = 3;
    let (torrent_data, metainfo, piece_data) =
        build_test_torrent("reader-torrent", piece_length, num_pieces);
    let content: Vec<u8> = piece_data.iter().flatten().copied().collect();

    let temp_dir = TempDir::new().unwrap();
    // Pre-place the complete file so the torrent verifies as complete.
    tokio::fs::write(temp_dir.path().join(&metainfo.info.name), &content)
        .await
        .unwrap();

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config).await.unwrap();
    let id = engine
        .add_torrent(&torrent_data, DownloadOptions::default())
        .await
        .unwrap();

    let mut reader = engine.open_reader(id, 0).expect("open reader");
    let mut streamed = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), reader.read_to_end(&mut streamed))
        .await
        .expect("stream should not stall")
        .expect("stream read should succeed");

    assert_eq!(streamed, content, "streamed bytes should match the torrent");

    // Offset reads work too.
    let mut reader = engine
        .open_reader(id, 20000)
        .expect("open reader at offset");
    let mut tail = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), reader.read_to_end(&mut tail))
        .await
        .expect("offset stream should not stall")
        .expect("offset read should succeed");
    assert_eq!(tail, &content[20000..]);

    engine.shutdown().await.ok();
}

/// verify() re-hashes torrent pieces and reports corruption; the piece
/// bitmap is rebuilt so the bad piece becomes wanted again.
#[tokio::test]
async fn test_verify_detects_corrupt_torrent_piece() {
    let piece_length = 16384;
    let num_pieces = 3;
    let (torrent_data, metainfo, piece_data) =
        build_test_torrent("verify-torrent", piece_length, num_pieces);
    let content: Vec<u8> = piece_data.iter().flatten().copied().collect();

    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join(&metainfo.info.name);
    tokio::fs::write(&file_path, &content).await.unwrap();

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config).await.unwrap();
    let id = engine
        .add_torrent(&torrent_data, DownloadOptions::default())
        .await
        .unwrap();

    // Wait for the torrent to finish startup verification and settle into
    // Seeding (verify() refuses while actively downloading).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = engine.status(id).map(|s| s.state);
        if matches!(
            state,
            Some(gosh_dl::DownloadState::Seeding) | Some(gosh_dl::DownloadState::Completed)
        ) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "torrent never reached Seeding, state: {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let report = engine.verify(id).await.expect("verify should run");
    assert!(
        report.valid,
        "intact torrent should verify: {}",
        report.detail
    );
    assert_eq!(report.bad_pieces, 0);

    // Corrupt the middle piece on disk.
    let mut bad = tokio::fs::read(&file_path).await.unwrap();
    bad[piece_length + 5] ^= 0xFF;
    tokio::fs::write(&file_path, &bad).await.unwrap();

    let report = engine.verify(id).await.expect("verify should run");
    assert!(!report.valid, "corrupt piece must fail verification");
    assert_eq!(report.bad_pieces, 1, "exactly one piece should be bad");

    engine.shutdown().await.ok();
}

// =============================================================================
// Rollout lifecycle regressions
// =============================================================================

#[tokio::test]
async fn paused_queued_torrent_waits_for_explicit_resume() {
    use gosh_dl::{torrent::BencodeValue, DownloadState};
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        download_dir: dir.path().into(),
        max_concurrent_downloads: 1,
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let mut ids = Vec::new();
    for name in ["occupy-slot", "queued-torrent"] {
        let (data, _, _) = build_test_torrent(name, 16384, 1);
        let mut value = BencodeValue::parse_exact(&data).unwrap();
        if let BencodeValue::Dict(ref mut dict) = value {
            dict.remove(b"announce".as_slice());
        }
        ids.push(
            engine
                .add_torrent(&value.encode(), DownloadOptions::default())
                .await
                .unwrap(),
        );
        if ids.len() == 1 {
            timeout(Duration::from_secs(3), async {
                while engine.status(ids[0]).unwrap().state == DownloadState::Queued {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
    }
    engine.pause(ids[1]).await.unwrap();
    engine.cancel(ids[0], false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(engine.status(ids[1]).unwrap().state, DownloadState::Paused);
    engine.resume(ids[1]).await.unwrap();
    timeout(Duration::from_secs(3), async {
        while engine.status(ids[1]).unwrap().state != DownloadState::Downloading {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("resumed torrent must start a live worker");
    engine.shutdown().await.unwrap();
}

#[cfg(feature = "http")]
#[tokio::test]
async fn repair_restarts_a_corrupt_seeding_torrent() {
    use gosh_dl::{torrent::BencodeValue, DownloadState};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let dir = TempDir::new().unwrap();
    let server = MockServer::start().await;
    let (data, _, pieces) = build_test_torrent("repair-seed", 16384, 1);
    let content = pieces[0].clone();
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("Content-Range", "bytes 0-16383/16384")
                .set_body_bytes(content.clone()),
        )
        .mount(&server)
        .await;
    let mut value = BencodeValue::parse_exact(&data).unwrap();
    if let BencodeValue::Dict(ref mut dict) = value {
        dict.remove(b"announce".as_slice());
        dict.insert(
            b"url-list".to_vec(),
            BencodeValue::Bytes(format!("{}/repair-seed", server.uri()).into_bytes()),
        );
    }
    tokio::fs::write(dir.path().join("repair-seed"), &content)
        .await
        .unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        download_dir: dir.path().into(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let id = engine
        .add_torrent(&value.encode(), DownloadOptions::default())
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while engine.status(id).unwrap().state != DownloadState::Seeding {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut corrupt = content.clone();
    corrupt[10] ^= 0xff;
    tokio::fs::write(dir.path().join("repair-seed"), &corrupt)
        .await
        .unwrap();
    assert!(!engine.repair(id).await.unwrap().valid);
    timeout(Duration::from_secs(10), async {
        loop {
            if tokio::fs::read(dir.path().join("repair-seed"))
                .await
                .unwrap()
                == content
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("repair must fetch and write the corrupt piece again");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn verify_and_read_a_restored_completed_torrent_without_starting_peers() {
    use gosh_dl::{
        storage::{MemoryStorage, Storage},
        torrent::BencodeValue,
        DownloadState,
    };
    use tokio::io::AsyncReadExt;
    let dir = TempDir::new().unwrap();
    let (data, _, pieces) = build_test_torrent("restored-reader", 16384, 1);
    let content = &pieces[0];
    tokio::fs::write(dir.path().join("restored-reader"), content)
        .await
        .unwrap();
    let mut value = BencodeValue::parse_exact(&data).unwrap();
    if let BencodeValue::Dict(ref mut dict) = value {
        dict.remove(b"announce".as_slice());
    }
    let storage = Arc::new(MemoryStorage::new());
    let config = EngineConfig {
        download_dir: dir.path().into(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    };
    let original = DownloadEngine::with_storage(config.clone(), storage.clone())
        .await
        .unwrap();
    let id = original
        .add_torrent(&value.encode(), DownloadOptions::default())
        .await
        .unwrap();
    original.pause(id).await.unwrap();
    let mut status = original.status(id).unwrap();
    original.shutdown().await.unwrap();
    status.state = DownloadState::Completed;
    status.progress.total_size = Some(content.len() as u64);
    status.progress.completed_size = content.len() as u64;
    storage.save_download(&status).await.unwrap();
    let engine = DownloadEngine::with_storage(config, storage).await.unwrap();
    assert!(engine.verify(id).await.unwrap().valid);
    let mut reader = engine.open_reader(id, 0).unwrap();
    let mut bytes = Vec::new();
    timeout(Duration::from_secs(2), reader.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&bytes, content);
    assert_eq!(engine.status(id).unwrap().state, DownloadState::Completed);
    let mut corrupt = content.clone();
    corrupt[0] ^= 0xff;
    tokio::fs::write(dir.path().join("restored-reader"), corrupt)
        .await
        .unwrap();
    assert!(!engine.verify(id).await.unwrap().valid);
    engine.shutdown().await.unwrap();
}

// =============================================================================
// uTP Transport Tests
// =============================================================================

/// Full torrent transfer over uTP: a seeder accepts an inbound uTP
/// connection and a uTP-only leecher downloads the complete torrent.
#[tokio::test]
async fn test_torrent_transfer_over_utp() {
    use gosh_dl::config::TransportPolicy;

    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("utp-transfer", piece_length, num_pieces);
    let content: Vec<u8> = piece_data.iter().flatten().copied().collect();

    let mut seed_metainfo = metainfo.clone();
    seed_metainfo.announce = None;
    seed_metainfo.announce_list = vec![];
    let mut leech_metainfo = metainfo;
    leech_metainfo.announce = None;
    leech_metainfo.announce_list = vec![];

    // Seeder: complete file on disk, uTP enabled on an OS-assigned port.
    let seed_dir = TempDir::new().unwrap();
    tokio::fs::write(seed_dir.path().join("utp-transfer"), &content)
        .await
        .unwrap();
    let seed_config = TorrentConfig {
        listen_port_range: (0, 0),
        enable_utp: true,
        seed_ratio: None,
        choking_interval_secs: 1,
        ..test_torrent_config()
    };
    let (seed_tx, _seed_rx) = broadcast::channel(64);
    let seeder = Arc::new(
        TorrentDownloader::from_torrent(
            DownloadId::new(),
            seed_metainfo,
            seed_dir.path().to_path_buf(),
            seed_config,
            seed_tx,
        )
        .unwrap(),
    );
    Arc::clone(&seeder).start().await.unwrap();
    let seed_utp_port = seeder
        .utp_listen_port()
        .expect("seeder should bind a uTP port");
    let seed_loop = tokio::spawn({
        let seeder = Arc::clone(&seeder);
        async move {
            seeder.run_peer_loop().await.ok();
        }
    });

    // Leecher: uTP only, no TCP fallback.
    let leech_dir = TempDir::new().unwrap();
    let leech_config = TorrentConfig {
        listen_port_range: (0, 0),
        enable_utp: true,
        ..test_torrent_config()
    };
    let (leech_tx, _leech_rx) = broadcast::channel(64);
    let leecher = Arc::new(
        TorrentDownloader::from_torrent(
            DownloadId::new(),
            leech_metainfo,
            leech_dir.path().to_path_buf(),
            leech_config,
            leech_tx,
        )
        .unwrap(),
    );
    leecher.set_transport_policy(TransportPolicy::UtpOnly, false);
    Arc::clone(&leecher).start().await.unwrap();
    leecher.add_known_peers([format!("127.0.0.1:{seed_utp_port}")
        .parse::<SocketAddr>()
        .unwrap()]);
    let leech_loop = tokio::spawn({
        let leecher = Arc::clone(&leecher);
        async move {
            leecher.run_peer_loop().await.ok();
        }
    });

    // Wait for the leecher to finish.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !leecher.is_complete() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(leecher.is_complete(), "uTP download did not complete");

    let downloaded = tokio::fs::read(leech_dir.path().join("utp-transfer"))
        .await
        .expect("downloaded file should exist");
    assert_eq!(downloaded, content, "uTP-transferred data must match");

    leech_loop.abort();
    seed_loop.abort();
    leecher.stop().await.ok();
    seeder.stop().await.ok();
}

#[tokio::test]
async fn start_paused_torrent_keeps_metainfo_without_storage() {
    use gosh_dl::{torrent::BencodeValue, DownloadState};
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        download_dir: dir.path().into(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let (data, _, _) = build_test_torrent("paused-new", 16384, 1);
    let mut value = BencodeValue::parse_exact(&data).unwrap();
    if let BencodeValue::Dict(ref mut dict) = value {
        dict.remove(b"announce".as_slice());
    }
    let id = engine
        .add_torrent(
            &value.encode(),
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(engine.status(id).unwrap().state, DownloadState::Paused);
    assert!(!engine.verify(id).await.unwrap().valid);
    engine.resume(id).await.unwrap();
    timeout(Duration::from_secs(3), async {
        while engine.status(id).unwrap().state != DownloadState::Downloading {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("paused torrent must retain metainfo for resume");
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn start_paused_magnet_does_not_start_discovery() {
    use gosh_dl::DownloadState;
    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(EngineConfig {
        download_dir: dir.path().into(),
        enable_dht: false,
        enable_lpd: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let id = engine
        .add_magnet(
            "magnet:?xt=urn:btih:0123456789012345678901234567890123456789",
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(engine.status(id).unwrap().state, DownloadState::Paused);
    engine.resume(id).await.unwrap();
    assert_ne!(engine.status(id).unwrap().state, DownloadState::Paused);
    engine.shutdown().await.unwrap();
}
