//! Frozen `fastcdc-v1` chunker (Mega `bb3ef17` / Libra wire contract).
//!
//! Gear table, masks, and min/avg/max are protocol constants. Changing them
//! requires a new algorithm name; v1 must stay byte-identical.

use std::io::{self, Read};

use crate::ceres::lfs::digest::LfsDigest;

/// Algorithm name recorded by later Media manifests (FC-02).
pub const ALGORITHM: &str = "fastcdc-v1";

/// Minimum chunk size: no content boundary may fire before this many bytes.
pub const MIN_SIZE: usize = 512 * 1024;
/// Target average chunk size (`log2(AVG_SIZE) == 21` sets mask width).
pub const AVG_SIZE: usize = 2 * 1024 * 1024;
/// Maximum chunk size: a boundary is forced here (8 MiB).
pub const MAX_SIZE: usize = 8 * 1024 * 1024;

const MASK_STRICT: u64 = ((1u64 << 23) - 1) << 41;
const MASK_LOOSE: u64 = ((1u64 << 19) - 1) << 45;

const GEAR: [u64; 256] = build_gear();

const fn build_gear() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        table[i] = z;
        i += 1;
    }
    table
}

fn sha256_hex(bytes: &[u8]) -> String {
    LfsDigest::sha256_of(bytes).hex().to_owned()
}

/// One content-defined chunk: byte range in the media object and lowercase-hex
/// SHA-256 of its raw (uncompressed) bytes. LFS digest domain, not Git HashKind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    pub length: u64,
    pub chunk_hash: String,
}

/// Chunk a byte stream. Empty input yields no chunks; input shorter than
/// [`MIN_SIZE`] yields exactly one chunk covering the whole input.
pub fn chunk_reader<R: Read>(mut reader: R) -> io::Result<Vec<Chunk>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    let mut cur: Vec<u8> = Vec::with_capacity(MAX_SIZE.min(1 << 20));
    let mut offset: u64 = 0;
    let mut fingerprint: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            cur.push(byte);
            fingerprint = (fingerprint << 1).wrapping_add(GEAR[byte as usize]);
            let len = cur.len();
            let cut = if len < MIN_SIZE {
                false
            } else if len < AVG_SIZE {
                (fingerprint & MASK_STRICT) == 0
            } else if len < MAX_SIZE {
                (fingerprint & MASK_LOOSE) == 0
            } else {
                true
            };
            if cut {
                out.push(Chunk {
                    offset,
                    length: len as u64,
                    chunk_hash: sha256_hex(&cur),
                });
                offset += len as u64;
                cur.clear();
                fingerprint = 0;
            }
        }
    }
    if !cur.is_empty() {
        out.push(Chunk {
            offset,
            length: cur.len() as u64,
            chunk_hash: sha256_hex(&cur),
        });
    }
    Ok(out)
}

/// In-memory convenience over [`chunk_reader`].
pub fn chunk_bytes(data: &[u8]) -> Vec<Chunk> {
    // INVARIANT: `io::Cursor<&[u8]>` never returns an I/O error.
    chunk_reader(io::Cursor::new(data)).expect("in-memory chunking cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg_bytes(len: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(len);
        let mut x: u64 = 0x1234_5678_9ABC_DEF0;
        while data.len() < len {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.push((x >> 33) as u8);
        }
        data
    }

    fn assert_contiguous_cover(data: &[u8], chunks: &[Chunk]) {
        let mut expected_offset = 0u64;
        for c in chunks {
            assert_eq!(c.offset, expected_offset);
            assert!(c.length >= 1 && c.length <= MAX_SIZE as u64);
            let start = c.offset as usize;
            let end = start + c.length as usize;
            assert_eq!(c.chunk_hash, sha256_hex(&data[start..end]));
            expected_offset += c.length;
        }
        assert_eq!(expected_offset as usize, data.len());
    }

    #[test]
    fn empty_input_yields_zero_chunks() {
        assert!(chunk_bytes(&[]).is_empty());
    }

    #[test]
    fn short_input_is_a_single_chunk() {
        let data = vec![7u8; MIN_SIZE - 1];
        let chunks = chunk_bytes(&data);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[0].length, (MIN_SIZE - 1) as u64);
        assert_eq!(chunks[0].chunk_hash, sha256_hex(&data));
    }

    #[test]
    fn min_size_is_the_first_possible_boundary() {
        let data = vec![0x11u8; MIN_SIZE];
        let chunks = chunk_bytes(&data);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].length, MIN_SIZE as u64);
    }

    #[test]
    fn repeated_identical_inputs_match() {
        let data = vec![0x5Au8; MIN_SIZE + 4096];
        assert_eq!(chunk_bytes(&data), chunk_bytes(&data));
    }

    #[test]
    fn greater_than_max_splits_and_never_exceeds_max() {
        let data = lcg_bytes(MAX_SIZE * 3);
        let chunks = chunk_bytes(&data);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.length <= MAX_SIZE as u64));
        assert_contiguous_cover(&data, &chunks);
    }

    #[test]
    fn streaming_and_memory_agree() {
        let data = vec![0xABu8; MAX_SIZE + 123];
        let streamed = chunk_reader(io::Cursor::new(&data[..])).unwrap();
        assert_eq!(streamed, chunk_bytes(&data));
        assert_contiguous_cover(&data, &streamed);
    }

    #[test]
    fn algorithm_constants_match_fastcdc_v1() {
        assert_eq!(ALGORITHM, "fastcdc-v1");
        assert_eq!(MIN_SIZE, 512 * 1024);
        assert_eq!(AVG_SIZE, 2 * 1024 * 1024);
        assert_eq!(MAX_SIZE, 8 * 1024 * 1024);
    }
}
