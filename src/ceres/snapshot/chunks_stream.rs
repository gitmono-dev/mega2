use std::sync::{Arc, OnceLock};

use futures::StreamExt;
use tokio::sync::Semaphore;

use super::*;
use crate::orbit_api::object_storage::ObjectByteStream;

const STREAM_ITEM_MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024 * 1024;
const MAX_BUILDERS: usize = 4;
#[cfg(test)]
const STAGED_CAP_BYTES: usize = 512 * 1024 * 1024;
const CONSTRUCTION_ALLOWANCE: usize = 64 * 1024;

#[cfg(test)]
pub(super) fn reserved_bytes(size: u64) -> Result<usize, SnapshotError> {
    reserved_bytes_for(size, size <= STAGED_CAP_BYTES as u64)
}

pub(super) fn reserved_map_bytes(size: u64) -> Result<usize, SnapshotError> {
    reserved_bytes_for(size, false)
}

fn reserved_bytes_for(size: u64, inline: bool) -> Result<usize, SnapshotError> {
    if size == 0 || size > MAX_FILE_BYTES {
        return Err(SnapshotError::new(
            if size == 0 {
                SnapshotErrorCode::ScopeInvalid
            } else {
                SnapshotErrorCode::LimitExceeded
            },
            "file is outside the chunk projection profile",
        ));
    }
    let chunks = size.div_ceil(CHUNK_SIZE as u64);
    let pages = chunks.div_ceil(CHUNKS_PER_PAGE as u64);
    let inline = if inline { size } else { 0 };
    let bytes = chunks
        .checked_mul(32)
        .and_then(|n| {
            pages
                .checked_mul((std::mem::size_of::<ChunkLeaf>() + 32) as u64)
                .and_then(|pages| n.checked_add(pages))
        })
        .and_then(|n| n.checked_add(inline))
        .and_then(|n| n.checked_add(CONSTRUCTION_ALLOWANCE as u64))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| internal("chunk projection memory arithmetic overflow"))?;
    Ok(bytes)
}

#[cfg(test)]
async fn build_stream(
    content_id: [u8; 32],
    size: u64,
    input: ObjectByteStream,
    lease: MemoryLease,
) -> Result<ChunkProjection, SnapshotError> {
    build_stream_with_inline(
        content_id,
        size,
        input,
        lease,
        size <= STAGED_CAP_BYTES as u64,
        None,
    )
    .await
}

pub(super) async fn build_source_stream<F, Fut>(
    content_id: [u8; 32],
    size: u64,
    open: F,
    budget: &Arc<super::super::content_budget::MemoryBudget>,
    admission: &crate::jupiter::storage::native_chunk_map::retention::ChunkMapInstall,
) -> Result<ChunkProjection, SnapshotError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<ObjectByteStream, SnapshotError>>,
{
    static BUILDERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    build_source_with_resources(
        content_id,
        size,
        open,
        budget,
        BUILDERS.get_or_init(|| Arc::new(Semaphore::new(MAX_BUILDERS))),
        Some(admission),
    )
    .await
}

async fn build_source_with_resources<F, Fut>(
    content_id: [u8; 32],
    size: u64,
    open: F,
    budget: &Arc<super::super::content_budget::MemoryBudget>,
    builders: &Arc<Semaphore>,
    admission: Option<&crate::jupiter::storage::native_chunk_map::retention::ChunkMapInstall>,
) -> Result<ChunkProjection, SnapshotError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<ObjectByteStream, SnapshotError>>,
{
    let _builder = builders.clone().try_acquire_owned().map_err(|_| {
        SnapshotError::new(
            SnapshotErrorCode::TemporaryUnavailable,
            "chunk map builders are occupied",
        )
    })?;
    let lease = budget.reserve(source_reservation_bytes(size)?)?;
    let input = if let Some(admission) = admission {
        admission.ensure_live().await?;
        let result = admission.await_backend(open()).await?;
        admission.ensure_live().await?;
        result?
    } else {
        open().await?
    };
    build_stream_with_inline(content_id, size, input, lease, false, admission).await
}

pub(super) fn source_reservation_bytes(size: u64) -> Result<usize, SnapshotError> {
    let map_bytes = reserved_map_bytes(size)?;
    let pages = size
        .div_ceil(CHUNK_SIZE as u64)
        .div_ceil(CHUNKS_PER_PAGE as u64);
    let nodes = pages
        .checked_mul(2)
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| {
            n.checked_mul(std::mem::size_of::<super::super::chunk_map_index::ChunkMapNode>() as u64)
        })
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| internal("chunk map install reservation overflow"))?;
    map_bytes
        .checked_add(nodes)
        .and_then(|n| n.checked_add(4 * 1024 * 1024))
        .ok_or_else(|| internal("chunk map install reservation overflow"))
}

async fn build_stream_with_inline(
    content_id: [u8; 32],
    size: u64,
    mut input: ObjectByteStream,
    lease: MemoryLease,
    inline: bool,
    admission: Option<&crate::jupiter::storage::native_chunk_map::retention::ChunkMapInstall>,
) -> Result<ChunkProjection, SnapshotError> {
    let chunk_count = size.div_ceil(CHUNK_SIZE as u64);
    let page_count = chunk_count.div_ceil(CHUNKS_PER_PAGE as u64);
    let mut raw = Vec::new();
    let mut leaves = Vec::new();
    let mut leaf_hashes = Vec::new();
    let mut current = Vec::new();
    if inline {
        raw.try_reserve_exact(size as usize)
            .map_err(allocation_error)?;
    }
    leaves
        .try_reserve_exact(page_count as usize)
        .map_err(allocation_error)?;
    leaf_hashes
        .try_reserve_exact(page_count as usize)
        .map_err(allocation_error)?;
    current
        .try_reserve_exact(chunk_count.min(CHUNKS_PER_PAGE as u64) as usize)
        .map_err(allocation_error)?;
    let mut full_hash = Sha256::new();
    let mut chunk_hash = Sha256::new();
    let mut received = 0u64;
    let mut chunk_bytes = 0usize;
    let mut digests = 0u64;
    let mut since_yield = 0usize;
    let mut empty_parts = 0usize;
    loop {
        let next = if let Some(admission) = admission {
            admission.ensure_live().await?;
            let next = admission.await_backend(input.next()).await?;
            admission.ensure_live().await?;
            next
        } else {
            input.next().await
        };
        let Some(part) = next else { break };
        let bytes = part.map_err(|error| {
            tracing::warn!(error = %error, "fixed-view chunk projection stream failed");
            SnapshotError::new(
                SnapshotErrorCode::ObjectUnavailable,
                "content stream failed",
            )
        })?;
        if bytes.len() as u64 > size - received {
            return Err(length_error());
        }
        // A visible-item admission limit cannot bound a producer's backing
        // allocation, buffers or work surviving cancellation.
        if bytes.len() > STREAM_ITEM_MAX_BYTES {
            return Err(SnapshotError::new(
                SnapshotErrorCode::TemporaryUnavailable,
                "content producer item exceeds the consumer processing limit",
            ));
        }
        full_hash.update(&bytes);
        if inline {
            raw.extend_from_slice(&bytes);
        }
        received += bytes.len() as u64;
        if let Some(admission) = admission {
            if !bytes.is_empty() {
                admission.record_progress(received).await?;
            } else {
                empty_parts += 1;
                if empty_parts == 32 {
                    tokio::task::yield_now().await;
                    admission.ensure_live().await?;
                    empty_parts = 0;
                }
            }
        }
        let mut rest = bytes.as_ref();
        while !rest.is_empty() {
            let len = rest.len().min(CHUNK_SIZE as usize - chunk_bytes);
            chunk_hash.update(&rest[..len]);
            chunk_bytes += len;
            rest = &rest[len..];
            if chunk_bytes == CHUNK_SIZE as usize {
                current.push(chunk_hash.finalize_reset().into());
                digests += 1;
                chunk_bytes = 0;
                if current.len() == CHUNKS_PER_PAGE {
                    append_leaf(&mut leaves, &mut leaf_hashes, std::mem::take(&mut current))?;
                    if digests < chunk_count {
                        current
                            .try_reserve_exact(
                                (chunk_count - digests).min(CHUNKS_PER_PAGE as u64) as usize
                            )
                            .map_err(allocation_error)?;
                    }
                }
            }
        }
        since_yield += bytes.len();
        if since_yield >= STREAM_ITEM_MAX_BYTES {
            // A ready stream still cooperates with cancellation and the
            // executor while hashing a large file. No detached CPU worker.
            tokio::task::yield_now().await;
            since_yield = 0;
        }
    }
    if received != size {
        return Err(length_error());
    }
    let got: [u8; 32] = full_hash.finalize().into();
    if got != content_id {
        return Err(SnapshotError::new(
            SnapshotErrorCode::DigestMismatch,
            "chunk projection input does not hash to content_id",
        ));
    }
    if chunk_bytes != 0 {
        current.push(chunk_hash.finalize().into());
        digests += 1;
    }
    if !current.is_empty() {
        append_leaf(&mut leaves, &mut leaf_hashes, current)?;
    }
    if digests != chunk_count || leaves.len() as u64 != page_count {
        return Err(internal(
            "streamed chunk boundaries disagree with file size",
        ));
    }
    let pages_root = mst2_codec::chunkmap::merkle_root(&leaf_hashes).map_err(codec_err)?;
    let map = ChunkMap::new(content_id, size, pages_root).map_err(codec_err)?;
    let projection = ChunkProjection {
        map_id: map.map_id(),
        map,
        leaves,
        leaf_hashes,
        raw,
        memory: Some(lease),
    };
    if projection.allocated_bytes() > projection.retained_bytes() {
        return Err(internal("projection allocation exceeds its reservation"));
    }
    Ok(projection)
}

fn append_leaf(
    leaves: &mut Vec<ChunkLeaf>,
    hashes: &mut Vec<[u8; 32]>,
    chunk_sha256: Vec<[u8; 32]>,
) -> Result<(), SnapshotError> {
    let leaf = ChunkLeaf {
        page_index: leaves.len() as u64,
        chunk_sha256,
    };
    hashes.push(leaf.leaf_hash().map_err(codec_err)?);
    leaves.push(leaf);
    Ok(())
}

fn allocation_error(_: std::collections::TryReserveError) -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::TemporaryUnavailable,
        "chunk projection allocation could not be admitted",
    )
}

fn length_error() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::IntegrityError,
        "fixed blob length disagrees with its verified size fact",
    )
}

#[cfg(test)]
#[path = "chunks_stream_tests.rs"]
mod tests;
