use sea_orm::{DatabaseConnection, DbBackend, Statement};
use tokio::{sync::Notify, time::timeout};

use super::*;
use crate::{
    ceres::snapshot::{
        chunks::{ChunkMapSource, VerifiedSourceChunkMap},
        error::SnapshotErrorCode,
    },
    jupiter::storage::{
        native_chunk_map::PostgresChunkMapRepository, object_storage::mock_object_storage,
    },
};

fn statement<const N: usize>(sql: &str, values: [sea_orm::Value; N]) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn router(state: &MonoApiServiceState) -> Router {
    Router::new().nest("/api/v2", routers(state.clone()).with_state(state.clone()))
}

fn budgeted_chunks_app(
    fixture: &Fixture,
    response_budget: &Arc<crate::ceres::snapshot::content_budget::MemoryBudget>,
    scratch_budget: &Arc<crate::ceres::snapshot::content_budget::MemoryBudget>,
) -> Router {
    use axum::{
        extract::{Path as AxumPath, State},
        routing::post,
    };

    use crate::api::router::snapshot_router::{
        content, request::Mst2Bytes, snapshot_auth_middleware,
    };

    let response_budget = response_budget.clone();
    let scratch_budget = scratch_budget.clone();
    Router::new()
        .route(
            "/api/v2/snapshots/{snapshot_id}/chunks",
            post(
                move |state: State<MonoApiServiceState>,
                      path: AxumPath<String>,
                      body: Mst2Bytes| {
                    content::chunks_with_budgets(
                        state,
                        path,
                        body,
                        content::ChunksBudgets {
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

async fn count(db: &DatabaseConnection, table: &str) -> i64 {
    db.query_one_raw(statement(
        &format!("SELECT count(*) AS count FROM {table}"),
        [],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "count")
    .unwrap()
}

async fn wait_count(db: &DatabaseConnection, table: &str, expected: i64) {
    timeout(Duration::from_secs(10), async {
        loop {
            if count(db, table).await == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("actual durable owner release did not complete");
}

async fn age_unowned_candidates(db: &DatabaseConnection) {
    db.execute_unprepared("UPDATE mst2_chunk_receipt_generation SET last_progress=pg_catalog.clock_timestamp()-interval '2 hours' WHERE state='LIVE'; UPDATE mst2_chunk_map_lifetime SET last_used=pg_catalog.clock_timestamp()-interval '2 hours' WHERE state='LIVE'").await.unwrap();
}

async fn oid_for(fixture: &Fixture, path: &str) -> String {
    let handler = MonoApiService::from(&fixture.state);
    let main = fixture
        .state
        .storage
        .mono_storage()
        .get_main_ref("/project")
        .await
        .unwrap()
        .unwrap();
    let tree = handler.get_tree_by_hash(&main.ref_tree_hash).await.unwrap();
    match resolve_abs_metadata(&handler, &tree, path).await.unwrap() {
        MetadataWalkOutcome::FoundFile { oid, .. } => oid,
        other => panic!("retention fixture did not resolve: {other:?}"),
    }
}

#[tokio::test]
async fn shared_map_survives_other_source_retirement_while_actual_reader_is_live() {
    let fixture =
        Fixture::new_in_metadata_family(false, 0, &[("other".to_string(), vec![19; 4096])], true)
            .await;
    let original = fixture.map("/file").await;
    let other = oid_for(&fixture, "/other").await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    // This G fixture isolates independent source receipts without changing a
    // Q-certified file tuple. Two exact facts consume the same actual raw
    // bytes independently; map sharing never admits the second source.
    db.execute_raw(statement(
        "UPDATE mst2_verified_object SET size=$1,raw_sha256=$2 WHERE git_oid=$3",
        [
            (fixture.raw.len() as i64).into(),
            fixture.digest.to_vec().into(),
            other.clone().into(),
        ],
    ))
    .await
    .unwrap();
    *fixture.counts.object_size_override.lock().unwrap() = Some(fixture.raw.len() as i64);
    *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
        oid: other.clone(),
        kind: bounded_objects::FaultKind::Parts(
            fixture
                .raw
                .chunks(CHUNK_SIZE as usize)
                .map(Bytes::copy_from_slice)
                .collect(),
        ),
    });
    fixture.counts.reset();
    assert_eq!(fixture.map("/other").await["map"], original["map"]);
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
    wait_count(db, "mst2_chunk_reader", 0).await;
    assert_eq!(count(db, "mst2_chunk_map_source").await, 2);
    assert_eq!(count(db, "mst2_chunk_map").await, 1);
    assert_eq!(count(db, "mst2_chunk_map_lifetime").await, 1);
    let fact = fixture
        .state
        .storage
        .mono_storage()
        .get_verified_blobs(vec![other.clone()])
        .await
        .unwrap()
        .remove(&other)
        .unwrap();
    let source = ChunkMapSource::from_fact(fact, &other).unwrap();
    let repository = fixture.state.storage.chunk_maps().await.unwrap();
    let objects = &fixture.state.storage.git_service.obj_storage;
    let reader = repository.read(&source, objects).await.unwrap().unwrap();
    age_unowned_candidates(db).await;
    repository.maintain(objects, 64).await.unwrap();
    assert_eq!(count(db, "mst2_chunk_map_source").await, 1);
    assert_eq!(count(db, "mst2_chunk_map").await, 1);
    assert_eq!(count(db, "mst2_chunk_map_leaf").await, 1);
    assert_eq!(count(db, "mst2_chunk_map_node").await, 1);
    assert_eq!(fixture.counts.receipt_deletes.load(Ordering::SeqCst), 1);
    repository
        .selected_page(&reader, 0)
        .await
        .unwrap()
        .verify_chunk(&reader.map, 0, &fixture.raw[..CHUNK_SIZE as usize])
        .unwrap();
    drop(reader);
    wait_count(db, "mst2_chunk_reader", 0).await;
    age_unowned_candidates(db).await;
    repository.maintain(objects, 64).await.unwrap();
    assert_eq!(count(db, "mst2_chunk_map_source").await, 0);
    assert_eq!(count(db, "mst2_chunk_map").await, 0);
    assert_eq!(fixture.counts.receipt_deletes.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn pending_receipt_is_replayed_before_another_aged_live_victim() {
    let fixture = Fixture::new_with_pg_config_directories_and_objects(
        false,
        0,
        &[("other".to_string(), vec![19; 4096])],
    )
    .await;
    fixture.map("/file").await;
    let other = fixture.map("/other").await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    wait_count(db, "mst2_chunk_reader", 0).await;
    age_unowned_candidates(db).await;
    db.execute_raw(statement(
        "UPDATE mst2_chunk_receipt_generation g SET state='DELETING' WHERE EXISTS(SELECT 1 FROM mst2_chunk_map_source s WHERE s.receipt_key=g.receipt_key AND s.git_oid=$1)",
        [fixture.oid.clone().into()],
    ))
    .await
    .unwrap();
    let repository = fixture.state.storage.chunk_maps().await.unwrap();
    repository
        .maintain(&fixture.state.storage.git_service.obj_storage, 1)
        .await
        .unwrap();
    assert_eq!(count(db, "mst2_chunk_map_source").await, 1);
    assert_eq!(count(db, "mst2_chunk_map").await, 1);
    assert_eq!(fixture.counts.receipt_deletes.load(Ordering::SeqCst), 1);
    fixture.counts.reset();
    assert_eq!(fixture.map("/other").await["map"], other["map"]);
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn actual_http_replays_a_retiring_source_row_before_rebuilding_its_next_generation() {
    let fixture = Fixture::new().await;
    let original = fixture.map("/file").await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    wait_count(db, "mst2_chunk_reader", 0).await;
    let old_key: String = db
        .query_one_raw(statement(
            "SELECT receipt_key FROM mst2_chunk_map_source",
            [],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "receipt_key")
        .unwrap();
    db.execute_unprepared(
        "UPDATE mst2_chunk_receipt_generation SET state='DELETING' WHERE state='LIVE'",
    )
    .await
    .unwrap();
    // The source row is deliberately still present; inline replay cannot
    // depend on the source-index delete having already finished.
    assert_eq!(count(db, "mst2_chunk_map_source").await, 1);
    fixture.counts.reset();
    assert_eq!(fixture.map("/file").await["map"], original["map"]);
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(fixture.counts.receipt_deletes.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
    let new_key: String = db
        .query_one_raw(statement(
            "SELECT receipt_key FROM mst2_chunk_map_source",
            [],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "receipt_key")
        .unwrap();
    assert_ne!(old_key, new_key);
    assert_eq!(count(db, "mst2_chunk_map").await, 1);
    fixture.counts.reset();
    fixture.map("/alias").await;
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn actual_warm_body_owner_cancels_never_returning_open_and_next_without_backend_release() {
    use crate::ceres::snapshot::content_budget::{MemoryBudget, RANGE_WORK_BYTES};

    for raw_route in [true, false] {
        for stage in 0..3 {
            let held_open = stage == 0;
            let complete_without_eof = stage == 2;
            let fixture = Fixture::new().await;
            let map = fixture.map("/file").await;
            let mono = fixture.state.storage.mono_storage();
            let db = mono.get_connection();
            wait_count(db, "mst2_chunk_reader", 0).await;
            fixture.counts.reset();
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let drops = Arc::new(AtomicUsize::new(0));
            let tail_polls = Arc::new(AtomicUsize::new(0));
            if held_open {
                let holds = Some((entered.clone(), release.clone(), drops.clone()));
                if raw_route {
                    *fixture.counts.whole_open_holds.lock().unwrap() = holds;
                } else {
                    *fixture.counts.range_open_holds.lock().unwrap() = holds;
                }
            } else {
                *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
                    oid: fixture.oid.clone(),
                    kind: bounded_objects::FaultKind::HeldFragment {
                        prefix: if complete_without_eof {
                            Bytes::copy_from_slice(if raw_route {
                                &fixture.raw
                            } else {
                                &fixture.raw[..CHUNK_SIZE as usize]
                            })
                        } else {
                            Bytes::new()
                        },
                        fragment: None,
                        entered: entered.clone(),
                        release: release.clone(),
                        drops: drops.clone(),
                        tail_polls: tail_polls.clone(),
                    },
                });
            }
            let response_bytes = if raw_route {
                CHUNK_SIZE as usize
            } else {
                CHUNK_SIZE as usize + 2048
            };
            let response_budget = MemoryBudget::new(response_bytes);
            let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
            let app = if raw_route {
                super::raw_blob::budgeted_app(&fixture, &response_budget, &scratch_budget)
            } else {
                budgeted_chunks_app(&fixture, &response_budget, &scratch_budget)
            };
            let request = if raw_route {
                fixture.request("GET", "blob?path=/file", Body::empty())
            } else {
                fixture.request(
                    "POST",
                    "chunks",
                    Body::from(
                        fixture
                            .chunk_body("/file", map["map"]["map_id"].as_str().unwrap(), "0")
                            .to_string(),
                    ),
                )
            };
            let expected_raw = fixture.raw.clone();
            let mut task = tokio::spawn(async move {
                let response = app.oneshot(request).await.unwrap();
                if raw_route && !held_open {
                    assert_eq!(response.status(), 200);
                    let mut body = response.into_body().into_data_stream();
                    if complete_without_eof {
                        let first = body.next().await.unwrap().unwrap();
                        assert_eq!(first.as_ref(), &expected_raw[..CHUNK_SIZE as usize]);
                        drop(first);
                    }
                    assert!(body.next().await.unwrap().is_err());
                    assert!(body.next().await.is_none());
                } else {
                    error(response, 410, "LEASE_EXPIRED", false).await;
                }
            });
            timeout(Duration::from_secs(10), entered.notified())
                .await
                .unwrap();
            assert_eq!(count(db, "mst2_chunk_reader").await, 1);
            assert_eq!(scratch_budget.used(), RANGE_WORK_BYTES);
            assert_eq!(
                response_budget.used(),
                if raw_route && complete_without_eof {
                    fixture.raw.len() - CHUNK_SIZE as usize
                } else {
                    response_bytes
                }
            );
            db.execute_unprepared("UPDATE mst2_chunk_reader SET deadline=pg_catalog.clock_timestamp()+interval '20 milliseconds'").await.unwrap();
            // Never release the backend and never cancel the caller. The
            // request must observe actual expiry during its pending await.
            let finished = timeout(Duration::from_secs(12), &mut task).await;
            if finished.is_err() {
                task.abort();
                let _ = task.await;
                panic!("actual body owner did not cancel its permanently held backend");
            }
            finished.unwrap().unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(tail_polls.load(Ordering::SeqCst), 0);
            let consumed = if complete_without_eof {
                if raw_route {
                    fixture.raw.len()
                } else {
                    CHUNK_SIZE as usize
                }
            } else {
                0
            };
            assert_eq!(fixture.counts.bytes.load(Ordering::SeqCst), consumed);
            assert_eq!(scratch_budget.used(), 0);
            assert_eq!(response_budget.used(), 0);
            if raw_route {
                fixture.counts.assert(1, consumed);
            } else {
                assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
                assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 1);
            }
            wait_count(db, "mst2_chunk_reader", 0).await;
            assert_eq!(count(db, "mst2_chunk_map_source").await, 1);
            drop(release);
        }
    }
}

#[tokio::test]
async fn actual_cold_builder_uses_remaining_database_deadline_for_open_and_next_and_refunds_credit()
{
    use crate::ceres::snapshot::content_budget::MemoryBudget;

    for stage in 0..3 {
        let held_open = stage == 0;
        let complete_without_eof = stage == 2;
        let fixture = Fixture::new().await;
        let repository = fixture.state.storage.chunk_maps().await.unwrap();
        let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
        let admission = repository
            .admit_install(&source, &fixture.state.storage.git_service.obj_storage)
            .await
            .unwrap();
        let mono = fixture.state.storage.mono_storage();
        let db = mono.get_connection();
        db.execute_unprepared("UPDATE mst2_chunk_receipt_generation SET deadline=pg_catalog.clock_timestamp()+interval '3 seconds' WHERE state='RESERVED'").await.unwrap();
        admission.test_check_next_owner_operation().await;
        fixture.counts.reset();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let tail_polls = Arc::new(AtomicUsize::new(0));
        if held_open {
            *fixture.counts.whole_open_holds.lock().unwrap() =
                Some((entered.clone(), release.clone(), drops.clone()));
        } else {
            *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
                oid: fixture.oid.clone(),
                kind: bounded_objects::FaultKind::HeldFragment {
                    prefix: if complete_without_eof {
                        Bytes::copy_from_slice(&fixture.raw)
                    } else {
                        Bytes::new()
                    },
                    fragment: None,
                    entered: entered.clone(),
                    release: release.clone(),
                    drops: drops.clone(),
                    tail_polls: tail_polls.clone(),
                },
            });
        }
        let budget = MemoryBudget::new(512 * 1024 * 1024);
        let task_budget = budget.clone();
        let handler = MonoApiService::from(&fixture.state);
        let mut task = tokio::spawn(async move {
            VerifiedSourceChunkMap::verify(&handler, source, &task_budget, &admission).await
        });
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        assert!(budget.used() > 0);
        let result = timeout(Duration::from_secs(5), &mut task).await;
        if result.is_err() {
            task.abort();
            let _ = task.await;
            panic!("cold source ignored its actual remaining database deadline");
        }
        let result = result.unwrap().unwrap();
        let error = match result {
            Ok(_) => panic!("incomplete cold source produced trust"),
            Err(error) => error,
        };
        assert_eq!(error.code, SnapshotErrorCode::LeaseExpired);
        assert_eq!(budget.used(), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(tail_polls.load(Ordering::SeqCst), 0);
        fixture.counts.assert(
            1,
            if complete_without_eof {
                fixture.raw.len()
            } else {
                0
            },
        );
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
        assert_eq!(count(db, "mst2_chunk_map").await, 0);
        assert_eq!(count(db, "mst2_chunk_map_source").await, 0);
        drop(release);
    }
}

#[tokio::test]
async fn actual_receipt_create_and_read_waits_cancel_without_backend_release_or_source_publication()
{
    use crate::ceres::snapshot::content_budget::MemoryBudget;

    for creating in [true, false] {
        let fixture = Fixture::new().await;
        let repository = fixture.state.storage.chunk_maps().await.unwrap();
        let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
        let objects = fixture.state.storage.git_service.obj_storage.clone();
        let admission = repository.admit_install(&source, &objects).await.unwrap();
        let budget = MemoryBudget::new(512 * 1024 * 1024);
        let handler = MonoApiService::from(&fixture.state);
        let verified = VerifiedSourceChunkMap::verify(&handler, source, &budget, &admission)
            .await
            .unwrap();
        assert!(budget.used() > 0);
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let drops = if creating {
            *fixture.counts.receipt_write_holds.lock().unwrap() =
                Some((entered.clone(), release.clone()));
            fixture.counts.receipt_write_wait_drops.clone()
        } else {
            let drops = Arc::new(AtomicUsize::new(0));
            *fixture.counts.receipt_open_holds.lock().unwrap() =
                Some((entered.clone(), release.clone(), drops.clone()));
            drops
        };
        let task_storage = fixture.state.storage.clone();
        let mut task = tokio::spawn(async move {
            let repository = task_storage.chunk_maps().await.unwrap();
            repository.install(verified, &objects, &admission).await
        });
        timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        let mono = fixture.state.storage.mono_storage();
        let db = mono.get_connection();
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
        if creating {
            db.execute_unprepared("UPDATE mst2_chunk_receipt_generation SET deadline=pg_catalog.clock_timestamp()+interval '20 milliseconds' WHERE state='CREATING'").await.unwrap();
        }
        // Atomic create uses the actual owner expiry; receipt input also keeps
        // its stricter five-second cap. Neither wait needs backend release.
        let finished = timeout(
            Duration::from_secs(if creating { 12 } else { 7 }),
            &mut task,
        )
        .await;
        if finished.is_err() {
            task.abort();
            let _ = task.await;
            panic!("receipt wait retained its install workspace indefinitely");
        }
        let error = finished.unwrap().unwrap().unwrap_err();
        assert_eq!(
            error.code,
            if creating {
                SnapshotErrorCode::LeaseExpired
            } else {
                SnapshotErrorCode::TemporaryUnavailable
            }
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(budget.used(), 0);
        fixture.counts.assert(1, fixture.raw.len());
        assert_eq!(
            fixture.counts.receipt_reads.load(Ordering::SeqCst),
            usize::from(!creating)
        );
        assert_eq!(count(db, "mst2_chunk_reader").await, 0);
        assert_eq!(count(db, "mst2_chunk_map_source").await, 0);
        assert_eq!(count(db, "mst2_chunk_map").await, 0);
        *fixture.counts.receipt_open_holds.lock().unwrap() = None;
        let repository = fixture.state.storage.chunk_maps().await.unwrap();
        repository
            .maintain(&fixture.state.storage.git_service.obj_storage, 64)
            .await
            .unwrap();
        assert_eq!(fixture.counts.receipt_deletes.load(Ordering::SeqCst), 1);
        drop(release);
    }
}

#[tokio::test]
async fn held_raw_and_exact_range_do_not_resume_after_actual_reader_expiry() {
    for raw_route in [true, false] {
        let fixture = Fixture::new().await;
        let map = fixture.map("/file").await;
        let mono = fixture.state.storage.mono_storage();
        let db = mono.get_connection();
        wait_count(db, "mst2_chunk_reader", 0).await;
        fixture.counts.reset();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let tail_polls = Arc::new(AtomicUsize::new(0));
        *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
            oid: fixture.oid.clone(),
            kind: bounded_objects::FaultKind::HeldFragment {
                prefix: Bytes::new(),
                fragment: Some(Bytes::new()),
                entered: entered.clone(),
                release: release.clone(),
                tail_polls: tail_polls.clone(),
                drops: drops.clone(),
            },
        });
        let app = fixture.app.clone();
        let request = if raw_route {
            fixture.request("GET", "blob?path=/file", Body::empty())
        } else {
            fixture.request(
                "POST",
                "chunks",
                Body::from(
                    fixture
                        .chunk_body("/file", map["map"]["map_id"].as_str().unwrap(), "0")
                        .to_string(),
                ),
            )
        };
        let mut task = tokio::spawn(async move {
            let response = app.oneshot(request).await.unwrap();
            if raw_route {
                assert_eq!(response.status(), 200);
                let mut body = response.into_body().into_data_stream();
                assert!(body.next().await.unwrap().is_err());
                assert!(body.next().await.is_none());
            } else {
                error(response, 410, "LEASE_EXPIRED", false).await;
            }
        });
        timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        assert_eq!(count(db, "mst2_chunk_reader").await, 1);
        db.execute_unprepared("UPDATE mst2_chunk_reader SET deadline=pg_catalog.clock_timestamp()+interval '20 milliseconds'").await.unwrap();
        // Exercise the real request-held reader without reaching into it:
        // let the existing ten-second local check interval elapse too.
        tokio::time::sleep(Duration::from_secs(11)).await;
        release.notify_one();
        let completed = timeout(Duration::from_secs(5), &mut task).await;
        if completed.is_err() {
            task.abort();
            let _ = task.await;
            panic!("expired reader continued into another held backend poll");
        }
        completed.unwrap().unwrap();
        assert_eq!(tail_polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.counts.bytes.load(Ordering::SeqCst), 0);
        wait_count(db, "mst2_chunk_reader", 0).await;
        assert_eq!(count(db, "mst2_chunk_map_source").await, 1);
    }
}

#[tokio::test]
async fn production_q_bootstrap_after_retention_preserves_catalog_and_reconstructed_warm_routes() {
    use crate::jupiter::storage::qualified_metadata_family::{
        SnapshotMetadataFamily, provision_or_verify_rooted_qualified_family,
    };

    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let namespace = provision_or_verify_rooted_qualified_family(db)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .state
            .storage
            .snapshot_metadata_family(&fixture.lease, true)
            .await
            .unwrap(),
        Some(SnapshotMetadataFamily::Rooted)
    );
    let applied: i64 = db
        .query_one_raw(statement(
            "SELECT count(*) AS count FROM seaql_migrations WHERE version='m20261008_000300_add_mst2_chunk_map_retention'",
            [],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "count")
        .unwrap();
    assert_eq!(applied, 1);
    let original = fixture.map("/file").await;
    fixture.counts.assert(1, fixture.raw.len());
    wait_count(db, "mst2_chunk_reader", 0).await;

    // This is the production connection path: it reapplies migration checks
    // and verifies the complete existing Q authority catalog without a test
    // exemption or a refreshed registration fingerprint.
    let config = fixture.state.storage.config();
    let connection = crate::jupiter::storage::init::database_connection(&config.database)
        .await
        .unwrap();
    assert_eq!(
        provision_or_verify_rooted_qualified_family(&connection)
            .await
            .unwrap(),
        namespace
    );
    let storage = crate::jupiter::storage::Storage::new_with_connection(
        config,
        Arc::new(connection),
        fixture.state.storage.git_service.obj_storage.clone(),
    )
    .await
    .unwrap();
    let state = MonoApiServiceState {
        storage,
        ..fixture.state.clone()
    };
    assert_eq!(
        state
            .storage
            .snapshot_metadata_family(&fixture.lease, true)
            .await
            .unwrap(),
        Some(SnapshotMetadataFamily::Rooted)
    );
    let app = router(&state);
    fixture.counts.reset();
    let warm = success_json(
        app.clone()
            .oneshot(fixture.request("GET", "chunk-map?path=/alias", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(warm["map"], original["map"]);
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);

    fixture.counts.reset();
    let response = app
        .oneshot(
            fixture.request(
                "POST",
                "chunks",
                Body::from(
                    fixture
                        .chunk_body("/alias", original["map"]["map_id"].as_str().unwrap(), "0")
                        .to_string(),
                ),
            ),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let wire = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let frames = parse_stream(&wire).unwrap();
    let [Frame::Chunk(chunk), Frame::End(end)] = frames.as_slice() else {
        panic!("reconstructed rooted route must emit CHUNK and terminal END");
    };
    assert_eq!(chunk.chunk_bytes, &fixture.raw[..CHUNK_SIZE as usize]);
    assert_eq!(chunk.file_content_id, fixture.digest);
    assert_eq!(chunk.chunk_index, 0);
    assert_eq!(end.request_item_count, 1);
    assert_eq!(end.logical_bytes, u64::from(CHUNK_SIZE));
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.counts.bytes.load(Ordering::SeqCst),
        CHUNK_SIZE as usize
    );
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    drop(wire);
    wait_count(db, "mst2_chunk_reader", 0).await;
    assert_eq!(count(db, "mst2_chunk_map_source").await, 1);
    assert_eq!(count(db, "mst2_chunk_map").await, 1);
}

#[tokio::test]
async fn one_connection_install_commits_bounded_stages_and_reconstructed_warm_reads() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mut config = fixture.state.storage.config().database.clone();
    config.max_connection = 1;
    config.min_connection = 1;
    let connection = crate::jupiter::storage::init::database_connection(&config)
        .await
        .unwrap();
    let repository = PostgresChunkMapRepository::new(connection.clone())
        .await
        .unwrap();
    assert!(
        fixture
            .state
            .storage
            .native_chunk_maps
            .set(repository)
            .is_ok()
    );
    connection.execute_unprepared("CREATE TABLE chunk_stage_audit(relation text NOT NULL, writer_xid bigint NOT NULL); CREATE FUNCTION chunk_stage_audit() RETURNS trigger LANGUAGE plpgsql AS $audit$ BEGIN INSERT INTO chunk_stage_audit VALUES(TG_TABLE_NAME,pg_catalog.txid_current()); RETURN NEW; END $audit$; CREATE TRIGGER chunk_stage_audit AFTER INSERT ON mst2_chunk_map FOR EACH ROW EXECUTE FUNCTION chunk_stage_audit(); CREATE TRIGGER chunk_stage_audit AFTER INSERT ON mst2_chunk_map_leaf FOR EACH ROW EXECUTE FUNCTION chunk_stage_audit(); CREATE TRIGGER chunk_stage_audit AFTER INSERT ON mst2_chunk_map_node FOR EACH ROW EXECUTE FUNCTION chunk_stage_audit(); CREATE TRIGGER chunk_stage_audit AFTER INSERT ON mst2_chunk_map_source FOR EACH ROW EXECUTE FUNCTION chunk_stage_audit()").await.unwrap();
    let map = timeout(Duration::from_secs(10), fixture.map("/file"))
        .await
        .expect("single-connection staging waited for its own held connection");
    fixture.counts.assert(1, fixture.raw.len());
    let transactions: i64 = connection
        .query_one_raw(statement(
            "SELECT count(DISTINCT writer_xid) AS count FROM chunk_stage_audit",
            [],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "count")
        .unwrap();
    assert!(transactions >= 4);
    for table in [
        "mst2_chunk_map",
        "mst2_chunk_map_leaf",
        "mst2_chunk_map_node",
        "mst2_chunk_map_source",
    ] {
        assert_eq!(count(&connection, table).await, 1);
    }
    fixture.counts.reset();
    let reconstructed = PostgresChunkMapRepository::new(connection.clone())
        .await
        .unwrap();
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    let persisted = timeout(
        Duration::from_secs(10),
        reconstructed.read(&source, &fixture.state.storage.git_service.obj_storage),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(
        format!("sha256:{}", hex_of(&persisted.map_id)),
        map["map"]["map_id"]
    );
    let page = reconstructed.selected_page(&persisted, 0).await.unwrap();
    page.verify_chunk(&persisted.map, 0, &fixture.raw[..CHUNK_SIZE as usize])
        .unwrap();
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cached_inventory_is_coalesced_and_bound_to_the_actual_backend_arc() {
    let fixture = Fixture::new().await;
    let repository = fixture.state.storage.chunk_maps().await.unwrap();
    let objects = &fixture.state.storage.git_service.obj_storage;
    repository.maintain(objects, 8).await.unwrap();
    let first = fixture
        .counts
        .receipt_inventory_calls
        .load(Ordering::SeqCst);
    repository.maintain(objects, 8).await.unwrap();
    assert_eq!(
        fixture
            .counts
            .receipt_inventory_calls
            .load(Ordering::SeqCst),
        first
    );
    let counts = Arc::new(ReadCounts::default());
    counts
        .receipt_retention_unsupported
        .store(true, Ordering::SeqCst);
    let other = MegaObjectStorageWrapper::new(Arc::new(CountingStorage {
        inner: objects.clone(),
        counts: counts.clone(),
    }));
    let error = repository.maintain(&other, 8).await.unwrap_err();
    assert_eq!(error.code, SnapshotErrorCode::TemporaryUnavailable);
    assert_eq!(counts.receipt_inventory_calls.load(Ordering::SeqCst), 1);
    fixture.counts.assert(0, 0);
    assert_eq!(
        count(
            fixture.state.storage.mono_storage().get_connection(),
            "mst2_chunk_receipt_generation"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn completion_during_real_inventory_keeps_backing_credit_after_pending_install_disappears() {
    let mut fixture = Fixture::new().await;
    let backing = mock_object_storage();
    backing
        .inner
        .put_stream(
            &ObjectKey {
                namespace: ObjectNamespace::Git,
                key: fixture.oid.clone(),
            },
            Box::pin(futures::stream::iter([Ok(Bytes::copy_from_slice(
                &fixture.raw,
            ))])),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
    fixture.state.storage.git_service = GitService {
        obj_storage: MegaObjectStorageWrapper::new(Arc::new(CountingStorage {
            inner: backing.clone(),
            counts: fixture.counts.clone(),
        })),
    };
    fixture.app = router(&fixture.state);
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let create_entered = Arc::new(Notify::new());
    let create_release = Arc::new(Notify::new());
    *fixture.counts.receipt_late_create_holds.lock().unwrap() =
        Some((create_entered.clone(), create_release.clone()));
    let app = fixture.app.clone();
    let request = fixture.request("GET", "chunk-map?path=/file", Body::empty());
    let leader = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    timeout(Duration::from_secs(10), create_entered.notified())
        .await
        .unwrap();
    *fixture.counts.receipt_late_create_holds.lock().unwrap() = None;
    for index in 0..crate::orbit_api::object_storage::MAX_CHUNK_MAP_RECEIPTS - 1 {
        backing
            .inner
            .put_metadata_atomic_create(
                &ObjectKey {
                    namespace: ObjectNamespace::ChunkMapReceipt,
                    key: format!("{index:064x}"),
                },
                Bytes::from_static(&[1; 129]),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
    }
    // Synthetic occupants test physical capacity; they confer no source trust.
    let actual = &fixture.state.storage.git_service.obj_storage;
    let inventory_entered = Arc::new(Notify::new());
    let inventory_release = Arc::new(Notify::new());
    let observer_counts = Arc::new(ReadCounts::default());
    *observer_counts.receipt_inventory_holds.lock().unwrap() =
        Some((inventory_entered.clone(), inventory_release.clone()));
    let observer = PostgresChunkMapRepository::new(db.clone()).await.unwrap();
    let observed = MegaObjectStorageWrapper::new(Arc::new(CountingStorage {
        inner: actual.clone(),
        counts: observer_counts,
    }));
    let capacity = tokio::spawn(async move { observer.test_new_install_capacity(&observed).await });
    timeout(Duration::from_secs(30), inventory_entered.notified())
        .await
        .unwrap();
    create_release.notify_one();
    success_json(
        timeout(Duration::from_secs(30), leader)
            .await
            .unwrap()
            .unwrap(),
    )
    .await;
    assert_eq!(count(db, "mst2_chunk_map_source").await, 1);
    let pending:i64=db.query_one_raw(statement("SELECT count(*) AS count FROM mst2_chunk_receipt_generation WHERE state IN ('RESERVED','CREATING')",[])).await.unwrap().unwrap().try_get("","count").unwrap();
    assert_eq!(pending, 0);
    inventory_release.notify_one();
    let error = timeout(Duration::from_secs(10), capacity)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, SnapshotErrorCode::LimitExceeded);
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(
        actual
            .inner
            .chunk_map_receipt_inventory()
            .await
            .unwrap()
            .objects
            .len(),
        crate::orbit_api::object_storage::MAX_CHUNK_MAP_RECEIPTS
    );
}

#[tokio::test]
async fn json_chunk_and_raw_last_transport_clones_block_actual_collection() {
    use crate::ceres::snapshot::content_budget::{MemoryBudget, RANGE_WORK_BYTES};

    for route in 0..3 {
        let fixture = Fixture::new().await;
        let map = fixture.map("/file").await;
        let repository = fixture.state.storage.chunk_maps().await.unwrap();
        let mono = fixture.state.storage.mono_storage();
        let db = mono.get_connection();
        wait_count(db, "mst2_chunk_reader", 0).await;
        fixture.counts.reset();
        let response_budget = MemoryBudget::new(CHUNK_SIZE as usize + 2048);
        let scratch_budget = MemoryBudget::new(RANGE_WORK_BYTES);
        let response = match route {
            0 => {
                fixture
                    .send("GET", "chunk-map?path=/file", Body::empty())
                    .await
            }
            1 => budgeted_chunks_app(&fixture, &response_budget, &scratch_budget)
                .oneshot(
                    fixture.request(
                        "POST",
                        "chunks",
                        Body::from(
                            json!({"items":[{
                "path":"/file", "expected_digest":fixture.digest_string(),
                "map_id":map["map"]["map_id"], "chunk_index":"0"
            }],"encoding":"identity"})
                            .to_string(),
                        ),
                    ),
                )
                .await
                .unwrap(),
            _ => super::raw_blob::budgeted_app(&fixture, &response_budget, &scratch_budget)
                .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
                .await
                .unwrap(),
        };
        assert_eq!(response.status(), 200);
        let mut body = response.into_body().into_data_stream();
        let mut wire = Vec::new();
        let mut transport = Vec::new();
        while let Some(frame) = body.next().await {
            let bytes = frame.unwrap();
            wire.extend_from_slice(&bytes);
            transport.push(bytes.clone());
        }
        drop(body);
        if route == 2 {
            assert_eq!(wire, fixture.raw);
        }
        if route == 1 {
            let frames = parse_stream(&wire).unwrap();
            assert!(frames.iter().any(|frame| matches!(frame, Frame::End(_))));
        }
        assert!(!transport.is_empty());
        let last = transport.pop().unwrap();
        let last_transport = last.clone();
        drop(last);
        drop(transport);
        assert_eq!(scratch_budget.used(), 0);
        if route == 1 {
            assert_eq!(response_budget.used(), CHUNK_SIZE as usize + 2048);
        } else if route == 2 {
            assert_eq!(
                response_budget.used(),
                fixture.raw.len() - CHUNK_SIZE as usize
            );
        }
        assert_eq!(count(db, "mst2_chunk_reader").await, 1);
        age_unowned_candidates(db).await;
        repository
            .maintain(&fixture.state.storage.git_service.obj_storage, 64)
            .await
            .unwrap();
        assert_eq!(count(db, "mst2_chunk_map_source").await, 1);
        assert_eq!(count(db, "mst2_chunk_map").await, 1);
        assert_eq!(fixture.counts.receipt_deletes.load(Ordering::SeqCst), 0);
        drop(last_transport);
        assert_eq!(response_budget.used(), 0);
        wait_count(db, "mst2_chunk_reader", 0).await;
        repository
            .maintain(&fixture.state.storage.git_service.obj_storage, 64)
            .await
            .unwrap();
        for table in [
            "mst2_chunk_map_source",
            "mst2_chunk_map",
            "mst2_chunk_map_leaf",
            "mst2_chunk_map_node",
            "mst2_chunk_map_lifetime",
        ] {
            assert_eq!(count(db, table).await, 0);
        }
        assert_eq!(fixture.counts.receipt_deletes.load(Ordering::SeqCst), 1);
        assert_eq!(count(db, "mst2_chunk_receipt_generation").await, 1);
        assert_eq!(count(db, "mst2_chunk_map_gc").await, 1);
    }
}

#[tokio::test]
async fn expired_reader_arc_cannot_resurrect_or_authenticate_a_page() {
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    wait_count(db, "mst2_chunk_reader", 0).await;
    let repository = fixture.state.storage.chunk_maps().await.unwrap();
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    let map = repository
        .read(&source, &fixture.state.storage.git_service.obj_storage)
        .await
        .unwrap()
        .unwrap();
    let old_generation: i64 = db
        .query_one_raw(statement(
            "SELECT generation FROM mst2_chunk_map_lifetime WHERE map_id=$1",
            [map.map_id.to_vec().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "generation")
        .unwrap();
    db.execute_unprepared("UPDATE mst2_chunk_reader SET deadline=pg_catalog.clock_timestamp()+interval '20 milliseconds'").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    map.test_check_next_owner_operation().await;
    assert_eq!(
        map.ensure_live().await.unwrap_err().code,
        SnapshotErrorCode::LeaseExpired
    );
    assert_eq!(
        map.record_progress().await.unwrap_err().code,
        SnapshotErrorCode::LeaseExpired
    );
    assert_eq!(
        repository.selected_page(&map, 0).await.err().unwrap().code,
        SnapshotErrorCode::LeaseExpired
    );
    assert!(db.execute_unprepared("UPDATE mst2_chunk_reader SET deadline=pg_catalog.clock_timestamp()+interval '59 seconds'").await.is_err());
    age_unowned_candidates(db).await;
    repository
        .maintain(&fixture.state.storage.git_service.obj_storage, 64)
        .await
        .unwrap();
    assert_eq!(count(db, "mst2_chunk_map").await, 0);
    assert_eq!(
        map.ensure_live().await.unwrap_err().code,
        SnapshotErrorCode::LeaseExpired
    );
    fixture.counts.reset();
    assert_eq!(
        fixture.map("/file").await["map"]["file_size"],
        fixture.raw.len().to_string()
    );
    fixture.counts.assert(1, fixture.raw.len());
    let new_generation: i64 = db
        .query_one_raw(statement(
            "SELECT generation FROM mst2_chunk_map_lifetime WHERE map_id=$1",
            [map.map_id.to_vec().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "generation")
        .unwrap();
    assert!(new_generation > old_generation);
    assert!(
        db.execute_raw(statement(
            "UPDATE mst2_chunk_map_lifetime SET state='DELETING' WHERE map_id=$1",
            [map.map_id.to_vec().into()]
        ))
        .await
        .is_err()
    );
    assert!(
        db.execute_raw(statement(
            "DELETE FROM mst2_chunk_map_leaf WHERE map_id=$1",
            [map.map_id.to_vec().into()]
        ))
        .await
        .is_err()
    );
    fixture.counts.reset();
    fixture.map("/alias").await;
    fixture.counts.assert(0, 0);
    assert_eq!(
        map.ensure_live().await.unwrap_err().code,
        SnapshotErrorCode::LeaseExpired
    );
}

#[tokio::test]
async fn late_cancelled_create_is_reserved_and_old_key_replay_preserves_new_generation() {
    let fixture = Fixture::new().await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    *fixture.counts.receipt_late_create_holds.lock().unwrap() =
        Some((entered.clone(), release.clone()));
    let app = fixture.app.clone();
    let request = fixture.request("GET", "chunk-map?path=/file", Body::empty());
    let cancelled = tokio::spawn(async move { app.oneshot(request).await });
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    let old_key: String = db
        .query_one_raw(statement(
            "SELECT receipt_key FROM mst2_chunk_receipt_generation",
            [],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "receipt_key")
        .unwrap();
    assert!(
        !fixture
            .state
            .storage
            .git_service
            .obj_storage
            .inner
            .exists(&ObjectKey {
                namespace: ObjectNamespace::ChunkMapReceipt,
                key: old_key.clone()
            })
            .await
            .unwrap()
    );
    cancelled.abort();
    assert!(cancelled.await.err().unwrap().is_cancelled());
    *fixture.counts.receipt_late_create_holds.lock().unwrap() = None;
    let current = fixture.map("/file").await;
    let repository = fixture.state.storage.chunk_maps().await.unwrap();
    wait_count(db, "mst2_chunk_reader", 0).await;
    let history = db
        .query_one_raw(statement(
            "SELECT state,create_completed FROM mst2_chunk_receipt_generation WHERE receipt_key=$1",
            [old_key.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history.try_get::<String>("", "state").unwrap(), "APPLIED");
    assert!(!history.try_get::<bool>("", "create_completed").unwrap());
    let new_key: String = db
        .query_one_raw(statement(
            "SELECT receipt_key FROM mst2_chunk_map_source",
            [],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "receipt_key")
        .unwrap();
    assert_ne!(old_key, new_key);
    release.notify_one();
    timeout(Duration::from_secs(10), async {
        loop {
            if fixture
                .state
                .storage
                .git_service
                .obj_storage
                .inner
                .exists(&ObjectKey {
                    namespace: ObjectNamespace::ChunkMapReceipt,
                    key: old_key.clone(),
                })
                .await
                .unwrap()
                && fixture.counts.receipt_writes.load(Ordering::SeqCst) == 2
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    repository
        .maintain(&fixture.state.storage.git_service.obj_storage, 64)
        .await
        .unwrap();
    assert!(
        !fixture
            .state
            .storage
            .git_service
            .obj_storage
            .inner
            .exists(&ObjectKey {
                namespace: ObjectNamespace::ChunkMapReceipt,
                key: old_key.clone()
            })
            .await
            .unwrap()
    );
    assert!(
        fixture
            .state
            .storage
            .git_service
            .obj_storage
            .inner
            .exists(&ObjectKey {
                namespace: ObjectNamespace::ChunkMapReceipt,
                key: new_key
            })
            .await
            .unwrap()
    );
    fixture.counts.reset();
    assert_eq!(fixture.map("/file").await["map"], current["map"]);
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    let history = db
        .query_one_raw(statement(
            "SELECT create_completed FROM mst2_chunk_receipt_generation WHERE receipt_key=$1",
            [old_key.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    assert!(!history.try_get::<bool>("", "create_completed").unwrap());
}

#[tokio::test]
async fn actual_backing_quota_and_unsupported_capability_reject_before_source_open() {
    for unsupported in [true, false] {
        let fixture = Fixture::new().await;
        let counts = Arc::new(ReadCounts::default());
        counts
            .receipt_retention_unsupported
            .store(unsupported, Ordering::SeqCst);
        let backend = mock_object_storage();
        if !unsupported {
            for index in 0..=crate::orbit_api::object_storage::MAX_CHUNK_MAP_RECEIPTS {
                backend
                    .inner
                    .put_metadata_atomic_create(
                        &ObjectKey {
                            namespace: ObjectNamespace::ChunkMapReceipt,
                            key: format!("{index:064x}"),
                        },
                        Bytes::from_static(&[1; 129]),
                        ObjectMeta::default(),
                    )
                    .await
                    .unwrap();
            }
        }
        let mut state = fixture.state.clone();
        state.storage.git_service = GitService {
            obj_storage: MegaObjectStorageWrapper::new(Arc::new(CountingStorage {
                inner: backend,
                counts: counts.clone(),
            })),
        };
        let response = router(&state)
            .oneshot(fixture.request("GET", "chunk-map?path=/file", Body::empty()))
            .await
            .unwrap();
        if unsupported {
            error(response, 503, "TEMPORARY_UNAVAILABLE", true).await;
        } else {
            error(response, 413, "LIMIT_EXCEEDED", false).await;
        }
        counts.assert(0, 0);
        assert_eq!(counts.receipt_writes.load(Ordering::SeqCst), 0);
        let mono = fixture.state.storage.mono_storage();
        let db = mono.get_connection();
        assert_eq!(count(db, "mst2_chunk_receipt_generation").await, 0);
        assert_eq!(count(db, "mst2_chunk_map").await, 0);
    }
}

#[tokio::test]
async fn empty_source_fragment_after_deadline_does_not_renew_install_or_publish() {
    let fixture = Fixture::new().await;
    let repository = fixture.state.storage.chunk_maps().await.unwrap();
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    let admission = Arc::new(
        repository
            .admit_install(&source, &fixture.state.storage.git_service.obj_storage)
            .await
            .unwrap(),
    );
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let tail_polls = Arc::new(AtomicUsize::new(0));
    *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
        oid: fixture.oid.clone(),
        kind: bounded_objects::FaultKind::HeldFragment {
            prefix: Bytes::new(),
            fragment: Some(Bytes::new()),
            entered: entered.clone(),
            release: release.clone(),
            tail_polls: tail_polls.clone(),
            drops: drops.clone(),
        },
    });
    let handler = MonoApiService::from(&fixture.state);
    let budget = crate::ceres::snapshot::content_budget::MemoryBudget::new(8 * 1024 * 1024);
    let verifier_budget = budget.clone();
    let owner = admission.clone();
    let task = tokio::spawn(async move {
        VerifiedSourceChunkMap::verify(&handler, source, &verifier_budget, &owner).await
    });
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    db.execute_unprepared("UPDATE mst2_chunk_receipt_generation SET deadline=pg_catalog.clock_timestamp()+interval '20 milliseconds' WHERE state='RESERVED'").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    admission.test_check_next_owner_operation().await;
    release.notify_one();
    let error = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .err()
        .unwrap();
    assert_eq!(error.code, SnapshotErrorCode::LeaseExpired);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(tail_polls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.bytes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    assert_eq!(count(db, "mst2_chunk_map_source").await, 0);
    assert_eq!(budget.used(), 0);
    drop(admission);
    repository
        .maintain(&fixture.state.storage.git_service.obj_storage, 64)
        .await
        .unwrap();
    assert_eq!(count(db, "mst2_chunk_map").await, 0);
}
