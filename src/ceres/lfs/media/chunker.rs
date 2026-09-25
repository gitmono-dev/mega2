//! Frozen `fastcdc-v2020-32k` chunker (shared mega2 ↔ Libra Media contract).
//!
//! Recipe: `fastcdc = "=3.2.1"`, `v2020`, `Normalization::Level1`, seed=`0`,
//! min/avg/max = 32768/65536/262144 bytes. Changing any of these requires a
//! new algorithm name and synchronized dual-repo revision.

use std::io::{self, Read};

use fastcdc::v2020::{ChunkData, FastCDC, Normalization, StreamCDC};

use crate::ceres::lfs::digest::LfsDigest;

/// Algorithm name recorded by Media manifests (FC-02 / C-01).
pub const ALGORITHM: &str = "fastcdc-v2020-32k";

/// Minimum chunk size (non-tail chunks must be ≥ this).
pub const MIN_SIZE: usize = 32 * 1024;
/// Target average chunk size.
pub const AVG_SIZE: usize = 64 * 1024;
/// Maximum chunk size (non-tail and tail upper bound).
pub const MAX_SIZE: usize = 256 * 1024;

const MIN_U32: u32 = MIN_SIZE as u32;
const AVG_U32: u32 = AVG_SIZE as u32;
const MAX_U32: u32 = MAX_SIZE as u32;
const SEED: u64 = 0;

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

/// Chunk a byte stream. Empty input yields no chunks; EOF always emits a
/// trailing chunk covering remaining bytes (may be shorter than [`MIN_SIZE`]).
pub fn chunk_reader<R: Read>(reader: R) -> io::Result<Vec<Chunk>> {
    let chunker = StreamCDC::with_level_and_seed(
        reader,
        MIN_U32,
        AVG_U32,
        MAX_U32,
        Normalization::Level1,
        SEED,
    );
    let mut out = Vec::new();
    let mut offset: u64 = 0;
    for item in chunker {
        let ChunkData { length, data, .. } = item.map_err(io::Error::other)?;
        out.push(Chunk {
            offset,
            length: length as u64,
            chunk_hash: sha256_hex(&data),
        });
        offset = offset
            .checked_add(length as u64)
            .ok_or_else(|| io::Error::other("chunk offset overflow"))?;
    }
    Ok(out)
}

/// In-memory convenience over [`chunk_reader`]; must agree with the streaming path.
pub fn chunk_bytes(data: &[u8]) -> Vec<Chunk> {
    let chunker =
        FastCDC::with_level_and_seed(data, MIN_U32, AVG_U32, MAX_U32, Normalization::Level1, SEED);
    chunker
        .map(|c| {
            let slice = &data[c.offset..c.offset + c.length];
            Chunk {
                offset: c.offset as u64,
                length: c.length as u64,
                chunk_hash: sha256_hex(slice),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct GoldenFile {
        algorithm: String,
        vectors: std::collections::BTreeMap<String, GoldenVector>,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenVector {
        input_kind: String,
        input_len: usize,
        input_sha256: String,
        chunks: Vec<GoldenChunk>,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct GoldenChunk {
        offset: u64,
        length: u64,
        chunk_hash: String,
    }

    /// Short-read reader: at most `n` bytes per `read` (C-01 short-read equivalence).
    struct ShortRead<'a> {
        inner: io::Cursor<&'a [u8]>,
        n: usize,
    }

    impl Read for ShortRead<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let cap = self.n.min(buf.len());
            self.inner.read(&mut buf[..cap])
        }
    }

    fn fixture_bytes(kind: &str, len: usize) -> Vec<u8> {
        match kind {
            "empty" => Vec::new(),
            "zeros" => vec![0u8; len],
            "repeat_a" => vec![b'A'; len],
            "fixed_seq" => {
                let mut out = Vec::with_capacity(len);
                let mut counter: u64 = 0;
                while out.len() < len {
                    let mut h = ring::digest::Context::new(&ring::digest::SHA256);
                    h.update(b"libra-fastcdc-fixture");
                    h.update(&counter.to_le_bytes());
                    let raw = h.finish();
                    let take = (len - out.len()).min(raw.as_ref().len());
                    out.extend_from_slice(&raw.as_ref()[..take]);
                    counter += 1;
                }
                out
            }
            other => panic!("unknown golden input_kind: {other}"),
        }
    }

    fn load_golden() -> GoldenFile {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fastcdc/golden.json");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        serde_json::from_str(&text).expect("parse shared golden.json")
    }

    #[test]
    fn algorithm_constants_match_shared_recipe() {
        assert_eq!(ALGORITHM, "fastcdc-v2020-32k");
        assert_eq!(MIN_SIZE, 32 * 1024);
        assert_eq!(AVG_SIZE, 64 * 1024);
        assert_eq!(MAX_SIZE, 256 * 1024);
    }

    #[test]
    fn empty_input_yields_zero_chunks() {
        assert!(chunk_bytes(&[]).is_empty());
        assert!(chunk_reader(io::Cursor::new(&[][..])).unwrap().is_empty());
    }

    #[test]
    fn shared_golden_slice_stream_and_short_read_agree() {
        let golden = load_golden();
        assert_eq!(golden.algorithm, ALGORITHM);
        assert!(!golden.vectors.is_empty());
        for (name, vec) in &golden.vectors {
            let data = fixture_bytes(&vec.input_kind, vec.input_len);
            assert_eq!(data.len(), vec.input_len, "{name} length");
            assert_eq!(sha256_hex(&data), vec.input_sha256, "{name} input hash");

            let sliced = chunk_bytes(&data);
            let expected: Vec<Chunk> = vec
                .chunks
                .iter()
                .map(|c| Chunk {
                    offset: c.offset,
                    length: c.length,
                    chunk_hash: c.chunk_hash.clone(),
                })
                .collect();
            assert_eq!(sliced, expected, "{name} slice vs golden");

            let streamed = chunk_reader(io::Cursor::new(&data[..])).unwrap();
            assert_eq!(streamed, expected, "{name} stream vs golden");

            if !data.is_empty() {
                let short = chunk_reader(ShortRead {
                    inner: io::Cursor::new(&data[..]),
                    n: 7,
                })
                .unwrap();
                assert_eq!(short, expected, "{name} short-read vs golden");
            }

            // Contiguity + bounds (C-01 / block limits).
            let mut off = 0u64;
            for (i, c) in sliced.iter().enumerate() {
                assert_eq!(c.offset, off, "{name}[{i}] offset");
                assert!(
                    c.length >= 1 && c.length <= MAX_SIZE as u64,
                    "{name}[{i}] len"
                );
                if i + 1 < sliced.len() {
                    assert!(
                        c.length >= MIN_SIZE as u64,
                        "{name}[{i}] non-tail below MIN"
                    );
                }
                let start = c.offset as usize;
                let end = start + c.length as usize;
                assert_eq!(c.chunk_hash, sha256_hex(&data[start..end]));
                off += c.length;
            }
            assert_eq!(off as usize, data.len(), "{name} coverage");
        }
    }
}
