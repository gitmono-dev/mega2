//! On-the-fly, range-readable chunk projection for one file (spec 07).
//!
//! Persistent segment/locator storage is T04/T12 work; this identity slice
//! builds the MCM2 map and MCL2 leaves from a fixed view's verified blob and
//! stages the raw bytes in a bounded in-process cache, so a CHUNK request
//! slices its range out of an already-verified representation rather than
//! reconstructing the whole Git object per chunk (spec 07 §8).
//!
//! The cache is an optimization, never the authority: every projected file
//! is re-hashed against the `content_id` the fixed view advertised, and a
//! cache miss simply rebuilds from Git. Entries are addressed by content
//! digest, never by request path.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Mutex, OnceLock},
};

use mst2_codec::chunkmap::{CHUNK_SIZE, CHUNKS_PER_PAGE, ChunkLeaf, ChunkMap};
use sha2::{Digest, Sha256};

use crate::ceres::snapshot::error::{SnapshotError, SnapshotErrorCode};

/// One file's verified range-readable projection.
pub struct ChunkProjection {
    pub map: ChunkMap,
    pub map_id: [u8; 32],
    leaves: Vec<ChunkLeaf>,
    leaf_hashes: Vec<[u8; 32]>,
    /// Full verified content. Indexed by fixed 1 MiB chunk boundaries.
    raw: Vec<u8>,
}

impl ChunkProjection {
    /// Build from bytes that the caller already resolved at a fixed path.
    /// `content_id` is the digest the fixed view advertises.
    pub fn build(content_id: [u8; 32], raw: Vec<u8>) -> Result<Self, SnapshotError> {
        let mut hasher = Sha256::new();
        hasher.update(&raw);
        let got: [u8; 32] = hasher.finalize().into();
        if got != content_id {
            return Err(SnapshotError::new(
                SnapshotErrorCode::DigestMismatch,
                "chunk projection input does not hash to content_id",
            ));
        }
        if raw.is_empty() {
            return Err(SnapshotError::new(
                SnapshotErrorCode::ScopeInvalid,
                "empty files have no chunk map (spec 07 §3)",
            ));
        }

        // Chunk digests over raw slices at the fixed boundary.
        let chunk_count = raw.len().div_ceil(CHUNK_SIZE as usize);
        let mut chunk_digests: Vec<[u8; 32]> = Vec::with_capacity(chunk_count);
        for (i, start) in (0..raw.len()).step_by(CHUNK_SIZE as usize).enumerate() {
            let end = (start + CHUNK_SIZE as usize).min(raw.len());
            // Every non-final chunk is exactly 1 MiB; the final chunk is a
            // positive remainder — and is itself 1 MiB for exact multiples
            // (BODY-09), so "final" does not imply "shorter".
            let len = end - start;
            assert!(len >= 1 && len <= CHUNK_SIZE as usize);
            if i < chunk_count - 1 {
                assert_eq!(len, CHUNK_SIZE as usize);
            }
            let mut h = Sha256::new();
            h.update(&raw[start..end]);
            chunk_digests.push(h.finalize().into());
        }

        // One MCL2 leaf per 256 chunks (spec 07 §4: full pages hold 256,
        // only the last page may be shorter, but it is never empty).
        let page_count = chunk_count.div_ceil(CHUNKS_PER_PAGE);
        let mut leaves = Vec::with_capacity(page_count);
        for page_index in 0..page_count {
            let lo = page_index * CHUNKS_PER_PAGE;
            let hi = (lo + CHUNKS_PER_PAGE).min(chunk_count);
            leaves.push(ChunkLeaf {
                page_index: page_index as u64,
                chunk_sha256: chunk_digests[lo..hi].to_vec(),
            });
        }
        let leaf_hashes: Vec<[u8; 32]> = leaves
            .iter()
            .map(|l| l.leaf_hash())
            .collect::<Result<_, _>>()
            .map_err(codec_err)?;
        let pages_root = mst2_codec::chunkmap::merkle_root(&leaf_hashes).map_err(codec_err)?;
        let map = ChunkMap::new(content_id, raw.len() as u64, pages_root).map_err(codec_err)?;
        let map_id = map.map_id();
        Ok(ChunkProjection {
            map,
            map_id,
            leaves,
            leaf_hashes,
            raw,
        })
    }

    pub fn page_count(&self) -> u64 {
        self.map.page_count
    }

    /// One MCL2 leaf plus its bottom-up proof toward `pages_root`.
    pub fn leaf_and_proof(
        &self,
        page_index: u64,
    ) -> Result<(&ChunkLeaf, Vec<mst2_codec::chunkmap::ProofStep>), SnapshotError> {
        if page_index >= self.map.page_count {
            return Err(SnapshotError::new(
                SnapshotErrorCode::PathNotFound,
                format!(
                    "page {page_index} does not exist; page_count={}",
                    self.map.page_count
                ),
            ));
        }
        let idx = page_index as usize;
        let proof =
            mst2_codec::chunkmap::leaf_proof(&self.leaf_hashes, page_index).map_err(codec_err)?;
        Ok((&self.leaves[idx], proof))
    }

    /// Raw bytes of chunk `index`, length-checked against the map (spec 07
    /// §4: full 1 MiB chunks except the positive-length remainder).
    pub fn chunk_bytes(&self, index: u64) -> Result<&[u8], SnapshotError> {
        let want_len = self.map.chunk_len(index).map_err(codec_err)?;
        let start = (index as usize)
            .checked_mul(CHUNK_SIZE as usize)
            .ok_or_else(|| internal("chunk offset overflow"))?;
        let end = start
            .checked_add(want_len as usize)
            .ok_or_else(|| internal("chunk end overflow"))?;
        let bytes = self.raw.get(start..end).ok_or_else(|| {
            SnapshotError::new(
                SnapshotErrorCode::Internal,
                format!("chunk {index} range {start}..{end} outside staged bytes"),
            )
        })?;
        // Independent re-hash: staging cache corruption must not be served.
        let page = (index / CHUNKS_PER_PAGE as u64) as usize;
        let slot = (index % CHUNKS_PER_PAGE as u64) as usize;
        let mut h = Sha256::new();
        h.update(bytes);
        let got: [u8; 32] = h.finalize().into();
        if got != self.leaves[page].chunk_sha256[slot] {
            return Err(SnapshotError::new(
                SnapshotErrorCode::DigestMismatch,
                format!("chunk {index} failed re-verification before serving"),
            ));
        }
        if bytes.len() as u64 != want_len {
            return Err(internal("chunk length disagrees with the map"));
        }
        Ok(bytes)
    }
}

fn codec_err(e: mst2_codec::CodecError) -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::Internal,
        format!("chunk codec error: {e}"),
    )
}
fn internal(m: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::Internal, m)
}

/// Bounded process-wide cache of verified projections. Eviction is pure
/// memory reclaim; the projection is reproducible from Git, so an evicted
/// entry is rebuilt, never an error to the client.
static STAGED: OnceLock<Mutex<ProjectionCache>> = OnceLock::new();

/// 512 MiB cap for staged file bytes in this identity slice. The real
/// persistent RAW_OBJECT locator layout replaces this (spec 10 §4).
const STAGED_CAP_BYTES: usize = 512 * 1024 * 1024;

struct ProjectionCache {
    entries: HashMap<[u8; 32], std::sync::Arc<ChunkProjection>>,
    /// Insertion/last-used order for FIFO reclaim.
    order: VecDeque<[u8; 32]>,
    total_bytes: usize,
}

impl ProjectionCache {
    fn new() -> Self {
        ProjectionCache {
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_bytes: 0,
        }
    }

    fn get(&mut self, id: [u8; 32]) -> Option<std::sync::Arc<ChunkProjection>> {
        self.entries.get(&id).cloned()
    }

    fn put(&mut self, proj: std::sync::Arc<ChunkProjection>) {
        let id = proj.map.file_content_id;
        if self.entries.contains_key(&id) {
            return;
        }
        // Reclaim oldest entries until the new one fits. A file larger than
        // the cap alone is not cached (rebuild per request) but still
        // served correctly.
        while self.total_bytes + proj.raw.len() > STAGED_CAP_BYTES
            && let Some(victim) = self.order.pop_front()
        {
            if let Some(v) = self.entries.remove(&victim) {
                self.total_bytes = self.total_bytes.saturating_sub(v.raw.len());
            }
        }
        if proj.raw.len() <= STAGED_CAP_BYTES {
            self.total_bytes += proj.raw.len();
            self.order.push_back(id);
            self.entries.insert(id, proj);
        }
    }
}

/// Return the cached projection, or build one via `load` (which must resolve
/// the file in the fixed view and return its verified bytes).
pub async fn get_or_project<F, Fut>(
    content_id: [u8; 32],
    load: F,
) -> Result<std::sync::Arc<ChunkProjection>, SnapshotError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, SnapshotError>>,
{
    let cache = STAGED.get_or_init(|| Mutex::new(ProjectionCache::new()));
    if let Some(p) = cache.lock().unwrap().get(content_id) {
        return Ok(p);
    }
    let raw = load().await?;
    let proj = std::sync::Arc::new(ChunkProjection::build(content_id, raw)?);
    cache.lock().unwrap().put(proj.clone());
    Ok(proj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(pattern: u8, size: usize) -> ([u8; 32], Vec<u8>) {
        let raw = vec![pattern; size];
        let mut h = Sha256::new();
        h.update(&raw);
        (h.finalize().into(), raw)
    }

    #[test]
    fn one_mib_plus_one_makes_two_chunks_with_positive_remainder() {
        let (id, raw) = data(0x42, CHUNK_SIZE as usize + 1);
        let p = ChunkProjection::build(id, raw).unwrap();
        assert_eq!(p.map.chunk_count, 2);
        assert_eq!(p.map.page_count, 1);
        assert_eq!(p.chunk_bytes(0).unwrap().len(), CHUNK_SIZE as usize);
        assert_eq!(p.chunk_bytes(1).unwrap().len(), 1);
        assert!(p.chunk_bytes(2).is_err());
    }

    #[test]
    fn exact_multiple_last_chunk_is_full_not_zero() {
        let (id, raw) = data(0x7, 2 * CHUNK_SIZE as usize);
        let p = ChunkProjection::build(id, raw).unwrap();
        assert_eq!(p.map.chunk_count, 2);
        assert_eq!(p.chunk_bytes(1).unwrap().len(), CHUNK_SIZE as usize);
    }

    #[test]
    fn wrong_content_id_is_rejected() {
        let (_, raw) = data(0x1, 10);
        assert!(ChunkProjection::build([0u8; 32], raw).is_err());
    }

    #[test]
    fn empty_file_has_no_projection() {
        let mut h = Sha256::new();
        h.update(b"");
        let id: [u8; 32] = h.finalize().into();
        assert!(ChunkProjection::build(id, Vec::new()).is_err());
    }

    #[test]
    fn proof_for_every_page_verifies_against_pages_root() {
        use mst2_codec::chunkmap::verify_leaf;
        // 257 chunks => 2 leaves (256 + 1), exercising the Merkle split.
        let (id, raw) = data(0x9, CHUNK_SIZE as usize * 257);
        let p = ChunkProjection::build(id, raw).unwrap();
        assert_eq!(p.page_count(), 2);
        for page_index in 0..p.page_count() {
            let (leaf, proof) = p.leaf_and_proof(page_index).unwrap();
            let hash = leaf.leaf_hash().unwrap();
            verify_leaf(p.page_count(), page_index, hash, &proof, p.map.pages_root).unwrap();
        }
    }
}
