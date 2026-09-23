//! WeChat Channels' ISAAC64 prefix transform. Only the first 128 KiB of a
//! media file is encrypted. This Rust implementation follows the publicly
//! documented ISAAC64 variant used by WeChat Channels; it runs entirely local.
use anyhow::{ensure, Context, Result};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

const PREFIX_LEN: usize = 128 * 1024;
const GOLDEN: u64 = 0x9e3779b97f4a7c13;

struct Isaac64 {
    count: usize,
    results: [u64; 256],
    memory: [u64; 256],
    aa: u64,
    bb: u64,
    cc: u64,
}

impl Isaac64 {
    fn new(key: u64) -> Self {
        let mut state = Self {
            count: 255,
            results: [0; 256],
            memory: [0; 256],
            aa: 0,
            bb: 0,
            cc: 0,
        };
        state.results[0] = key;
        let mut values = [GOLDEN; 8];
        for _ in 0..4 {
            mix(&mut values);
        }
        for i in (0..256).step_by(8) {
            for (offset, value) in values.iter_mut().enumerate() {
                *value = value.wrapping_add(state.results[i + offset]);
            }
            mix(&mut values);
            state.memory[i..i + 8].copy_from_slice(&values);
        }
        for i in (0..256).step_by(8) {
            for (offset, value) in values.iter_mut().enumerate() {
                *value = value.wrapping_add(state.memory[i + offset]);
            }
            mix(&mut values);
            state.memory[i..i + 8].copy_from_slice(&values);
        }
        state.refill();
        state
    }

    fn next(&mut self) -> u64 {
        let value = self.results[self.count];
        if self.count == 0 {
            self.refill();
            self.count = 255;
        } else {
            self.count -= 1;
        }
        value
    }

    fn refill(&mut self) {
        self.cc = self.cc.wrapping_add(1);
        self.bb = self.bb.wrapping_add(self.cc);
        for i in 0..256 {
            self.aa = match i % 4 {
                0 => !(self.aa ^ self.aa.wrapping_shl(21)),
                1 => self.aa ^ (self.aa >> 5),
                2 => self.aa ^ self.aa.wrapping_shl(12),
                _ => self.aa ^ (self.aa >> 33),
            };
            self.aa = self.aa.wrapping_add(self.memory[(i + 128) % 256]);
            let x = self.memory[i];
            let y = self.memory[((x >> 3) & 255) as usize]
                .wrapping_add(self.aa)
                .wrapping_add(self.bb);
            self.memory[i] = y;
            self.bb = self.memory[((y >> 11) & 255) as usize].wrapping_add(x);
            self.results[i] = self.bb;
        }
    }
}

fn mix(v: &mut [u64; 8]) {
    v[0] = v[0].wrapping_sub(v[4]);
    v[5] ^= v[7] >> 9;
    v[7] = v[7].wrapping_add(v[0]);
    v[1] = v[1].wrapping_sub(v[5]);
    v[6] ^= v[0].wrapping_shl(9);
    v[0] = v[0].wrapping_add(v[1]);
    v[2] = v[2].wrapping_sub(v[6]);
    v[7] ^= v[1] >> 23;
    v[1] = v[1].wrapping_add(v[2]);
    v[3] = v[3].wrapping_sub(v[7]);
    v[0] ^= v[2].wrapping_shl(15);
    v[2] = v[2].wrapping_add(v[3]);
    v[4] = v[4].wrapping_sub(v[0]);
    v[1] ^= v[3] >> 14;
    v[3] = v[3].wrapping_add(v[4]);
    v[5] = v[5].wrapping_sub(v[1]);
    v[2] ^= v[4].wrapping_shl(20);
    v[4] = v[4].wrapping_add(v[5]);
    v[6] = v[6].wrapping_sub(v[2]);
    v[3] ^= v[5] >> 17;
    v[5] = v[5].wrapping_add(v[6]);
    v[7] = v[7].wrapping_sub(v[3]);
    v[4] ^= v[6].wrapping_shl(14);
    v[6] = v[6].wrapping_add(v[7]);
}

fn xor_prefix(bytes: &mut [u8], key: u64) {
    let mut random = Isaac64::new(key);
    for chunk in bytes.chunks_mut(8) {
        let word = random.next().to_be_bytes();
        for (byte, mask) in chunk.iter_mut().zip(word) {
            *byte ^= mask;
        }
    }
}

pub fn decrypt_copy(source: &Path, to: &mut File, key: u64) -> Result<()> {
    let mut from = File::open(source)?;
    to.set_len(0)?;
    to.seek(SeekFrom::Start(0))?;
    std::io::copy(&mut from, &mut *to).context("复制视频号媒体失败")?;
    let length = to.metadata()?.len().min(PREFIX_LEN as u64) as usize;
    ensure!(length >= 12, "视频号媒体文件过短");
    let mut prefix = vec![0; length];
    to.seek(SeekFrom::Start(0))?;
    to.read_exact(&mut prefix)?;
    xor_prefix(&mut prefix, key);
    ensure!(
        &prefix[4..8] == b"ftyp",
        "视频号解密后不是 MP4；播放令牌或解密密钥可能已失效"
    );
    to.seek(SeekFrom::Start(0))?;
    to.write_all(&prefix)?;
    to.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isaac64_prefix_round_trip_and_tail_unchanged() {
        let mut bytes = vec![0x55; PREFIX_LEN + 99];
        bytes[..12].copy_from_slice(b"\0\0\0\x18ftypmp42");
        let original = bytes.clone();
        xor_prefix(&mut bytes[..PREFIX_LEN], 2136343393);
        assert_ne!(&bytes[..16], &original[..16]);
        assert_eq!(&bytes[PREFIX_LEN..], &original[PREFIX_LEN..]);
        xor_prefix(&mut bytes[..PREFIX_LEN], 2136343393);
        assert_eq!(bytes, original);
    }

    #[test]
    fn decrypt_copy_keeps_raw_checkpoint_and_rejects_wrong_key() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("encrypted.bin");
        let mut original = vec![0x33; PREFIX_LEN + 17];
        original[..12].copy_from_slice(b"\0\0\0\x18ftypmp42");
        let mut encrypted = original.clone();
        xor_prefix(&mut encrypted[..PREFIX_LEN], 2136343393);
        std::fs::write(&source, &encrypted).unwrap();
        let mut good = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        decrypt_copy(&source, good.as_file_mut(), 2136343393).unwrap();
        assert_eq!(std::fs::read(good.path()).unwrap(), original);
        assert_eq!(std::fs::read(&source).unwrap(), encrypted);
        let mut wrong = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        assert!(decrypt_copy(&source, wrong.as_file_mut(), 1).is_err());
    }
}
