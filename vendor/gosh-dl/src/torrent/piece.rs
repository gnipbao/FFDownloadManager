//! Piece Manager
//!
//! This module manages piece downloading, verification, and writing to disk.
//! It handles piece selection strategies (rarest first), block management,
//! and SHA-1 hash verification.

use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bitvec::prelude::*;
use parking_lot::RwLock;
use sha1::{Digest, Sha1};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::metainfo::{Metainfo, Sha1Hash};
use super::peer::BLOCK_SIZE;
use crate::error::{EngineError, ProtocolErrorKind, Result, StorageErrorKind};

/// Piece manager for coordinating downloads
pub struct PieceManager {
    metainfo: Arc<Metainfo>,
    save_dir: PathBuf,

    /// Bitfield of pieces we have
    have: RwLock<BitVec<u8, Msb0>>,

    /// Pieces currently being downloaded
    pending: RwLock<HashMap<u32, PendingPiece>>,

    /// Pieces that have been verified
    verified_count: AtomicU64,

    /// Total bytes verified
    verified_bytes: AtomicU64,

    /// Piece rarity (how many peers have each piece)
    piece_availability: RwLock<Vec<u32>>,

    /// Pieces that should be downloaded (for selective file download)
    /// If None, all pieces are wanted. If Some, only marked pieces are wanted.
    wanted_pieces: RwLock<Option<BitVec<u8, Msb0>>>,

    /// Sequential download mode - download pieces in order for streaming
    sequential_mode: RwLock<bool>,
    /// Streaming read position (piece index); u32::MAX = no active stream.
    /// When set, piece selection prioritizes pieces at/after this position so
    /// the reader is never starved.
    stream_position: std::sync::atomic::AtomicU32,
}

/// A piece being downloaded
#[derive(Debug)]
pub struct PendingPiece {
    /// Piece index
    pub index: u32,
    /// Expected piece length
    pub length: u64,
    /// Blocks in this piece (None = not yet received)
    pub blocks: Vec<Option<Vec<u8>>>,
    /// Block size used
    pub block_size: u32,
    /// Number of blocks received
    pub blocks_received: usize,
    /// When we started downloading this piece
    pub started_at: Instant,
    /// Last time we received a block (for stale detection)
    pub last_activity: Instant,
    /// Which blocks have been requested (block index -> peer that requested)
    pub requested_blocks: HashMap<u32, usize>,
}

impl PendingPiece {
    /// Create a new pending piece
    pub fn new(index: u32, piece_length: u64) -> Self {
        let block_size = BLOCK_SIZE as u64;
        let num_blocks = piece_length.div_ceil(block_size) as usize;

        let now = Instant::now();
        Self {
            index,
            length: piece_length,
            blocks: vec![None; num_blocks],
            block_size: BLOCK_SIZE,
            blocks_received: 0,
            started_at: now,
            last_activity: now,
            requested_blocks: HashMap::new(),
        }
    }

    /// Add a received block
    pub fn add_block(&mut self, offset: u32, data: Vec<u8>) -> bool {
        let block_index = (offset / self.block_size) as usize;

        if block_index >= self.blocks.len() {
            return false;
        }

        // Validate offset is aligned to block size
        if offset % self.block_size != 0 {
            tracing::warn!(
                "Block offset {} is not aligned to block size {}",
                offset,
                self.block_size
            );
            return false;
        }

        // Validate block size is correct
        let expected_size = if block_index == self.blocks.len() - 1 {
            // Last block may be smaller
            let remaining = self.length - offset as u64;
            remaining.min(self.block_size as u64) as usize
        } else {
            self.block_size as usize
        };

        if data.len() != expected_size {
            tracing::warn!(
                "Block {} has wrong size: expected {}, got {}",
                block_index,
                expected_size,
                data.len()
            );
            return false;
        }

        // Don't count duplicates
        if self.blocks[block_index].is_none() {
            self.blocks_received += 1;
        }

        self.blocks[block_index] = Some(data);
        self.requested_blocks.remove(&(block_index as u32));
        self.last_activity = Instant::now();

        true
    }

    /// Check if all blocks have been received
    pub fn is_complete(&self) -> bool {
        self.blocks_received == self.blocks.len()
    }

    /// Get the combined piece data
    pub fn data(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }

        let mut data = Vec::with_capacity(self.length as usize);
        for block in &self.blocks {
            if let Some(b) = block {
                data.extend_from_slice(b);
            } else {
                return None;
            }
        }

        // Trim to actual piece length (last piece may have smaller final block)
        data.truncate(self.length as usize);

        Some(data)
    }

    /// Whether the block starting at `offset` has been received
    pub fn has_block_at(&self, offset: u32) -> bool {
        let index = (offset / self.block_size) as usize;
        self.blocks.get(index).map(|b| b.is_some()).unwrap_or(false)
    }

    /// Get blocks that haven't been requested yet
    pub fn unrequested_blocks(&self) -> Vec<(u32, u32)> {
        let mut blocks = Vec::new();
        let num_blocks = self.blocks.len();

        for i in 0..num_blocks {
            if self.blocks[i].is_none() && !self.requested_blocks.contains_key(&(i as u32)) {
                let offset = i as u32 * self.block_size;
                let length = if i == num_blocks - 1 {
                    // Last block may be smaller
                    let remaining = self.length - offset as u64;
                    remaining.min(self.block_size as u64) as u32
                } else {
                    self.block_size
                };
                blocks.push((offset, length));
            }
        }

        blocks
    }

    /// Mark a block as requested
    pub fn mark_requested(&mut self, block_index: u32, peer_id: usize) {
        self.requested_blocks.insert(block_index, peer_id);
    }
}

/// Block request
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRequest {
    /// Piece index
    pub piece: u32,
    /// Block offset within piece
    pub offset: u32,
    /// Block length
    pub length: u32,
}

impl PieceManager {
    /// Create a new piece manager
    pub fn new(metainfo: Arc<Metainfo>, save_dir: PathBuf) -> Self {
        let num_pieces = metainfo.info.pieces.len();

        Self {
            metainfo,
            save_dir,
            have: RwLock::new(bitvec![u8, Msb0; 0; num_pieces]),
            pending: RwLock::new(HashMap::new()),
            verified_count: AtomicU64::new(0),
            verified_bytes: AtomicU64::new(0),
            piece_availability: RwLock::new(vec![0; num_pieces]),
            wanted_pieces: RwLock::new(None),
            sequential_mode: RwLock::new(false),
            stream_position: std::sync::atomic::AtomicU32::new(u32::MAX),
        }
    }

    /// Enable or disable sequential download mode
    pub fn set_sequential(&self, sequential: bool) {
        *self.sequential_mode.write() = sequential;
    }

    /// Bytes readable as one contiguous run starting at byte `offset`: the
    /// run extends to the end of the last consecutive verified piece.
    pub fn contiguous_available_from(&self, offset: u64) -> u64 {
        let piece_len = self.metainfo.info.piece_length;
        let total = self.metainfo.info.total_size;
        if offset >= total || piece_len == 0 {
            return 0;
        }
        let have = self.have.read();
        let first_piece = (offset / piece_len) as usize;
        let mut end = offset;
        for i in first_piece..self.num_pieces() {
            if have.get(i).map(|b| *b).unwrap_or(false) {
                end = ((i as u64 + 1) * piece_len).min(total);
            } else {
                break;
            }
        }
        end.saturating_sub(offset)
    }

    /// Total torrent size in bytes.
    pub fn total_size(&self) -> u64 {
        self.metainfo.info.total_size
    }

    /// Nominal piece length in bytes.
    pub fn nominal_piece_length(&self) -> u64 {
        self.metainfo.info.piece_length
    }

    /// Set (or clear, with None) the streaming read position. Pieces at or
    /// after this index are prioritized by `select_piece` so a streaming
    /// reader is served before the rest of the torrent.
    pub fn set_stream_position(&self, piece: Option<u32>) {
        self.stream_position
            .store(piece.unwrap_or(u32::MAX), Ordering::Relaxed);
    }

    /// Check if sequential mode is enabled
    pub fn is_sequential(&self) -> bool {
        *self.sequential_mode.read()
    }

    /// Preallocate files on disk according to the specified allocation mode.
    ///
    /// - `None`: No preallocation, files grow as data is written
    /// - `Sparse`: Set file size without writing (fast, works on most filesystems)
    /// - `Full`: Write zeros to allocate full file (slow but prevents fragmentation)
    pub async fn preallocate_files(&self, mode: crate::config::AllocationMode) -> Result<()> {
        use crate::config::AllocationMode;

        if matches!(mode, AllocationMode::None) {
            return Ok(());
        }

        for file_info in &self.metainfo.info.files {
            // Build file path with security validation
            let file_path = if self.metainfo.info.is_single_file {
                for component in std::path::Path::new(&self.metainfo.info.name).components() {
                    Self::validate_path_component(&component)?;
                }
                self.save_dir.join(&self.metainfo.info.name)
            } else {
                for component in std::path::Path::new(&self.metainfo.info.name).components() {
                    Self::validate_path_component(&component)?;
                }
                for component in std::path::Path::new(&file_info.path).components() {
                    Self::validate_path_component(&component)?;
                }
                self.save_dir
                    .join(&self.metainfo.info.name)
                    .join(&file_info.path)
            };

            // Create parent directories
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            // Open or create file
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file_path)
                .await?;

            let file_size = file_info.length;

            match mode {
                AllocationMode::None => unreachable!(),
                AllocationMode::Sparse => {
                    // Sparse allocation: just set the file length
                    // The filesystem will handle sparse regions
                    file.set_len(file_size).await?;
                    tracing::debug!(
                        "Sparse allocated {} ({} bytes)",
                        file_path.display(),
                        file_size
                    );
                }
                AllocationMode::Full => {
                    // Full allocation: write zeros to the entire file
                    // This is slow but prevents fragmentation
                    let current_size = file.metadata().await?.len();
                    if current_size < file_size {
                        // Only allocate what's needed
                        file.set_len(file_size).await?;

                        // On Linux, try to use fallocate for true allocation
                        #[cfg(target_os = "linux")]
                        {
                            use std::os::unix::fs::FileExt;
                            let std_file = file.try_into_std().unwrap();
                            // Write a single byte at the end to force allocation
                            // This is a fallback; true fallocate would be better
                            if file_size > 0 {
                                let _ = std_file.write_at(&[0], file_size - 1);
                            }
                        }

                        tracing::debug!(
                            "Full allocated {} ({} bytes)",
                            file_path.display(),
                            file_size
                        );
                    }
                }
            }
        }

        tracing::info!(
            "Preallocated {} files using {:?} mode",
            self.metainfo.info.files.len(),
            mode
        );

        Ok(())
    }

    /// Set selected files for partial download.
    ///
    /// Only pieces that contain data from the selected files will be downloaded.
    /// If `file_indices` is empty or None, all files will be downloaded.
    pub fn set_selected_files(&self, file_indices: Option<&[usize]>) {
        let indices = match file_indices {
            Some(indices) if !indices.is_empty() => indices,
            _ => {
                // Download all files
                *self.wanted_pieces.write() = None;
                return;
            }
        };

        let piece_length = self.metainfo.info.piece_length;
        let num_pieces = self.num_pieces();
        let mut wanted = bitvec![u8, Msb0; 0; num_pieces];

        // For each selected file, mark the pieces that contain its data
        for &file_idx in indices {
            if file_idx >= self.metainfo.info.files.len() {
                continue;
            }

            let file = &self.metainfo.info.files[file_idx];
            let file_start = file.offset;
            let file_end = file.offset + file.length;

            // Calculate piece range for this file
            let first_piece = (file_start / piece_length) as usize;
            let last_piece = if file_end == 0 {
                first_piece
            } else {
                ((file_end - 1) / piece_length) as usize
            };

            // Mark all pieces in range as wanted
            for piece_idx in first_piece..=last_piece.min(num_pieces - 1) {
                wanted.set(piece_idx, true);
            }
        }

        *self.wanted_pieces.write() = Some(wanted);
    }

    /// Check if a piece is wanted (needed for selected files)
    pub fn is_piece_wanted(&self, index: usize) -> bool {
        let wanted = self.wanted_pieces.read();
        match &*wanted {
            None => true, // All pieces wanted
            Some(bits) => bits.get(index).map(|b| *b).unwrap_or(false),
        }
    }

    /// Get the number of pieces
    pub fn num_pieces(&self) -> usize {
        self.metainfo.info.pieces.len()
    }

    /// Check if we have a piece
    pub fn have_piece(&self, index: usize) -> bool {
        self.have.read().get(index).map(|b| *b).unwrap_or(false)
    }

    /// Check if we need a piece
    pub fn need_piece(&self, index: u32) -> bool {
        let index = index as usize;
        if index >= self.num_pieces() {
            return false;
        }

        let have = self.have.read();
        let pending = self.pending.read();

        !have[index] && !pending.contains_key(&(index as u32))
    }

    /// Get our bitfield
    pub fn bitfield(&self) -> BitVec<u8, Msb0> {
        self.have.read().clone()
    }

    /// Get set of piece indices currently being downloaded
    pub fn pending_pieces(&self) -> HashSet<u32> {
        self.pending.read().keys().copied().collect()
    }

    /// Update availability for a single piece (from a Have message)
    pub fn update_piece_availability(&self, index: u32, add: bool) {
        let mut availability = self.piece_availability.write();
        if let Some(slot) = availability.get_mut(index as usize) {
            *slot = if add {
                slot.saturating_add(1)
            } else {
                slot.saturating_sub(1)
            };
        }
    }

    /// Whether a block has already been received (its piece is complete, or
    /// the block is present in the pending piece). Used to cancel duplicate
    /// in-flight requests in endgame mode.
    pub fn is_block_received(&self, piece: u32, offset: u32) -> bool {
        if self
            .have
            .read()
            .get(piece as usize)
            .map(|b| *b)
            .unwrap_or(false)
        {
            return true;
        }
        self.pending
            .read()
            .get(&piece)
            .map(|p| p.has_block_at(offset))
            .unwrap_or(false)
    }

    /// Update piece availability from a peer's bitfield
    pub fn update_availability(&self, peer_pieces: &BitVec<u8, Msb0>, add: bool) {
        let mut availability = self.piece_availability.write();

        for (i, has_piece) in peer_pieces.iter().enumerate() {
            if *has_piece {
                if add {
                    availability[i] = availability[i].saturating_add(1);
                } else {
                    availability[i] = availability[i].saturating_sub(1);
                }
            }
        }
    }

    /// Select the next piece to download using rarest-first or sequential strategy
    ///
    /// Returns the piece index if a suitable piece is found.
    /// In sequential mode, returns the lowest-numbered needed piece for streaming support.
    pub fn select_piece(&self, peer_has: &BitVec<u8, Msb0>) -> Option<u32> {
        let have = self.have.read();
        let pending = self.pending.read();
        let availability = self.piece_availability.read();
        let wanted = self.wanted_pieces.read();
        let sequential = *self.sequential_mode.read();

        // Find pieces we need that the peer has
        let mut candidates: Vec<(u32, u32)> = Vec::new();

        for i in 0..self.num_pieces() {
            // Skip pieces we have or are downloading
            if have[i] || pending.contains_key(&(i as u32)) {
                continue;
            }

            // Skip pieces not in selected files (if selective download is active)
            if let Some(ref wanted_bits) = *wanted {
                if !wanted_bits.get(i).map(|b| *b).unwrap_or(false) {
                    continue;
                }
            }

            // Check if peer has this piece
            if !peer_has.get(i).map(|b| *b).unwrap_or(false) {
                continue;
            }

            candidates.push((i as u32, availability[i]));
        }

        if candidates.is_empty() {
            return None;
        }

        // An active streaming reader overrides the normal strategy: serve
        // pieces in order from the read position first, so playback is never
        // starved; pieces before the read position come after, then by rarity.
        let stream_pos = self.stream_position.load(Ordering::Relaxed);
        if stream_pos != u32::MAX {
            candidates.sort_by_key(|&(index, count)| {
                if index >= stream_pos {
                    (0u8, index, count)
                } else {
                    (1u8, 0, count)
                }
            });
            return Some(candidates[0].0);
        }

        if sequential {
            // Sequential mode: select lowest-numbered piece for streaming
            candidates.sort_by_key(|&(index, _)| index);
        } else {
            // Normal mode: rarest first
            candidates.sort_by_key(|&(_, count)| count);
        }

        // Return the selected piece
        Some(candidates[0].0)
    }

    /// Start downloading a piece
    pub fn start_piece(&self, index: u32) -> Option<PendingPiece> {
        let piece_length = self.metainfo.piece_length(index as usize)?;

        let piece = PendingPiece::new(index, piece_length);

        let mut pending = self.pending.write();
        pending.insert(index, piece);

        pending.get(&index).cloned().map(|p| PendingPiece {
            index: p.index,
            length: p.length,
            blocks: vec![None; p.blocks.len()],
            block_size: p.block_size,
            blocks_received: 0,
            started_at: p.started_at,
            last_activity: p.last_activity,
            requested_blocks: HashMap::new(),
        })
    }

    /// Add a received block to a pending piece
    pub fn add_block(&self, index: u32, offset: u32, data: Vec<u8>) -> Result<bool> {
        let mut pending = self.pending.write();

        let piece = pending.get_mut(&index).ok_or_else(|| {
            EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                format!("Received block for unknown piece {}", index),
            )
        })?;

        if !piece.add_block(offset, data) {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                format!("Invalid block offset {} for piece {}", offset, index),
            ));
        }

        Ok(piece.is_complete())
    }

    /// Verify and save a completed piece
    pub async fn verify_and_save(&self, index: u32) -> Result<bool> {
        // Get piece data
        let data = {
            let pending = self.pending.read();
            let piece = match pending.get(&index) {
                Some(p) => p,
                None => {
                    // Piece was removed from pending (e.g., by cancel_stale_pieces).
                    // Late-arriving blocks are expected; silently discard.
                    tracing::trace!("Piece {} not in pending, discarding late block", index);
                    return Ok(false);
                }
            };

            piece.data().ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::PeerProtocol,
                    format!("Piece {} is incomplete", index),
                )
            })?
        };

        // Verify hash
        let expected_hash = self.metainfo.piece_hash(index as usize).ok_or_else(|| {
            EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                format!("No hash for piece {}", index),
            )
        })?;

        let mut hasher = Sha1::new();
        hasher.update(&data);
        let actual_hash: Sha1Hash = hasher.finalize().into();

        if actual_hash != *expected_hash {
            // Hash mismatch - remove from pending and return false
            self.pending.write().remove(&index);
            return Ok(false);
        }

        // Write to disk
        self.write_piece(index, &data).await?;

        // Update state
        {
            let mut have = self.have.write();
            have.set(index as usize, true);
        }

        // Only increment counters if we successfully removed the piece.
        // This prevents double-counting in endgame mode when multiple peers
        // send the same piece and both threads race through verify_and_save.
        if self.pending.write().remove(&index).is_some() {
            self.verified_count.fetch_add(1, Ordering::Relaxed);
            self.verified_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }

        Ok(true)
    }

    /// Validate a path component to prevent directory traversal attacks
    fn validate_path_component(component: &std::path::Component) -> Result<()> {
        use std::path::Component;
        match component {
            Component::ParentDir => Err(EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                "Invalid torrent: file path contains parent directory reference (..)",
            )),
            Component::RootDir | Component::Prefix(_) => Err(EngineError::protocol(
                ProtocolErrorKind::InvalidTorrent,
                "Invalid torrent: file path contains absolute path",
            )),
            _ => Ok(()),
        }
    }

    /// Write piece data to the appropriate files
    async fn write_piece(&self, index: u32, data: &[u8]) -> Result<()> {
        let files_for_piece = self.metainfo.files_for_piece(index as usize);

        let mut data_offset = 0usize;

        for (file_idx, file_offset, length) in files_for_piece {
            let file_info = &self.metainfo.info.files[file_idx];

            // Build full file path with security validation
            let file_path = if self.metainfo.info.is_single_file {
                // Validate single file name
                for component in std::path::Path::new(&self.metainfo.info.name).components() {
                    Self::validate_path_component(&component)?;
                }
                self.save_dir.join(&self.metainfo.info.name)
            } else {
                // Validate torrent name and file path components
                for component in std::path::Path::new(&self.metainfo.info.name).components() {
                    Self::validate_path_component(&component)?;
                }
                for component in std::path::Path::new(&file_info.path).components() {
                    Self::validate_path_component(&component)?;
                }
                self.save_dir
                    .join(&self.metainfo.info.name)
                    .join(&file_info.path)
            };

            // Create parent directories
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            // Open or create file (don't truncate - we write pieces at specific offsets)
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file_path)
                .await?;

            // Seek to the correct position
            file.seek(SeekFrom::Start(file_offset)).await?;

            // Write data
            let write_end = data_offset + length as usize;
            file.write_all(&data[data_offset..write_end]).await?;

            data_offset = write_end;
        }

        Ok(())
    }

    /// Cancel a pending piece (e.g., due to timeout)
    pub fn cancel_piece(&self, index: u32) {
        self.pending.write().remove(&index);
    }

    /// Cancel stale pending pieces that haven't received any blocks recently.
    ///
    /// This prevents pieces from getting stuck in the pending state forever
    /// when a peer disconnects mid-download.
    ///
    /// Returns the number of pieces cancelled.
    pub fn cancel_stale_pieces(&self, timeout: std::time::Duration) -> usize {
        let mut pending = self.pending.write();
        let now = Instant::now();

        let stale: Vec<u32> = pending
            .iter()
            .filter(|(_, piece)| {
                // A piece is stale if:
                // 1. No blocks have been received within the timeout period, AND
                // 2. It hasn't received all its blocks yet
                !piece.is_complete() && now.duration_since(piece.last_activity) > timeout
            })
            .map(|(&idx, _)| idx)
            .collect();

        let count = stale.len();
        for idx in stale {
            pending.remove(&idx);
        }

        if count > 0 {
            tracing::debug!(
                "Cancelled {} stale pending pieces (timeout {:?})",
                count,
                timeout
            );
        }

        count
    }

    /// Get blocks to request for a piece
    pub fn get_block_requests(&self, index: u32) -> Vec<BlockRequest> {
        let pending = self.pending.read();

        let Some(piece) = pending.get(&index) else {
            return Vec::new();
        };

        piece
            .unrequested_blocks()
            .into_iter()
            .map(|(offset, length)| BlockRequest {
                piece: index,
                offset,
                length,
            })
            .collect()
    }

    /// Mark a block as requested
    pub fn mark_block_requested(&self, piece: u32, block_index: u32, peer_id: usize) {
        let mut pending = self.pending.write();
        if let Some(p) = pending.get_mut(&piece) {
            p.mark_requested(block_index, peer_id);
        }
    }

    /// Get progress information
    pub fn progress(&self) -> PieceProgress {
        let have = self.have.read();
        let have_count = have.count_ones();

        PieceProgress {
            total_pieces: self.num_pieces(),
            have_pieces: have_count,
            pending_pieces: self.pending.read().len(),
            verified_bytes: self.verified_bytes.load(Ordering::Relaxed),
            total_size: self.metainfo.info.total_size,
        }
    }

    /// Check if download is complete
    ///
    /// If selective download is active, returns true when all wanted pieces are downloaded.
    pub fn is_complete(&self) -> bool {
        let have = self.have.read();
        let wanted = self.wanted_pieces.read();

        match &*wanted {
            None => {
                // All pieces wanted - check if we have all
                have.count_ones() == self.num_pieces()
            }
            Some(wanted_bits) => {
                // Only check wanted pieces
                for i in 0..self.num_pieces() {
                    if wanted_bits.get(i).map(|b| *b).unwrap_or(false)
                        && !have.get(i).map(|b| *b).unwrap_or(false)
                    {
                        return false;
                    }
                }
                true
            }
        }
    }

    /// Verify existing files and update bitfield
    ///
    /// Returns number of valid pieces found
    pub async fn verify_existing(&self) -> Result<usize> {
        let mut valid_count = 0;
        let mut verified_bytes = 0u64;

        for index in 0..self.num_pieces() {
            let valid = self.verify_piece_on_disk(index as u32).await?;
            {
                let mut have = self.have.write();
                // Also CLEAR the bit on failure: on a re-verify (repair
                // flow), a piece that has become corrupt on disk must go
                // back to "missing", not stay marked as held.
                have.set(index, valid);
            }
            if valid {
                valid_count += 1;
                verified_bytes += self.metainfo.piece_length(index).unwrap_or(0);
            }
        }

        self.verified_bytes.store(verified_bytes, Ordering::Relaxed);
        self.verified_count
            .store(valid_count as u64, Ordering::Relaxed);

        Ok(valid_count)
    }

    /// Verify a single piece from disk
    async fn verify_piece_on_disk(&self, index: u32) -> Result<bool> {
        let expected_hash = match self.metainfo.piece_hash(index as usize) {
            Some(h) => h,
            None => return Ok(false),
        };

        let piece_length = match self.metainfo.piece_length(index as usize) {
            Some(l) => l,
            None => return Ok(false),
        };

        let files_for_piece = self.metainfo.files_for_piece(index as usize);
        let mut piece_data = Vec::with_capacity(piece_length as usize);

        for (file_idx, file_offset, length) in files_for_piece {
            let file_info = &self.metainfo.info.files[file_idx];

            // Build and validate file path (security check)
            let file_path = if self.metainfo.info.is_single_file {
                for component in std::path::Path::new(&self.metainfo.info.name).components() {
                    Self::validate_path_component(&component)?;
                }
                self.save_dir.join(&self.metainfo.info.name)
            } else {
                for component in std::path::Path::new(&self.metainfo.info.name).components() {
                    Self::validate_path_component(&component)?;
                }
                for component in std::path::Path::new(&file_info.path).components() {
                    Self::validate_path_component(&component)?;
                }
                self.save_dir
                    .join(&self.metainfo.info.name)
                    .join(&file_info.path)
            };

            // Try to read from file
            let mut file = match File::open(&file_path).await {
                Ok(f) => f,
                Err(_) => return Ok(false),
            };

            file.seek(SeekFrom::Start(file_offset)).await?;

            let mut buf = vec![0u8; length as usize];
            match file.read_exact(&mut buf).await {
                Ok(_) => piece_data.extend_from_slice(&buf),
                Err(_) => return Ok(false),
            }
        }

        // Verify hash
        let mut hasher = Sha1::new();
        hasher.update(&piece_data);
        let actual_hash: Sha1Hash = hasher.finalize().into();

        Ok(actual_hash == *expected_hash)
    }

    /// Read a block from disk for uploading to peers
    ///
    /// Returns the block data if successful, or an error if:
    /// - We don't have the piece
    /// - The offset/length are invalid
    /// - File I/O fails
    pub async fn read_block(&self, piece_index: u32, offset: u32, length: u32) -> Result<Vec<u8>> {
        // Validate we have this piece
        if !self.have_piece(piece_index as usize) {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                format!("Don't have piece {} for upload", piece_index),
            ));
        }

        // Get piece length and validate bounds
        let piece_length = self
            .metainfo
            .piece_length(piece_index as usize)
            .ok_or_else(|| {
                EngineError::protocol(
                    ProtocolErrorKind::InvalidTorrent,
                    format!("Invalid piece index {}", piece_index),
                )
            })?;

        let block_end = offset as u64 + length as u64;
        if block_end > piece_length {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                format!(
                    "Block request out of bounds: offset={}, length={}, piece_length={}",
                    offset, length, piece_length
                ),
            ));
        }

        // Validate block size against BitTorrent protocol limits.
        // Standard block size is 16KB (BLOCK_SIZE = 16384 bytes).
        // We allow 1KB tolerance (1024 bytes) for two reasons:
        // 1. Some older clients may request slightly larger blocks
        // 2. The last block of a piece may have non-standard alignment
        // Requests larger than this are likely malicious or buggy and are rejected.
        if length > BLOCK_SIZE + 1024 {
            return Err(EngineError::protocol(
                ProtocolErrorKind::PeerProtocol,
                format!("Block request too large: {}", length),
            ));
        }

        // Read the full piece data from disk (same logic as verify_piece_on_disk)
        let files_for_piece = self.metainfo.files_for_piece(piece_index as usize);
        let mut piece_data = Vec::with_capacity(piece_length as usize);

        for (file_idx, file_offset, file_length) in files_for_piece {
            let file_info = &self.metainfo.info.files[file_idx];

            // Build and validate file path (security check)
            let file_path = if self.metainfo.info.is_single_file {
                for component in std::path::Path::new(&self.metainfo.info.name).components() {
                    Self::validate_path_component(&component)?;
                }
                self.save_dir.join(&self.metainfo.info.name)
            } else {
                for component in std::path::Path::new(&self.metainfo.info.name).components() {
                    Self::validate_path_component(&component)?;
                }
                for component in std::path::Path::new(&file_info.path).components() {
                    Self::validate_path_component(&component)?;
                }
                self.save_dir
                    .join(&self.metainfo.info.name)
                    .join(&file_info.path)
            };

            let mut file = File::open(&file_path).await.map_err(|e| {
                EngineError::storage(
                    StorageErrorKind::Io,
                    &file_path,
                    format!("Failed to open file for reading: {}", e),
                )
            })?;

            file.seek(SeekFrom::Start(file_offset)).await?;

            let mut buf = vec![0u8; file_length as usize];
            file.read_exact(&mut buf)
                .await
                .map_err(|e: std::io::Error| {
                    EngineError::storage(
                        StorageErrorKind::Io,
                        &file_path,
                        format!("Failed to read block data: {}", e),
                    )
                })?;
            piece_data.extend_from_slice(&buf);
        }

        // Extract just the requested block
        let block_start = offset as usize;
        let block_end = block_start + length as usize;

        Ok(piece_data[block_start..block_end].to_vec())
    }

    /// Write piece data received from a webseed (already verified)
    ///
    /// This method writes the piece to disk and updates the bitfield.
    /// The hash has already been verified by WebSeedManager.
    pub async fn write_piece_from_webseed(&self, index: u32, data: &[u8]) -> Result<()> {
        // Check if we already have this piece
        if self.have_piece(index as usize) {
            return Ok(()); // Already have it, skip
        }

        // Write to disk
        self.write_piece(index, data).await?;

        // Update state
        {
            let mut have = self.have.write();
            have.set(index as usize, true);
        }

        self.verified_count.fetch_add(1, Ordering::Relaxed);
        self.verified_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        tracing::debug!("Webseed piece {} written ({} bytes)", index, data.len());

        Ok(())
    }

    /// Get pieces for endgame mode (when only a few pieces remain).
    ///
    /// Endgame mode is a BitTorrent optimization where the remaining pieces are
    /// requested from multiple peers simultaneously to avoid the "last piece problem"
    /// where a slow peer holds up completion.
    ///
    /// Returns the list of remaining pieces if we're in endgame mode, otherwise empty.
    pub fn endgame_pieces(&self) -> Vec<u32> {
        let have = self.have.read();
        // Acquire pending lock to ensure consistent view with endgame_requests().
        // This prevents a race where we enter endgame mode but the pending state
        // changes between this call and the subsequent endgame_requests() call.
        let _pending = self.pending.read();

        let remaining: Vec<u32> = (0..self.num_pieces() as u32)
            .filter(|&i| !have[i as usize])
            .collect();

        // ENDGAME THRESHOLD: 10 pieces
        // This threshold is chosen as a balance between:
        // - Too low (e.g., 2-3): Miss optimization opportunities, slow final phase
        // - Too high (e.g., 50+): Excessive duplicate requests waste bandwidth
        // 10 pieces is a common choice in BitTorrent implementations (libtorrent, etc.)
        // and works well across torrent sizes. At typical piece sizes (256KB-4MB),
        // this represents 2.5MB-40MB of remaining data.
        if remaining.len() <= 10 {
            remaining
        } else {
            Vec::new()
        }
    }

    /// Get pending blocks that can be requested from multiple peers in endgame mode
    pub fn endgame_requests(&self) -> Vec<BlockRequest> {
        let pending = self.pending.read();
        let mut requests = Vec::new();

        for piece in pending.values() {
            for (i, block) in piece.blocks.iter().enumerate() {
                if block.is_none() {
                    let offset = i as u32 * piece.block_size;
                    let length = if i == piece.blocks.len() - 1 {
                        let remaining = piece.length - offset as u64;
                        remaining.min(piece.block_size as u64) as u32
                    } else {
                        piece.block_size
                    };

                    requests.push(BlockRequest {
                        piece: piece.index,
                        offset,
                        length,
                    });
                }
            }
        }

        requests
    }
}

// Manual Clone implementation for PendingPiece
impl Clone for PendingPiece {
    fn clone(&self) -> Self {
        Self {
            index: self.index,
            length: self.length,
            blocks: self.blocks.clone(),
            block_size: self.block_size,
            blocks_received: self.blocks_received,
            started_at: self.started_at,
            last_activity: self.last_activity,
            requested_blocks: self.requested_blocks.clone(),
        }
    }
}

/// Progress information
#[derive(Debug, Clone)]
pub struct PieceProgress {
    /// Total number of pieces
    pub total_pieces: usize,
    /// Number of pieces we have
    pub have_pieces: usize,
    /// Number of pieces being downloaded
    pub pending_pieces: usize,
    /// Total verified bytes
    pub verified_bytes: u64,
    /// Total size of all files
    pub total_size: u64,
}

impl PieceProgress {
    /// Calculate percentage complete
    pub fn percentage(&self) -> f64 {
        if self.total_pieces == 0 {
            return 0.0;
        }
        (self.have_pieces as f64 / self.total_pieces as f64) * 100.0
    }

    /// Calculate bytes remaining
    pub fn bytes_remaining(&self) -> u64 {
        self.total_size.saturating_sub(self.verified_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_piece() {
        let mut piece = PendingPiece::new(0, 32768); // 2 blocks of 16KB

        assert_eq!(piece.blocks.len(), 2);
        assert!(!piece.is_complete());

        // Add first block
        assert!(piece.add_block(0, vec![0; 16384]));
        assert!(!piece.is_complete());

        // Add second block
        assert!(piece.add_block(16384, vec![0; 16384]));
        assert!(piece.is_complete());

        // Get data
        let data = piece.data().unwrap();
        assert_eq!(data.len(), 32768);
    }

    #[test]
    fn test_unrequested_blocks() {
        let piece = PendingPiece::new(0, 32768);

        let blocks = piece.unrequested_blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], (0, 16384));
        assert_eq!(blocks[1], (16384, 16384));
    }

    #[test]
    fn test_block_request() {
        let req = BlockRequest {
            piece: 5,
            offset: 16384,
            length: 16384,
        };

        assert_eq!(req.piece, 5);
        assert_eq!(req.offset, 16384);
        assert_eq!(req.length, 16384);
    }

    #[test]
    fn test_piece_progress() {
        let progress = PieceProgress {
            total_pieces: 100,
            have_pieces: 50,
            pending_pieces: 5,
            verified_bytes: 50 * 32768,
            total_size: 100 * 32768,
        };

        assert_eq!(progress.percentage(), 50.0);
        assert_eq!(progress.bytes_remaining(), 50 * 32768);
    }

    #[test]
    fn test_last_block_size() {
        // Piece with non-standard size (e.g., last piece)
        let piece = PendingPiece::new(0, 20000);

        let blocks = piece.unrequested_blocks();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], (0, 16384));
        assert_eq!(blocks[1], (16384, 3616)); // 20000 - 16384 = 3616
    }

    // ========================================================================
    // Path Traversal Security Tests
    // ========================================================================

    #[test]
    fn test_validate_path_component_rejects_parent_dir() {
        use std::path::Component;

        let parent_dir = Component::ParentDir;
        let result = PieceManager::validate_path_component(&parent_dir);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("parent directory"));
    }

    #[test]
    fn test_validate_path_component_rejects_root_dir() {
        use std::path::Component;

        let root_dir = Component::RootDir;
        let result = PieceManager::validate_path_component(&root_dir);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("absolute path"));
    }

    #[test]
    fn test_validate_path_component_accepts_normal() {
        use std::ffi::OsStr;
        use std::path::Component;

        let normal = Component::Normal(OsStr::new("valid_filename.txt"));
        let result = PieceManager::validate_path_component(&normal);

        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_component_accepts_current_dir() {
        use std::path::Component;

        // CurDir (.) is harmless and should be allowed
        let cur_dir = Component::CurDir;
        let result = PieceManager::validate_path_component(&cur_dir);

        assert!(result.is_ok());
    }

    #[test]
    fn test_path_traversal_attack_patterns() {
        use std::path::Path;

        // These paths should all be rejected
        let malicious_paths = [
            "../etc/passwd",
            "foo/../../../etc/passwd",
            "/etc/passwd",
            "foo/bar/../../../etc/shadow",
        ];

        for path_str in malicious_paths {
            let path = Path::new(path_str);
            let mut has_invalid = false;

            for component in path.components() {
                if PieceManager::validate_path_component(&component).is_err() {
                    has_invalid = true;
                    break;
                }
            }

            assert!(
                has_invalid,
                "Path '{}' should be rejected but wasn't",
                path_str
            );
        }
    }

    #[test]
    fn test_safe_path_patterns() {
        use std::path::Path;

        // These paths should all be allowed
        let safe_paths = [
            "file.txt",
            "subdir/file.txt",
            "a/b/c/d/file.txt",
            "My Documents/file.pdf",
            "file with spaces.txt",
            "日本語ファイル.txt",
        ];

        for path_str in safe_paths {
            let path = Path::new(path_str);
            let mut all_valid = true;

            for component in path.components() {
                if PieceManager::validate_path_component(&component).is_err() {
                    all_valid = false;
                    break;
                }
            }

            assert!(
                all_valid,
                "Path '{}' should be allowed but was rejected",
                path_str
            );
        }
    }
}
