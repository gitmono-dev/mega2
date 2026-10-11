use axum::{
    extract::{Path as AxumPath, Query, State},
    http::HeaderMap,
    routing::get,
};
use tokio::{sync::Notify, time::timeout};

use super::*;
use crate::{
    api::router::snapshot_router::{raw_blob as serving, snapshot_auth_middleware},
    ceres::snapshot::content_budget::{MemoryBudget, RANGE_WORK_BYTES},
};

pub(super) fn budgeted_app(
    fixture: &Fixture,
    response_budget: &Arc<MemoryBudget>,
    scratch_budget: &Arc<MemoryBudget>,
) -> Router {
    let response_budget = response_budget.clone();
    let scratch_budget = scratch_budget.clone();
    Router::new()
        .route(
            "/api/v2/snapshots/{snapshot_id}/blob",
            get(
                move |state: State<MonoApiServiceState>,
                      path: AxumPath<String>,
                      query: Query<serving::BlobQuery>,
                      headers: HeaderMap| {
                    serving::blob_with_budgets(
                        state,
                        path,
                        query,
                        headers,
                        serving::BlobBudgets {
                            response: response_budget.clone(),
                            scratch: scratch_budget.clone(),
                        },
                    )
                },
            ),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            fixture.state.clone(),
            snapshot_auth_middleware,
        ))
        .with_state(fixture.state.clone())
}

fn fault(fixture: &Fixture, kind: bounded_objects::FaultKind) {
    *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
        oid: fixture.oid.clone(),
        kind,
    });
}

async fn path_oid(fixture: &Fixture, path: &str) -> String {
    use crate::ceres::snapshot::pages::{MetadataWalkOutcome, resolve_abs_metadata};
    let handler = fixture
        .state
        .api_handler(std::path::Path::new("/"))
        .await
        .unwrap();
    let context = fixture
        .state
        .storage
        .snapshot_context(&fixture.snapshot, &fixture.lease)
        .await
        .unwrap();
    let root = handler
        .get_tree_by_hash(&context.root_tree_oid)
        .await
        .unwrap();
    match resolve_abs_metadata(handler.as_ref(), &root, &format!("/project{path}"))
        .await
        .unwrap()
    {
        MetadataWalkOutcome::FoundFile { oid, .. } => oid,
        other => panic!("raw fixture path must be a fixed file: {other:?}"),
    }
}

async fn release_lease(fixture: &Fixture) {
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v2/snapshots/leases/{}", fixture.lease))
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn cold_raw_get_earns_source_proof_then_serves_one_current_stream_with_exact_headers() {
    let fixture = Fixture::new_rooted_with_options(
        &[],
        FixtureOptions {
            lease_seconds: Some(3600),
            ..Default::default()
        },
    )
    .await;
    let response = fixture.send("GET", "blob?path=/file", Body::empty()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-length"],
        fixture.raw.len().to_string()
    );
    assert_eq!(
        response.headers()["x-mega-content-size"],
        fixture.raw.len().to_string()
    );
    assert_eq!(
        response.headers()["etag"],
        format!("\"{}\"", fixture.digest_string())
    );
    assert_eq!(response.headers()["x-mega-fs-kind"], "regular");
    assert_eq!(response.headers()["vary"], "Authorization, Accept");
    assert_eq!(
        response.headers()["cache-control"],
        "private, no-cache, no-transform"
    );
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 2);
    assert_eq!(
        fixture.counts.bytes.load(Ordering::SeqCst),
        fixture.raw.len()
    );
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        to_bytes(response.into_body(), 2 * CHUNK_SIZE as usize)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw
    );
    fixture.counts.assert(2, 2 * fixture.raw.len());
    fixture.counts.reset();
    let response = fixture.send("GET", "blob?path=/alias", Body::empty()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(fixture.counts.bytes.load(Ordering::SeqCst), 0);
    assert_eq!(
        to_bytes(response.into_body(), 2 * CHUNK_SIZE as usize)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw
    );
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn raw_memory_pressure_rejects_before_cold_proof_or_delivery_source_io() {
    let fixture = Fixture::new().await;
    for occupy_scratch in [false, true] {
        fixture.counts.reset();
        let response_budget = MemoryBudget::new(CHUNK_SIZE as usize);
        let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
        let held = if occupy_scratch {
            scratch_budget.reserve(RANGE_WORK_BYTES).unwrap()
        } else {
            response_budget.reserve(CHUNK_SIZE as usize).unwrap()
        };
        let response = budgeted_app(&fixture, &response_budget, &scratch_budget)
            .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
            .await
            .unwrap();
        error(response, 503, "TEMPORARY_UNAVAILABLE", true).await;
        fixture.counts.assert(0, 0);
        assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
        assert_eq!(
            scratch_budget.used(),
            if occupy_scratch { RANGE_WORK_BYTES } else { 0 }
        );
        assert_eq!(
            response_budget.used(),
            if occupy_scratch {
                0
            } else {
                CHUNK_SIZE as usize
            }
        );
        drop(held);
        assert_eq!(response_budget.used(), 0);
        assert_eq!(scratch_budget.used(), 0);
    }
}

#[tokio::test]
async fn raw_missing_or_forged_receipt_and_real_stored_corruption_fail_without_rebuilding() {
    let fixture = Fixture::new_rooted_with_options(
        &[],
        FixtureOptions {
            lease_seconds: Some(3600),
            ..Default::default()
        },
    )
    .await;
    fixture.map("/file").await;
    for missing in [true, false] {
        fixture.counts.reset();
        fixture
            .counts
            .receipt_read_failure
            .store(missing, Ordering::SeqCst);
        *fixture.counts.receipt_read_corruption.lock().unwrap() =
            (!missing).then(|| Bytes::from_static(b"forged receipt"));
        error(
            fixture.send("GET", "blob?path=/file", Body::empty()).await,
            502,
            "INTEGRITY_ERROR",
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
        assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    }
    fixture
        .counts
        .receipt_read_failure
        .store(false, Ordering::SeqCst);
    *fixture.counts.receipt_read_corruption.lock().unwrap() = None;
    let mut wrong = fixture.raw.clone();
    wrong[0] ^= 1;
    fixture.write_raw(wrong).await;
    let response = fixture.send("GET", "blob?path=/file", Body::empty()).await;
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    fixture.write_raw(fixture.raw.clone()).await;
    let response = fixture.send("GET", "blob?path=/file", Body::empty()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        to_bytes(response.into_body(), 2 * CHUNK_SIZE as usize)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw
    );
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn warm_fragmented_actual_raw_body_polls_one_chunk_and_transport_clones_keep_exact_credit() {
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    fixture.counts.reset();
    fault(
        &fixture,
        bounded_objects::FaultKind::Parts(vec![
            Bytes::new(),
            Bytes::copy_from_slice(&fixture.raw[..13]),
            Bytes::copy_from_slice(&fixture.raw[13..CHUNK_SIZE as usize - 7]),
            Bytes::copy_from_slice(&fixture.raw[CHUNK_SIZE as usize - 7..CHUNK_SIZE as usize]),
            Bytes::copy_from_slice(&fixture.raw[CHUNK_SIZE as usize..]),
            Bytes::new(),
        ]),
    );
    let response_budget = MemoryBudget::new(fixture.raw.len());
    let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
    let response = budgeted_app(&fixture, &response_budget, &scratch_budget)
        .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    fixture.counts.assert(1, 0);
    assert_eq!(response_budget.used(), CHUNK_SIZE as usize);
    assert_eq!(scratch_budget.used(), RANGE_WORK_BYTES);
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.as_ref(), &fixture.raw[..CHUNK_SIZE as usize]);
    fixture.counts.assert(1, CHUNK_SIZE as usize);
    let transport = first.clone();
    drop(first);
    assert_eq!(response_budget.used(), CHUNK_SIZE as usize);
    let final_bytes = stream.next().await.unwrap().unwrap();
    assert_eq!(final_bytes.as_ref(), &fixture.raw[CHUNK_SIZE as usize..]);
    assert_eq!(response_budget.used(), fixture.raw.len());
    assert!(stream.next().await.is_none());
    assert_eq!(scratch_budget.used(), 0);
    assert_eq!(response_budget.used(), fixture.raw.len());
    drop(stream);
    drop(final_bytes);
    assert_eq!(response_budget.used(), CHUNK_SIZE as usize);
    drop(transport);
    assert_eq!(response_budget.used(), 0);
    fixture.counts.assert(1, fixture.raw.len());
}

#[tokio::test]
async fn raw_transport_quota_rejects_next_chunk_before_more_source_io_and_errors_terminally() {
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    fixture.counts.reset();
    fault(
        &fixture,
        bounded_objects::FaultKind::Parts(vec![
            Bytes::copy_from_slice(&fixture.raw[..CHUNK_SIZE as usize]),
            Bytes::copy_from_slice(&fixture.raw[CHUNK_SIZE as usize..]),
        ]),
    );
    let response_budget = MemoryBudget::new(CHUNK_SIZE as usize);
    let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
    let response = budgeted_app(&fixture, &response_budget, &scratch_budget)
        .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    let transport = stream.next().await.unwrap().unwrap();
    assert_eq!(transport.as_ref(), &fixture.raw[..CHUNK_SIZE as usize]);
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    fixture.counts.assert(1, CHUNK_SIZE as usize);
    assert_eq!(scratch_budget.used(), 0);
    assert_eq!(response_budget.used(), CHUNK_SIZE as usize);
    drop(transport);
    assert_eq!(response_budget.used(), 0);
}

#[tokio::test]
async fn warm_raw_corruption_growth_truncation_and_late_error_never_yield_the_last_bytes() {
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    let mut wrong = fixture.raw.clone();
    wrong[0] ^= 1;
    let mut grown = fixture.raw.clone();
    grown.push(0);
    let cases = [
        (
            bounded_objects::FaultKind::Parts(vec![Bytes::from(wrong)]),
            0,
        ),
        (
            bounded_objects::FaultKind::Parts(vec![Bytes::from(grown)]),
            0,
        ),
        (
            bounded_objects::FaultKind::Parts(vec![Bytes::copy_from_slice(
                &fixture.raw[..fixture.raw.len() - 1],
            )]),
            CHUNK_SIZE as usize,
        ),
        (
            bounded_objects::FaultKind::LateError(Bytes::copy_from_slice(&fixture.raw)),
            CHUNK_SIZE as usize,
        ),
    ];
    for (kind, delivered) in cases {
        fixture.counts.reset();
        fault(&fixture, kind);
        let response_budget = MemoryBudget::new(2 * CHUNK_SIZE as usize);
        let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
        let response = budgeted_app(&fixture, &response_budget, &scratch_budget)
            .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut stream = response.into_body().into_data_stream();
        let mut bytes = Vec::new();
        loop {
            match stream.next().await {
                Some(Ok(part)) => bytes.extend_from_slice(&part),
                Some(Err(_)) => break,
                None => panic!("corrupt current raw source must fail the actual response body"),
            }
        }
        assert_eq!(bytes.len(), delivered);
        assert_eq!(bytes, fixture.raw[..delivered]);
        assert!(stream.next().await.is_none());
        assert_eq!(response_budget.used(), 0);
        assert_eq!(scratch_budget.used(), 0);
        assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    }
    *fixture.counts.object_fault.lock().unwrap() = None;
    let response = fixture.send("GET", "blob?path=/file", Body::empty()).await;
    assert_eq!(
        to_bytes(response.into_body(), 2 * CHUNK_SIZE as usize)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw
    );
}

#[tokio::test]
async fn raw_drop_cancel_and_revocation_during_held_io_drop_producer_and_all_owned_credit() {
    for mode in 0..3 {
        let fixture = Fixture::new().await;
        fixture.map("/file").await;
        fixture.counts.reset();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let drops = Arc::new(AtomicUsize::new(0));
        fault(
            &fixture,
            bounded_objects::FaultKind::Held {
                raw: Bytes::copy_from_slice(&fixture.raw),
                entered: entered.clone(),
                release: release.clone(),
                drops: drops.clone(),
            },
        );
        let response_budget = MemoryBudget::new(2 * CHUNK_SIZE as usize);
        let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
        let response = budgeted_app(&fixture, &response_budget, &scratch_budget)
            .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        fixture.counts.assert(1, 0);
        if mode == 0 {
            drop(response);
        } else {
            let mut stream = response.into_body().into_data_stream();
            let task = tokio::spawn(async move {
                let first = stream.next().await;
                (first, stream)
            });
            timeout(Duration::from_secs(10), entered.notified())
                .await
                .unwrap();
            fixture.counts.assert(1, 1);
            if mode == 1 {
                task.abort();
                assert!(task.await.err().unwrap().is_cancelled());
            } else {
                release_lease(&fixture).await;
                release.notify_one();
                let (first, mut stream) = task.await.unwrap();
                assert!(first.unwrap().is_err());
                assert!(stream.next().await.is_none());
            }
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(response_budget.used(), 0);
        assert_eq!(scratch_budget.used(), 0);
    }
}

#[tokio::test]
async fn raw_first_poll_rechecks_released_lease_before_consuming_body_bytes() {
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    fixture.counts.reset();
    let response_budget = MemoryBudget::new(2 * CHUNK_SIZE as usize);
    let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
    let response = budgeted_app(&fixture, &response_budget, &scratch_budget)
        .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    release_lease(&fixture).await;
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    fixture.counts.assert(1, 0);
    assert_eq!(response_budget.used(), 0);
    assert_eq!(scratch_budget.used(), 0);
}

#[tokio::test]
async fn raw_post_await_revocation_rejects_fragments_and_eof_before_another_backend_poll() {
    for case in 0..4 {
        let fixture = Fixture::new().await;
        fixture.map("/file").await;
        fixture.counts.reset();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let tail_polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let prefix = if case < 2 {
            Bytes::copy_from_slice(&fixture.raw[..1])
        } else {
            Bytes::copy_from_slice(&fixture.raw)
        };
        let prefix_length = prefix.len();
        let fragment = match case {
            0 => Some(Bytes::copy_from_slice(&fixture.raw[1..2])),
            1 | 2 => Some(Bytes::new()),
            _ => None,
        };
        fault(
            &fixture,
            bounded_objects::FaultKind::HeldFragment {
                prefix,
                fragment,
                entered: entered.clone(),
                release: release.clone(),
                tail_polls: tail_polls.clone(),
                drops: drops.clone(),
            },
        );
        let response_budget = MemoryBudget::new(2 * CHUNK_SIZE as usize);
        let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
        let response = budgeted_app(&fixture, &response_budget, &scratch_budget)
            .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut stream = response.into_body().into_data_stream();
        let mut task = tokio::spawn(async move {
            let mut delivered = Vec::new();
            loop {
                match stream.next().await {
                    Some(Ok(bytes)) => delivered.extend_from_slice(&bytes),
                    Some(Err(error)) => return (delivered, error, stream),
                    None => panic!("revoked source must fail the actual body"),
                }
            }
        });
        timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        fixture.counts.assert(1, prefix_length);
        release_lease(&fixture).await;
        release.notify_one();
        let finished = timeout(Duration::from_secs(10), &mut task).await;
        if finished.is_err() {
            task.abort();
            let _ = task.await;
            panic!("revoked fragmented source continued into another held backend poll");
        }
        let (delivered, _, mut stream) = finished.unwrap().unwrap();
        assert_eq!(
            delivered,
            fixture.raw[..if case < 2 { 0 } else { CHUNK_SIZE as usize }]
        );
        assert!(stream.next().await.is_none());
        assert_eq!(tail_polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(response_budget.used(), 0);
        assert_eq!(scratch_budget.used(), 0);
    }
}

#[tokio::test]
async fn empty_raw_post_await_revocation_fails_before_another_eof_poll_or_empty_success() {
    for fragment in [Some(Bytes::new()), None] {
        let fixture = Fixture::new_without_publication().await;
        let oid = path_oid(&fixture, "/empty").await;
        fixture.counts.reset();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let tail_polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
            oid,
            kind: bounded_objects::FaultKind::HeldFragment {
                prefix: Bytes::new(),
                fragment,
                entered: entered.clone(),
                release: release.clone(),
                tail_polls: tail_polls.clone(),
                drops: drops.clone(),
            },
        });
        let response_budget = MemoryBudget::new(CHUNK_SIZE as usize);
        let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
        let app = budgeted_app(&fixture, &response_budget, &scratch_budget);
        let request = fixture.request("GET", "blob?path=/empty", Body::empty());
        let mut task = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
        timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        release_lease(&fixture).await;
        release.notify_one();
        let finished = timeout(Duration::from_secs(10), &mut task).await;
        if finished.is_err() {
            task.abort();
            let _ = task.await;
            panic!("revoked empty source continued into another held backend poll");
        }
        error(finished.unwrap().unwrap(), 410, "LEASE_EXPIRED", false).await;
        assert_eq!(tail_polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(response_budget.used(), 0);
        assert_eq!(scratch_budget.used(), 0);
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn raw_physical_size_and_current_fact_fail_before_body_and_path_errors_keep_formal_statuses()
{
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    fixture.counts.reset();
    for (path, status, code) in [
        ("/absent", 404, "PATH_NOT_FOUND"),
        ("/outside", 404, "PATH_NOT_FOUND"),
        ("/directory", 409, "NOT_DIRECTORY"),
        ("/file/child", 409, "NOT_DIRECTORY"),
        ("/link/child", 409, "SYMLINK_TRAVERSAL"),
        ("/../outside", 400, "SCOPE_INVALID"),
    ] {
        error(
            fixture
                .send("GET", &format!("blob?path={path}"), Body::empty())
                .await,
            status,
            code,
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    let mut request = fixture.request("GET", "blob?path=/file", Body::empty());
    request
        .headers_mut()
        .insert("range", "bytes=0-9".parse().unwrap());
    error(
        fixture.app.clone().oneshot(request).await.unwrap(),
        400,
        "RANGE_NOT_SUPPORTED",
        false,
    )
    .await;
    error(
        fixture
            .send(
                "GET",
                &format!("blob?path=/file&expected_digest=sha256:{}", "0".repeat(64)),
                Body::empty(),
            )
            .await,
        409,
        "EXPECTED_DIGEST_MISMATCH",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    *fixture.counts.object_size_override.lock().unwrap() = Some(fixture.raw.len() as i64 + 1);
    error(
        fixture.send("GET", "blob?path=/file", Body::empty()).await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(1, 0);
    *fixture.counts.object_size_override.lock().unwrap() = None;
    let original = fixture.fact().await;
    let mut changed = original.clone();
    changed.created_at += chrono::Duration::seconds(1);
    fixture.replace_fact(changed).await;
    fixture.counts.reset();
    error(
        fixture.send("GET", "blob?path=/file", Body::empty()).await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    fixture.replace_fact(original).await;
    for (path, kind, bytes) in [
        ("/empty", "regular", &b""[..]),
        ("/link", "symlink", &b"file"[..]),
        ("/executable", "executable", fixture.raw.as_slice()),
    ] {
        let response = fixture
            .send("GET", &format!("blob?path={path}"), Body::empty())
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["x-mega-fs-kind"], kind);
        assert_eq!(
            response.headers()["content-length"],
            bytes.len().to_string()
        );
        assert_eq!(
            to_bytes(response.into_body(), 2 * CHUNK_SIZE as usize)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
    }
}

#[tokio::test]
async fn raw_visible_producer_item_cap_rejects_before_copy_hash_or_tail_poll() {
    let raw = vec![7; 8 * 1024 * 1024 + 1];
    let fixture = Fixture::new_with_pg_config_directories_and_objects(
        false,
        0,
        &[("large".into(), raw.clone())],
    )
    .await;
    let oid = path_oid(&fixture, "/large").await;
    *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
        oid: oid.clone(),
        kind: bounded_objects::FaultKind::Parts(
            raw.chunks(CHUNK_SIZE as usize)
                .map(Bytes::copy_from_slice)
                .collect(),
        ),
    });
    fixture.map("/large").await;
    let tails = Arc::new(AtomicUsize::new(0));
    *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
        oid,
        kind: bounded_objects::FaultKind::Oversized(Bytes::from(raw), tails.clone()),
    });
    fixture.counts.reset();
    let response_budget = MemoryBudget::new(2 * CHUNK_SIZE as usize);
    let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
    let response = budgeted_app(&fixture, &response_budget, &scratch_budget)
        .oneshot(fixture.request("GET", "blob?path=/large", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert_eq!(tails.load(Ordering::SeqCst), 0);
    assert_eq!(response_budget.used(), 0);
    assert_eq!(scratch_budget.used(), 0);
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture.counts.bytes.load(Ordering::SeqCst),
        8 * 1024 * 1024 + 1
    );
}

#[tokio::test]
async fn empty_raw_source_requires_physical_zero_exact_eof_and_current_empty_digest() {
    let fixture = Fixture::new().await;
    let oid = path_oid(&fixture, "/empty").await;
    for (kind, status, code) in [
        (
            bounded_objects::FaultKind::Parts(vec![Bytes::from_static(b"growth")]),
            502,
            "INTEGRITY_ERROR",
        ),
        (
            bounded_objects::FaultKind::LateError(Bytes::new()),
            503,
            "OBJECT_UNAVAILABLE",
        ),
    ] {
        *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
            oid: oid.clone(),
            kind,
        });
        fixture.counts.reset();
        error(
            fixture.send("GET", "blob?path=/empty", Body::empty()).await,
            status,
            code,
            false,
        )
        .await;
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    }
    *fixture.counts.object_fault.lock().unwrap() = None;
    fixture.counts.reset();
    let mono = fixture.state.storage.mono_storage();
    let original = mono
        .get_verified_blobs(vec![oid.clone()])
        .await
        .unwrap()
        .remove(&oid)
        .unwrap();
    let mut changed = original.clone().into_active_model();
    changed.raw_sha256 = Set(vec![9; 32]);
    changed.update(mono.get_connection()).await.unwrap();
    error(
        fixture.send("GET", "blob?path=/empty", Body::empty()).await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    original
        .into_active_model()
        .reset_all()
        .update(mono.get_connection())
        .await
        .unwrap();
    let response = fixture.send("GET", "blob?path=/empty", Body::empty()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-length"], "0");
    assert!(to_bytes(response.into_body(), 1).await.unwrap().is_empty());
    fixture.counts.assert(1, 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
}
