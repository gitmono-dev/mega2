//! Wire-compatible deterministic `fastcdc-v1` chunker.
//!
//! The Gear table, masks and size limits are frozen protocol parameters. Any
//! change to them changes chunk boundaries and therefore requires a new
//! algorithm identifier instead of an in-place edit.

use std::io::{self, Read};

use sha2::{Digest, Sha256};

/// The frozen chunker algorithm identifier recorded in Media manifests.
pub const ALGORITHM: &str = "fastcdc-v1";

/// Minimum chunk size: no boundary may fire before this many bytes (512 KiB).
pub const MIN_SIZE: usize = 512 * 1024;
/// Target average chunk size (2 MiB).
pub const AVG_SIZE: usize = 2 * 1024 * 1024;
/// Maximum chunk size: a boundary is forced at this limit (8 MiB).
pub const MAX_SIZE: usize = 8 * 1024 * 1024;

// Normalized FastCDC masks. These are protocol constants, not tuning knobs.
const MASK_STRICT: u64 = ((1u64 << 23) - 1) << 41;
const MASK_LOOSE: u64 = ((1u64 << 19) - 1) << 45;

/// Deterministic 256-entry Gear table, built from the frozen splitmix64 seed.
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

/// One content-defined chunk and its raw-byte SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    pub length: u64,
    pub chunk_hash: String,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Chunks a reader without buffering more than one maximum-size chunk.
pub fn chunk_reader<R: Read>(mut reader: R) -> io::Result<Vec<Chunk>> {
    let mut chunks = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut current = Vec::with_capacity(MAX_SIZE.min(1 << 20));
    let mut offset = 0u64;
    let mut fingerprint = 0u64;

    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }

        for &byte in &buffer[..count] {
            current.push(byte);
            fingerprint = (fingerprint << 1).wrapping_add(GEAR[byte as usize]);
            let length = current.len();
            let cut = if length < MIN_SIZE {
                false
            } else if length < AVG_SIZE {
                (fingerprint & MASK_STRICT) == 0
            } else if length < MAX_SIZE {
                (fingerprint & MASK_LOOSE) == 0
            } else {
                true
            };

            if cut {
                chunks.push(Chunk {
                    offset,
                    length: length as u64,
                    chunk_hash: sha256_hex(&current),
                });
                offset += length as u64;
                current.clear();
                fingerprint = 0;
            }
        }
    }

    if !current.is_empty() {
        chunks.push(Chunk {
            offset,
            length: current.len() as u64,
            chunk_hash: sha256_hex(&current),
        });
    }

    Ok(chunks)
}

/// In-memory convenience over [`chunk_reader`].
pub fn chunk_bytes(data: &[u8]) -> Vec<Chunk> {
    // INVARIANT: a cursor over an in-memory slice cannot report an I/O error.
    chunk_reader(io::Cursor::new(data)).expect("in-memory chunking cannot fail")
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    fn deterministic_bytes(length: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(length);
        let mut state = 0x1234_5678_9ABC_DEF0u64;
        while data.len() < length {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            data.push((state >> 33) as u8);
        }
        data
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
    fn pinned_wire_vector_is_deterministic_and_contiguous() {
        let data = deterministic_bytes(MAX_SIZE * 3);

        let first = chunk_bytes(&data);
        let second = chunk_bytes(&data);
        assert_eq!(first, second);
        assert!(first.len() > 1);

        let expected = [
            (
                0,
                2_424_659,
                "684da96a7c5b19f707b0702b8171950da7ff94cdb0e9b9a9606dd95f1de74ca6",
            ),
            (
                2_424_659,
                2_326_734,
                "30d4cdf448aa9178b759def17520ed5b22dacfa48750222be5122c5e1549b72e",
            ),
            (
                4_751_393,
                3_170_146,
                "f2b2c49d28ac5ae075f32f75e5c6e98e65422d541754f4ce230001755615e48c",
            ),
            (
                7_921_539,
                2_944_350,
                "c4c5ac35a7f06824414150941e8958baa45ecba587eec60abb95a01b7a055477",
            ),
            (
                10_865_889,
                2_414_768,
                "a45ae3978b6092eb3b50aae89d5f331b5422cba5843e60a25a9a37e292348939",
            ),
            (
                13_280_657,
                2_633_856,
                "fea464b72050099f1df17e8dfb60663edbc3d5b58fae0d6115dec091867fb30d",
            ),
            (
                15_914_513,
                1_407_290,
                "e8c52610c442cb86a975b6ffe25c2454b2d02a5f34c78233785464ab3c41583e",
            ),
            (
                17_321_803,
                2_762_440,
                "fb3ba34f62727f6e344e6ed4d1ace7a136c827642fc129151e1806d0525b3e67",
            ),
            (
                20_084_243,
                2_143_265,
                "635d62369888af39b3d761097fa99ca20c4571e25e8b65e519011acb49df63a9",
            ),
            (
                22_227_508,
                2_335_739,
                "c7b6f394a52f54127b2739cb6d790ca35d14bdec4b77e19ac3285887cc4f1138",
            ),
            (
                24_563_247,
                602_577,
                "7ca4dd84c27ec9fd24bf13b97838436906904e596e767ef58d103065a7800ca0",
            ),
        ];
        assert_eq!(first.len(), expected.len());

        let mut expected_offset = 0u64;
        for (chunk, (offset, length, hash)) in first.iter().zip(expected) {
            assert_eq!(chunk.offset, offset);
            assert_eq!(chunk.length, length);
            assert_eq!(chunk.chunk_hash.as_str(), hash);
            assert_eq!(chunk.offset, expected_offset);
            assert!((1..=MAX_SIZE as u64).contains(&chunk.length));
            assert_eq!(
                chunk.chunk_hash,
                sha256_hex(&data[chunk.offset as usize..(chunk.offset + chunk.length) as usize])
            );
            expected_offset += chunk.length;
        }
        assert_eq!(expected_offset as usize, data.len());
    }

    #[test]
    fn size_boundary_inputs_are_covered_without_oversize_chunks() {
        for size in [MIN_SIZE, AVG_SIZE, MAX_SIZE, MAX_SIZE + 1] {
            let data = deterministic_bytes(size);
            let chunks = chunk_bytes(&data);

            let mut expected_offset = 0u64;
            assert!(chunks.iter().all(|chunk| {
                let is_lowercase_hex = chunk
                    .chunk_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
                let contiguous = chunk.offset == expected_offset;
                expected_offset += chunk.length;
                (1..=MAX_SIZE as u64).contains(&chunk.length)
                    && chunk.chunk_hash.len() == 64
                    && is_lowercase_hex
                    && contiguous
            }));
            assert_eq!(expected_offset, size as u64);
            if size > MAX_SIZE {
                assert!(chunks.len() > 1);
            }
        }
    }

    #[test]
    fn streaming_and_memory_agree() {
        let data = vec![0xAB; MAX_SIZE + 123];
        let streamed = chunk_reader(io::Cursor::new(&data)).unwrap();

        assert_eq!(streamed, chunk_bytes(&data));
    }

    #[test]
    fn reader_errors_are_returned() {
        struct BrokenReader;

        impl Read for BrokenReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected reader failure"))
            }
        }

        assert!(chunk_reader(BrokenReader).is_err());
    }
}
