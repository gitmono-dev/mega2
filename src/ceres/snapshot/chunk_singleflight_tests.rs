use std::{
    sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::{sync::Notify, time::timeout};

use super::*;

fn unique_data(size: usize) -> ([u8; 32], Arc<Vec<u8>>) {
    let seed = uuid::Uuid::new_v4();
    let raw: Vec<u8> = (0..size).map(|i| seed.as_bytes()[i % 16]).collect();
    (Sha256::digest(&raw).into(), Arc::new(raw))
}

async fn started(signal: &Notify) {
    timeout(Duration::from_secs(5), signal.notified())
        .await
        .unwrap();
}

async fn participants(content_id: [u8; 32], expected: usize) {
    timeout(Duration::from_secs(5), async {
        loop {
            let count = FLIGHTS
                .get()
                .unwrap()
                .lock()
                .unwrap()
                .entries
                .get(&content_id)
                .map_or(0, Weak::strong_count);
            if count == expected {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn assert_flight_released(content_id: [u8; 32]) {
    assert!(
        !FLIGHTS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .entries
            .contains_key(&content_id)
    );
}

fn assert_uncached(content_id: [u8; 32]) {
    assert!(
        STAGED
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .get(content_id)
            .is_none()
    );
}

fn evict(content_id: [u8; 32]) {
    let mut cache = STAGED.get().unwrap().lock().unwrap();
    let projection = cache.entries.remove(&content_id).unwrap();
    cache.total_bytes -= projection.retained_bytes();
    let position = cache.order.iter().position(|id| *id == content_id).unwrap();
    cache.order.remove(position);
}

#[tokio::test]
async fn cold_same_digest_loads_once_and_warm_cache_skips_loader() {
    let (content_id, raw) = unique_data(CHUNK_SIZE as usize + 7);
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let leader = tokio::spawn({
        let raw = raw.clone();
        let calls = calls.clone();
        let entered = entered.clone();
        let release = release.clone();
        async move {
            get_or_project(content_id, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                Ok(raw.as_ref().clone())
            })
            .await
        }
    });
    started(&entered).await;
    let mut waiters = Vec::new();
    for _ in 0..7 {
        let raw = raw.clone();
        let calls = calls.clone();
        waiters.push(tokio::spawn(async move {
            get_or_project(content_id, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(raw.as_ref().clone())
            })
            .await
        }));
    }
    participants(content_id, 8).await;
    release.notify_one();
    let projection = leader.await.unwrap().unwrap();
    for waiter in waiters {
        let shared = waiter.await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&projection, &shared));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(projection.map.file_content_id, content_id);
    assert_eq!(projection.map.chunk_count, 2);
    assert_eq!(
        projection.chunk_bytes(0).unwrap(),
        &raw[..CHUNK_SIZE as usize]
    );
    assert_eq!(
        projection.chunk_bytes(1).unwrap(),
        &raw[CHUNK_SIZE as usize..]
    );
    let (leaf, proof) = projection.leaf_and_proof(0).unwrap();
    mst2_codec::chunkmap::verify_leaf(
        projection.page_count(),
        0,
        leaf.leaf_hash().unwrap(),
        &proof,
        projection.map.pages_root,
    )
    .unwrap();
    let warm = get_or_project(content_id, || async {
        panic!("warm projection must not invoke its loader");
    })
    .await
    .unwrap();
    assert!(Arc::ptr_eq(&projection, &warm));
    assert_flight_released(content_id);
}

#[tokio::test]
async fn different_digests_enter_their_loaders_independently() {
    let mut requests = Vec::new();
    let mut signals = Vec::new();
    for _ in 0..2 {
        let (content_id, raw) = unique_data(97);
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        signals.push((content_id, entered.clone(), release.clone()));
        requests.push(tokio::spawn(async move {
            get_or_project(content_id, || async move {
                entered.notify_one();
                release.notified().await;
                Ok(raw.as_ref().clone())
            })
            .await
        }));
    }
    for (_, entered, _) in &signals {
        started(entered).await;
    }
    for (_, _, release) in &signals {
        release.notify_one();
    }
    for request in requests {
        request.await.unwrap().unwrap();
    }
    for (content_id, _, _) in signals {
        assert_flight_released(content_id);
    }
}

async fn failed_leader_then_waiter(wrong_digest: bool) {
    let (content_id, raw) = unique_data(103);
    let calls = Arc::new(AtomicUsize::new(0));
    let first_entered = Arc::new(Notify::new());
    let first_release = Arc::new(Notify::new());
    let second_entered = Arc::new(Notify::new());
    let second_release = Arc::new(Notify::new());
    let leader = tokio::spawn({
        let calls = calls.clone();
        let entered = first_entered.clone();
        let release = first_release.clone();
        async move {
            get_or_project(content_id, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                if wrong_digest {
                    Ok(vec![0; 103])
                } else {
                    Err(SnapshotError::new(
                        SnapshotErrorCode::ObjectUnavailable,
                        "failed test loader",
                    ))
                }
            })
            .await
        }
    });
    started(&first_entered).await;
    let waiter = tokio::spawn({
        let raw = raw.clone();
        let calls = calls.clone();
        let entered = second_entered.clone();
        let release = second_release.clone();
        async move {
            get_or_project(content_id, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                Ok(raw.as_ref().clone())
            })
            .await
        }
    });
    participants(content_id, 2).await;
    first_release.notify_one();
    let failure = leader.await.unwrap().err().unwrap();
    assert_eq!(
        failure.code,
        if wrong_digest {
            SnapshotErrorCode::DigestMismatch
        } else {
            SnapshotErrorCode::ObjectUnavailable
        }
    );
    started(&second_entered).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_uncached(content_id);
    second_release.notify_one();
    let projection = waiter.await.unwrap().unwrap();
    assert_eq!(projection.chunk_bytes(0).unwrap(), raw.as_slice());
    assert_flight_released(content_id);
}

#[tokio::test]
async fn load_failure_is_unpublished_and_waiter_uses_its_loader() {
    failed_leader_then_waiter(false).await;
}

#[tokio::test]
async fn digest_failure_is_unpublished_and_waiter_uses_its_loader() {
    failed_leader_then_waiter(true).await;
}

#[tokio::test]
async fn cancelled_leader_releases_gate_for_waiting_loader() {
    let (content_id, raw) = unique_data(113);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let takeover = Arc::new(Notify::new());
    let takeover_release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let leader = tokio::spawn({
        let raw = raw.clone();
        let calls = calls.clone();
        let entered = entered.clone();
        let release = release.clone();
        async move {
            get_or_project(content_id, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                Ok(raw.as_ref().clone())
            })
            .await
        }
    });
    started(&entered).await;
    let waiter = tokio::spawn({
        let raw = raw.clone();
        let calls = calls.clone();
        let takeover = takeover.clone();
        let takeover_release = takeover_release.clone();
        async move {
            get_or_project(content_id, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                takeover.notify_one();
                takeover_release.notified().await;
                Ok(raw.as_ref().clone())
            })
            .await
        }
    });
    participants(content_id, 2).await;
    leader.abort();
    assert!(leader.await.err().unwrap().is_cancelled());
    started(&takeover).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_uncached(content_id);
    takeover_release.notify_one();
    let projection = waiter.await.unwrap().unwrap();
    assert_eq!(projection.chunk_bytes(0).unwrap(), raw.as_slice());
    assert_flight_released(content_id);
}

#[tokio::test]
async fn cancelled_waiter_does_not_cancel_or_leak_the_leader() {
    let (content_id, raw) = unique_data(127);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let leader = tokio::spawn({
        let entered = entered.clone();
        let release = release.clone();
        async move {
            get_or_project(content_id, || async move {
                entered.notify_one();
                release.notified().await;
                Ok(raw.as_ref().clone())
            })
            .await
        }
    });
    started(&entered).await;
    let waiter = tokio::spawn(async move {
        get_or_project(content_id, || async {
            panic!("cancelled waiter must not load while leader is active");
        })
        .await
    });
    participants(content_id, 2).await;
    waiter.abort();
    assert!(waiter.await.err().unwrap().is_cancelled());
    participants(content_id, 1).await;
    release.notify_one();
    let projection = leader.await.unwrap().unwrap();
    assert_eq!(projection.map.file_content_id, content_id);
    assert_flight_released(content_id);
}

#[tokio::test]
async fn evicted_result_survives_leader_drop_until_joined_waiter_finishes() {
    let (content_id, raw) = unique_data(131);
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let leader = tokio::spawn({
        let raw = raw.clone();
        let calls = calls.clone();
        let entered = entered.clone();
        let release = release.clone();
        async move {
            get_or_project(content_id, || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
                Ok(raw.as_ref().clone())
            })
            .await
        }
    });
    started(&entered).await;
    let registry = FLIGHTS.get().unwrap();
    let retained = FlightClaim::acquire(registry, content_id).unwrap();
    let result_weak;
    {
        // Queue a test-only lock before the real waiter, keeping its turn
        // blocked until eviction and the leader's return owner are gone.
        let queued = retained.flight.as_ref().unwrap().result.lock();
        tokio::pin!(queued);
        assert!(futures::poll!(&mut queued).is_pending());
        let waiter = tokio::spawn({
            let raw = raw.clone();
            let calls = calls.clone();
            async move {
                get_or_project(content_id, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(raw.as_ref().clone())
                })
                .await
            }
        });
        participants(content_id, 3).await;
        release.notify_one();
        let projection = leader.await.unwrap().unwrap();
        let guard = queued.await;
        result_weak = Arc::downgrade(&projection);
        evict(content_id);
        drop(projection);
        assert_uncached(content_id);
        assert!(result_weak.upgrade().is_some());
        drop(guard);
        let shared = waiter.await.unwrap().unwrap();
        assert!(result_weak.ptr_eq(&Arc::downgrade(&shared)));
        assert_eq!(shared.chunk_bytes(0).unwrap(), raw.as_slice());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        drop(shared);
    }
    assert!(result_weak.upgrade().is_some());
    drop(retained);
    assert!(result_weak.upgrade().is_none());
    assert_flight_released(content_id);
    assert_uncached(content_id);
    let reloaded = get_or_project(content_id, || async {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(raw.as_ref().clone())
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(reloaded.chunk_bytes(0).unwrap(), raw.as_slice());
    assert_flight_released(content_id);
}

#[test]
fn distinct_flights_are_bounded_but_existing_digest_can_join_at_capacity() {
    let registry = Mutex::new(FlightRegistry::default());
    let mut claims = Vec::new();
    for index in 0..PROJECTION_FLIGHT_CAP {
        claims.push(FlightClaim::acquire(&registry, [index as u8; 32]).unwrap());
    }
    let joined = FlightClaim::acquire(&registry, [42; 32]).unwrap();
    assert!(Arc::ptr_eq(
        claims[42].flight.as_ref().unwrap(),
        joined.flight.as_ref().unwrap()
    ));
    let error = FlightClaim::acquire(&registry, [255; 32]).err().unwrap();
    assert_eq!(error.code, SnapshotErrorCode::LimitExceeded);
    drop(claims);
    assert_eq!(registry.lock().unwrap().entries.len(), 1);
    drop(joined);
    assert!(registry.lock().unwrap().entries.is_empty());
    let fresh = FlightClaim::acquire(&registry, [255; 32]).unwrap();
    drop(fresh);
    assert!(registry.lock().unwrap().entries.is_empty());
}

#[test]
fn last_claim_removes_only_its_exact_registered_gate() {
    let registry = Mutex::new(FlightRegistry::default());
    let content_id = [91; 32];
    let old = FlightClaim::acquire(&registry, content_id).unwrap();
    let replacement = Arc::new(ProjectionFlight {
        result: tokio::sync::Mutex::new(None),
    });
    registry
        .lock()
        .unwrap()
        .entries
        .insert(content_id, Arc::downgrade(&replacement));
    drop(old);
    assert!(registry.lock().unwrap().entries[&content_id].ptr_eq(&Arc::downgrade(&replacement)));
    let joined = FlightClaim::acquire(&registry, content_id).unwrap();
    assert!(Arc::ptr_eq(joined.flight.as_ref().unwrap(), &replacement));
    drop(replacement);
    drop(joined);
    assert!(registry.lock().unwrap().entries.is_empty());
    registry
        .lock()
        .unwrap()
        .entries
        .insert(content_id, Weak::new());
    let reclaimed = FlightClaim::acquire(&registry, content_id).unwrap();
    drop(reclaimed);
    assert!(registry.lock().unwrap().entries.is_empty());
}

#[test]
fn concurrent_final_claim_drops_release_registry_capacity() {
    let registry = Mutex::new(FlightRegistry::default());
    for index in 0..64 {
        let content_id = [index; 32];
        let first = FlightClaim::acquire(&registry, content_id).unwrap();
        let second = FlightClaim::acquire(&registry, content_id).unwrap();
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                drop(first);
            });
            scope.spawn(move || {
                barrier.wait();
                drop(second);
            });
        });
        assert!(registry.lock().unwrap().entries.is_empty());
    }
}
