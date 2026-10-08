//! Cold full-stream chunk-map verifier. Actual content callers use immutable
//! source receipts and selected persisted pages, never a digest-only cache.

use std::sync::Arc;

use mst2_codec::chunkmap::{CHUNK_SIZE, CHUNKS_PER_PAGE, ChunkLeaf, ChunkMap};
use sha2::{Digest, Sha256};

use super::content_budget::MemoryLease;
use crate::ceres::snapshot::error::{SnapshotError, SnapshotErrorCode};

#[path = "chunks_stream.rs"]
mod streaming;

/// Full-stream admission for one exact source, independent of the digest cache.
/// This opaque value is the only production input to durable map installation.
pub(crate) struct VerifiedSourceChunkMap {
    source: ChunkMapSource,
    projection: ChunkProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChunkMapSource {
    fact: crate::callisto::mst2_verified_object::Model,
}

impl ChunkMapSource {
    pub(crate) fn from_fact(
        fact: crate::callisto::mst2_verified_object::Model,
        oid: &str,
    ) -> Result<Self, SnapshotError> {
        if fact.id <= 0
            || fact.storage_domain != "git"
            || fact.object_kind != "blob"
            || fact.git_oid != oid
            || ![40, 64].contains(&oid.len())
            || !oid
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            || fact.state != "VERIFIED"
            || fact.verification_version
                != crate::jupiter::storage::mono_storage::MST2_VERIFICATION_VERSION
            || fact.raw_sha256.len() != 32
            || fact.size <= 0
            || fact.size as u64 > 8 * 1024 * 1024 * 1024 * 1024
        {
            return Err(SnapshotError::new(
                SnapshotErrorCode::IntegrityError,
                "invalid exact chunk map source fact",
            ));
        }
        Ok(Self { fact })
    }

    pub(crate) fn fact(&self) -> &crate::callisto::mst2_verified_object::Model {
        &self.fact
    }

    pub(crate) fn canonical_bytes(&self) -> Result<Vec<u8>, SnapshotError> {
        let f = &self.fact;
        serde_json::to_vec(&(
            f.id,
            &f.storage_domain,
            &f.git_oid,
            &f.object_kind,
            &f.raw_sha256,
            f.size,
            f.verification_version,
            &f.state,
            f.created_at,
        ))
        .map_err(|_| internal("chunk map source encoding failed"))
    }
}

impl VerifiedSourceChunkMap {
    pub(crate) async fn verify<T: crate::ceres::api_service::ApiHandler + ?Sized>(
        handler: &T,
        source: ChunkMapSource,
        budget: &Arc<super::content_budget::MemoryBudget>,
        admission: &crate::jupiter::storage::native_chunk_map::retention::ChunkMapInstall,
    ) -> Result<Self, SnapshotError> {
        let digest: [u8; 32] = source
            .fact
            .raw_sha256
            .as_slice()
            .try_into()
            .map_err(|_| internal("chunk map source digest shape changed"))?;
        let projection = streaming::build_source_stream(
            digest,
            source.fact.size as u64,
            || async {
                handler
                    .get_raw_blob_stream_by_hash(&source.fact.git_oid)
                    .await
                    .map_err(|error| {
                        let code = match error {
                            crate::common::errors::MegaError::ObjStorageNotFound(_) => {
                                SnapshotErrorCode::ObjectUnavailable
                            }
                            crate::common::errors::MegaError::ObjStorageInconsistent(_) => {
                                SnapshotErrorCode::IntegrityError
                            }
                            _ => SnapshotErrorCode::Internal,
                        };
                        SnapshotError::new(code, "exact chunk map source could not be read")
                    })
            },
            budget,
            admission,
        )
        .await?;
        Ok(Self { source, projection })
    }

    pub(crate) fn source(&self) -> &ChunkMapSource {
        &self.source
    }
    pub(crate) fn map(&self) -> &ChunkMap {
        &self.projection.map
    }
    pub(crate) fn leaves(&self) -> &[ChunkLeaf] {
        &self.projection.leaves
    }
    pub(crate) fn leaf_hashes(&self) -> &[[u8; 32]] {
        &self.projection.leaf_hashes
    }
}

pub(crate) fn map_build_reservation_bytes(size: u64) -> Result<usize, SnapshotError> {
    streaming::reserved_map_bytes(size)
}

/// One file's verified range-readable projection.
pub struct ChunkProjection {
    pub map: ChunkMap,
    pub map_id: [u8; 32],
    leaves: Vec<ChunkLeaf>,
    leaf_hashes: Vec<[u8; 32]>,
    /// Empty for range-only projections; empty files have no projection.
    raw: Vec<u8>,
    memory: Option<MemoryLease>,
}

impl ChunkProjection {
    /// Build from bytes that the caller already resolved at a fixed path.
    /// `content_id` is the digest the fixed view advertises.
    #[cfg(test)]
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
            memory: None,
        })
    }

    #[cfg(test)]
    pub fn has_inline_bytes(&self) -> bool {
        !self.raw.is_empty()
    }

    fn retained_bytes(&self) -> usize {
        self.memory
            .as_ref()
            .map_or_else(|| self.allocated_bytes(), |lease| lease.bytes)
    }

    fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.raw.capacity()
            + self.leaves.capacity() * std::mem::size_of::<ChunkLeaf>()
            + self.leaf_hashes.capacity() * 32
            + self
                .leaves
                .iter()
                .map(|leaf| leaf.chunk_sha256.capacity() * 32)
                .sum::<usize>()
    }

    #[cfg(test)]
    pub fn verify_chunk(&self, index: u64, bytes: &[u8]) -> Result<(), SnapshotError> {
        let want_len = self.map.chunk_len(index).map_err(codec_err)?;
        if bytes.len() as u64 != want_len {
            return Err(SnapshotError::new(
                SnapshotErrorCode::IntegrityError,
                "range length disagrees with the verified chunk map",
            ));
        }
        let page = (index / CHUNKS_PER_PAGE as u64) as usize;
        let slot = (index % CHUNKS_PER_PAGE as u64) as usize;
        let got: [u8; 32] = Sha256::digest(bytes).into();
        if got != self.leaves[page].chunk_sha256[slot] {
            return Err(SnapshotError::new(
                SnapshotErrorCode::IntegrityError,
                "range digest disagrees with the verified chunk map",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn page_count(&self) -> u64 {
        self.map.page_count
    }

    /// One MCL2 leaf plus its bottom-up proof toward `pages_root`.
    #[cfg(test)]
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
    #[cfg(test)]
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
