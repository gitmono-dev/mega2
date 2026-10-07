use std::{
    io,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use bytes::Bytes;
use tokio::{sync::Notify, time::timeout};

use super::*;
use crate::ceres::snapshot::content_budget::MemoryBudget;

fn stream(parts: Vec<Result<Bytes, io::Error>>) -> ObjectByteStream {
    Box::pin(futures::stream::iter(parts))
}

#[tokio::test]
async fn exact_source_builder_admits_all_install_workspace_before_open_and_cancellation_releases_it()
 {
    let raw = Bytes::from_static(b"exact source body");
    let digest: [u8; 32] = Sha256::digest(&raw).into();
    let weight = source_reservation_bytes(raw.len() as u64).unwrap();
    assert!(weight > 4 * 1024 * 1024);
    let budget = MemoryBudget::new(weight);
    let builders = Arc::new(Semaphore::new(1));
    let opens = Arc::new(AtomicUsize::new(0));
    let held = budget.reserve(weight).unwrap();
    let error = build_source_with_resources(
        digest,
        raw.len() as u64,
        || async {
            opens.fetch_add(1, Ordering::SeqCst);
            Ok(stream(vec![Ok(raw.clone())]))
        },
        &budget,
        &builders,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, SnapshotErrorCode::TemporaryUnavailable);
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    assert_eq!(builders.available_permits(), 1);
    drop(held);
    let held_builder = builders.clone().acquire_owned().await.unwrap();
    assert_eq!(
        build_source_with_resources(
            digest,
            raw.len() as u64,
            || async {
                opens.fetch_add(1, Ordering::SeqCst);
                Ok(stream(vec![Ok(raw.clone())]))
            },
            &budget,
            &builders
        )
        .await
        .err()
        .unwrap()
        .code,
        SnapshotErrorCode::TemporaryUnavailable
    );
    assert_eq!(opens.load(Ordering::SeqCst), 0);
    assert_eq!(budget.used(), 0);
    drop(held_builder);
    let entered = Arc::new(Notify::new());
    let task_budget = budget.clone();
    let task_builders = builders.clone();
    let task_entered = entered.clone();
    let task_opens = opens.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let producer_owner = DropCount(drops.clone());
    let size = raw.len() as u64;
    let task = tokio::spawn(async move {
        build_source_with_resources(
            digest,
            size,
            || async {
                task_opens.fetch_add(1, Ordering::SeqCst);
                task_entered.notify_one();
                Ok(Box::pin(futures::stream::unfold(
                    producer_owner,
                    |owner| async move {
                        futures::future::pending::<()>().await;
                        Some((Ok(Bytes::new()), owner))
                    },
                )) as ObjectByteStream)
            },
            &task_budget,
            &task_builders,
        )
        .await
    });
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert_eq!(budget.used(), weight);
    assert_eq!(builders.available_permits(), 0);
    task.abort();
    assert!(task.await.err().unwrap().is_cancelled());
    assert_eq!(budget.used(), 0);
    assert_eq!(builders.available_permits(), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let projection = build_source_with_resources(
        digest,
        size,
        || async { Ok(stream(vec![Ok(raw.clone())])) },
        &budget,
        &builders,
    )
    .await
    .unwrap();
    assert!(!projection.has_inline_bytes());
    assert_eq!(projection.map.file_content_id, digest);
    assert_eq!(opens.load(Ordering::SeqCst), 2);
    assert_eq!(builders.available_permits(), 1);
    assert_eq!(budget.used(), weight);
    projection.verify_chunk(0, &raw).unwrap();
    drop(projection);
    assert_eq!(budget.used(), 0);
}

async fn project(raw: &[u8], parts: Vec<Result<Bytes, io::Error>>) -> ChunkProjection {
    let budget = MemoryBudget::new(reserved_bytes(raw.len() as u64).unwrap());
    let lease = budget
        .reserve(reserved_bytes(raw.len() as u64).unwrap())
        .unwrap();
    build_stream(
        Sha256::digest(raw).into(),
        raw.len() as u64,
        stream(parts),
        lease,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn arbitrary_fragmentation_preserves_canonical_map_leaves_and_full_final_chunk() {
    for size in [
        1,
        CHUNK_SIZE as usize - 1,
        CHUNK_SIZE as usize,
        CHUNK_SIZE as usize + 7,
        2 * CHUNK_SIZE as usize,
    ] {
        let raw: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let parts = raw
            .chunks(997)
            .map(|part| Ok(Bytes::copy_from_slice(part)))
            .collect();
        let projection = project(&raw, parts).await;
        let oracle = ChunkProjection::build(Sha256::digest(&raw).into(), raw.clone()).unwrap();
        assert_eq!(projection.map, oracle.map);
        assert_eq!(projection.map_id, oracle.map_id);
        assert_eq!(projection.leaves, oracle.leaves);
        assert_eq!(projection.leaf_hashes, oracle.leaf_hashes);
        assert!(projection.has_inline_bytes());
        for index in 0..projection.map.chunk_count {
            assert_eq!(
                projection.chunk_bytes(index).unwrap(),
                oracle.chunk_bytes(index).unwrap()
            );
            let (leaf, proof) = projection
                .leaf_and_proof(index / CHUNKS_PER_PAGE as u64)
                .unwrap();
            mst2_codec::chunkmap::verify_leaf(
                projection.page_count(),
                leaf.page_index,
                leaf.leaf_hash().unwrap(),
                &proof,
                projection.map.pages_root,
            )
            .unwrap();
        }
    }
}

#[tokio::test]
async fn large_production_threshold_retains_only_map_and_verifies_every_page() {
    // Reuse one producer allocation; never create the >512 MiB raw Vec.
    let block = Bytes::from(vec![37; CHUNK_SIZE as usize]);
    let size = STAGED_CAP_BYTES as u64 + 7;
    let mut full = Sha256::new();
    for _ in 0..512 {
        full.update(&block);
    }
    full.update(&block[..7]);
    let content_id: [u8; 32] = full.finalize().into();
    let input = Box::pin(futures::stream::iter((0..513).map(move |index| {
        Ok(if index == 512 {
            block.slice(..7)
        } else {
            block.clone()
        })
    })));
    let weight = reserved_bytes(size).unwrap();
    let budget = MemoryBudget::new(weight);
    let lease = budget.reserve(weight).unwrap();
    let projection = build_stream(content_id, size, input, lease).await.unwrap();
    assert!(!projection.has_inline_bytes());
    assert_eq!(projection.raw.capacity(), 0);
    assert_eq!(projection.map.chunk_count, 513);
    assert_eq!(projection.page_count(), 3);
    assert!(projection.allocated_bytes() < 128 * 1024);
    assert_eq!(budget.used(), weight);
    let full_chunk: [u8; 32] = Sha256::digest(vec![37; CHUNK_SIZE as usize]).into();
    let last_chunk: [u8; 32] = Sha256::digest([37; 7]).into();
    assert_eq!(projection.leaves[0].chunk_sha256, vec![full_chunk; 256]);
    assert_eq!(projection.leaves[1].chunk_sha256, vec![full_chunk; 256]);
    assert_eq!(projection.leaves[2].chunk_sha256, vec![last_chunk]);
    for page in 0..3 {
        let (leaf, proof) = projection.leaf_and_proof(page).unwrap();
        mst2_codec::chunkmap::verify_leaf(
            3,
            page,
            leaf.leaf_hash().unwrap(),
            &proof,
            projection.map.pages_root,
        )
        .unwrap();
    }
    projection.verify_chunk(512, &[37; 7]).unwrap();
    assert_eq!(
        projection.verify_chunk(512, &[38; 7]).unwrap_err().code,
        SnapshotErrorCode::IntegrityError
    );
    drop(projection);
    assert_eq!(budget.used(), 0);
}

#[tokio::test]
async fn exact_eof_wrong_hash_growth_and_late_error_never_publish_a_projection() {
    let cases = [
        (
            vec![Ok(Bytes::from_static(b"ab"))],
            SnapshotErrorCode::IntegrityError,
        ),
        (
            vec![Ok(Bytes::from_static(b"abc")), Ok(Bytes::from_static(b"d"))],
            SnapshotErrorCode::IntegrityError,
        ),
        (
            vec![Ok(Bytes::from_static(b"abd"))],
            SnapshotErrorCode::DigestMismatch,
        ),
        (
            vec![
                Ok(Bytes::from_static(b"abc")),
                Err(io::Error::other("late")),
            ],
            SnapshotErrorCode::ObjectUnavailable,
        ),
    ];
    for (parts, code) in cases {
        let budget = MemoryBudget::new(reserved_bytes(3).unwrap());
        let lease = budget.reserve(reserved_bytes(3).unwrap()).unwrap();
        let failure = build_stream(Sha256::digest(b"abc").into(), 3, stream(parts), lease)
            .await
            .err()
            .unwrap();
        assert_eq!(failure.code, code);
        assert_eq!(budget.used(), 0);
    }
}

#[tokio::test]
async fn overlong_producer_is_rejected_before_hash_copy_and_tail_poll() {
    let tail = Arc::new(AtomicUsize::new(0));
    let input = Box::pin(futures::stream::unfold(
        (false, tail.clone()),
        |(sent, tail)| async move {
            if sent {
                tail.fetch_add(1, Ordering::SeqCst);
                Some((Err(io::Error::other("must not poll tail")), (true, tail)))
            } else {
                Some((Ok(Bytes::from_static(b"abcd")), (true, tail)))
            }
        },
    ));
    let budget = MemoryBudget::new(reserved_bytes(3).unwrap());
    let lease = budget.reserve(reserved_bytes(3).unwrap()).unwrap();
    assert_eq!(
        build_stream(Sha256::digest(b"abc").into(), 3, input, lease)
            .await
            .err()
            .unwrap()
            .code,
        SnapshotErrorCode::IntegrityError
    );
    assert_eq!(tail.load(Ordering::SeqCst), 0);
    assert_eq!(budget.used(), 0);
}

#[tokio::test]
async fn visible_producer_item_limit_does_not_collect_a_large_item() {
    let size = STREAM_ITEM_MAX_BYTES + 1;
    let budget = MemoryBudget::new(reserved_bytes(size as u64).unwrap());
    let lease = budget
        .reserve(reserved_bytes(size as u64).unwrap())
        .unwrap();
    let input = stream(vec![Ok(Bytes::from(vec![1; size]))]);
    assert_eq!(
        build_stream([0; 32], size as u64, input, lease)
            .await
            .err()
            .unwrap()
            .code,
        SnapshotErrorCode::TemporaryUnavailable
    );
    assert_eq!(budget.used(), 0);
}

struct DropCount(Arc<AtomicUsize>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn maximum_file_metadata_fits_and_protocol_or_quota_rejection_needs_no_source() {
    assert!(reserved_bytes(MAX_FILE_BYTES).unwrap() < 300 * 1024 * 1024);
    assert_eq!(
        reserved_bytes(0).unwrap_err().code,
        SnapshotErrorCode::ScopeInvalid
    );
    assert_eq!(
        reserved_bytes(MAX_FILE_BYTES + 1).unwrap_err().code,
        SnapshotErrorCode::LimitExceeded
    );
    assert!(reserved_bytes(STAGED_CAP_BYTES as u64).unwrap() > STAGED_CAP_BYTES);
    assert!(reserved_bytes(STAGED_CAP_BYTES as u64 + 1).unwrap() < 128 * 1024);
}
