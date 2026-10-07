use std::collections::BTreeSet;

use mst2_codec::metapage::{Page, page_id};

use super::*;
use crate::jupiter::storage::native_snapshot_session::MetadataRouteRequest;

fn metadata_body(items: Value, encoding: &str) -> Vec<u8> {
    serde_json::to_vec_pretty(&json!({"encoding": encoding, "items": items})).unwrap()
}

async fn metadata_bytes(fixture: &Fixture, app: Router, body: &[u8]) -> Vec<u8> {
    let response = app
        .oneshot(fixture.request("POST", "metadata/pages", Body::from(body.to_vec())))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-mega-snapshot-id"], fixture.snapshot);
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn assert_metadata(bytes: &[u8], body: &[u8], count: u32, expected: &[([u8; 32], Vec<u8>)]) {
    let frames = parse_stream(bytes).unwrap();
    let mut actual = Vec::new();
    for frame in &frames[..frames.len() - 1] {
        let Frame::Meta(meta) = frame else {
            panic!("metadata route emitted a non-META frame")
        };
        assert!(meta.pages.len() <= mst2_codec::treeframe::META_MAX_PAGES);
        assert!(
            meta.pages
                .iter()
                .map(|(_, page)| 36 + page.len())
                .sum::<usize>()
                <= mst2_codec::treeframe::META_MAX_RAW
        );
        for (id, page) in &meta.pages {
            assert_eq!(*id, page_id(page));
            Page::decode(page).unwrap();
        }
        actual.extend_from_slice(&meta.pages);
    }
    assert_eq!(actual, expected);
    let Frame::End(end) = frames.last().unwrap() else {
        panic!("metadata route omitted END")
    };
    assert_eq!(end.request_item_count, count);
    assert_eq!(end.unique_unit_count, expected.len() as u32);
    assert_eq!(
        end.logical_bytes,
        expected
            .iter()
            .map(|(_, page)| page.len() as u64)
            .sum::<u64>()
    );
    assert_eq!(
        end.request_body_sha256,
        <[u8; 32]>::from(Sha256::digest(body))
    );
}

#[tokio::test]
async fn mst2_persisted_meta_matches_canonical_routes_after_rebuild_and_advance_without_git_reads()
{
    let fixture = Fixture::new_with_pg_config_and_directories(true, 140).await;
    let context = fixture
        .state
        .storage
        .snapshot_context(&fixture.snapshot, &fixture.lease)
        .await
        .unwrap();
    let handler = fixture
        .state
        .api_handler(std::path::Path::new("/"))
        .await
        .unwrap();
    let root_tree = handler
        .get_tree_by_hash(&context.root_tree_oid)
        .await
        .unwrap();
    let built = crate::ceres::snapshot::pages::build_directory_page(
        handler.as_ref(),
        &root_tree,
        "/project",
    )
    .await
    .unwrap();
    let (Page::Branch { children, .. }, _) = Page::decode(&built.page_bytes).unwrap() else {
        panic!("wide fixture must use canonical radix pages")
    };
    let label = children
        .iter()
        .find(|child| child.label == b'w')
        .unwrap()
        .label;
    let specs = [
        ("/", vec![]),
        ("/", vec![label]),
        ("/wide-139", vec![]),
        ("/nested", vec![]),
        ("/directory", vec![]),
        ("/", vec![label]),
    ];
    let mut expected = Vec::new();
    let mut seen = BTreeSet::new();
    let mut items = Vec::new();
    for (path, route) in &specs {
        let absolute = if *path == "/" {
            "/project".to_owned()
        } else {
            format!("/project{path}")
        };
        let directory = crate::ceres::snapshot::pages::build_directory_page(
            handler.as_ref(),
            &root_tree,
            &absolute,
        )
        .await
        .unwrap();
        let pages = Page::pages_along_route(&directory.codec_entries, route).unwrap();
        let reached = page_id(pages.last().unwrap());
        items.push(json!({"directory_path": path, "route": route,
            "expected_digest": format!("sha256:{}", hex::encode(reached))}));
        for page in pages {
            let id = page_id(&page);
            if seen.insert(id) {
                expected.push((id, page));
            }
        }
    }
    advance(&fixture).await;
    let state = rebuilt(&fixture).await;
    assert!(state.storage.native_snapshot_sessions.get().is_none());
    let mono = fixture.state.storage.mono_storage();
    let held = mono.get_connection().begin().await.unwrap();
    held.execute_unprepared("LOCK TABLE mega_tree,mst2_verified_object IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    fixture.counts.reset();
    for encoding in ["identity", "zstd"] {
        let body = metadata_body(json!(items), encoding);
        let bytes = tokio::time::timeout(
            Duration::from_secs(4),
            metadata_bytes(&fixture, app(&state), &body),
        )
        .await
        .expect("persisted META must not touch locked Git tree or verified-object tables");
        assert_metadata(&bytes, &body, specs.len() as u32, &expected);
    }
    let requests: Vec<_> = specs
        .iter()
        .map(|(path, route)| MetadataRouteRequest {
            directory_path: path,
            route,
            expected_digest: None,
        })
        .collect();
    let batch = state
        .storage
        .snapshot_sessions()
        .await
        .metadata_routes(&context, &requests)
        .await
        .unwrap();
    assert_eq!(batch.pages, expected);
    assert_eq!(batch.work.page_queries, batch.work.pages_loaded);
    assert!(
        batch.work.pages_loaded < 20,
        "route work must not scan the 140-directory DAG"
    );
    assert!(
        batch.work.walk_visits > batch.work.pages_loaded,
        "duplicate/path visits reuse request pages"
    );
    assert!(
        batch.work.payload_bytes >= batch.pages.iter().map(|(_, page)| page.len() as u64).sum()
    );
    held.rollback().await.unwrap();
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_persisted_meta_absence_scope_digest_limits_and_release_oracles() {
    let fixture = Fixture::new_with_pg_config(true).await;
    for (items, status, code) in [
        (
            json!([{"directory_path":"/missing"}]),
            404,
            "PATH_NOT_FOUND",
        ),
        (json!([{"directory_path":"/file"}]), 409, "NOT_DIRECTORY"),
        (
            json!([{"directory_path":"/link/child"}]),
            409,
            "NOT_DIRECTORY",
        ),
        (
            json!([{"directory_path":"/nested", "route":[0]}]),
            404,
            "PATH_NOT_FOUND",
        ),
        (
            json!([{"directory_path":"/../outside"}]),
            400,
            "SCOPE_INVALID",
        ),
        (
            json!([{"directory_path":format!("/{}", vec!["a"; 256].join("/"))}]),
            400,
            "SCOPE_INVALID",
        ),
        (
            json!([{"directory_path":"/outside"}]),
            404,
            "PATH_NOT_FOUND",
        ),
        (
            json!([{"directory_path":"/", "expected_digest":format!("sha256:{}", "0".repeat(64))}]),
            409,
            "EXPECTED_DIGEST_MISMATCH",
        ),
        (json!([]), 413, "LIMIT_EXCEEDED"),
        (
            json!(vec![json!({"directory_path":"/"}); 65]),
            413,
            "LIMIT_EXCEEDED",
        ),
    ] {
        error(
            fixture
                .send(
                    "POST",
                    "metadata/pages",
                    Body::from(metadata_body(items, "identity")),
                )
                .await,
            status,
            code,
            false,
        )
        .await;
    }
    let body = metadata_body(json!([{"directory_path":"/"}]), "identity");
    let response = fixture
        .send("POST", "metadata/pages", Body::from(body))
        .await;
    assert_eq!(response.status(), 200);
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    let (Frame::Meta(meta), consumed) = mst2_codec::treeframe::parse_frame(&first).unwrap() else {
        panic!("first persisted frame must be META")
    };
    assert_eq!(consumed, first.len());
    assert!(!meta.pages.is_empty());
    lease_control(&fixture, &fixture.lease, "DELETE", false).await;
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
    assert!(matches!(
        parse_stream(&first),
        Err(mst2_codec::CodecError::BadOrdering(
            "stream missing END/ERROR frame"
        ))
    ));
    fixture.counts.assert(0, 0);
}

async fn page_row(fixture: &Fixture, path: &str) -> ([u8; 32], Vec<u8>) {
    let body = metadata_body(json!([{"directory_path":path}]), "identity");
    let bytes = metadata_bytes(fixture, fixture.app.clone(), &body).await;
    let Frame::Meta(meta) = &parse_stream(&bytes).unwrap()[0] else {
        panic!("META required")
    };
    meta.pages[0].clone()
}

async fn damage_page(fixture: &Fixture, id: [u8; 32], remove: bool) {
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let modes = handoff_trigger_modes_for_test(db).await;
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared("SELECT pg_advisory_xact_lock(1296717362,hashtext(current_schema()))")
        .await
        .unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_payload DISABLE TRIGGER mst2_metadata_payload_fenced",
    )
    .await
    .unwrap();
    if remove {
        txn.execute_unprepared(
            "ALTER TABLE mst2_metadata_payload DISABLE TRIGGER mst2_metadata_payload_removed",
        )
        .await
        .unwrap();
    }
    let sql = if remove {
        "DELETE FROM mst2_metadata_payload WHERE page_id=$1"
    } else {
        "UPDATE mst2_metadata_payload SET payload=set_byte(payload,0,0) WHERE page_id=$1"
    };
    assert_eq!(
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            [id.to_vec().into()]
        ))
        .await
        .unwrap()
        .rows_affected(),
        1
    );
    if remove {
        txn.execute_unprepared(
            "ALTER TABLE mst2_metadata_payload ENABLE TRIGGER mst2_metadata_payload_removed",
        )
        .await
        .unwrap();
    }
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_payload ENABLE TRIGGER mst2_metadata_payload_fenced",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(handoff_trigger_modes_for_test(db).await, modes);
    assert!(
        db.execute_unprepared("UPDATE mst2_metadata_payload SET payload=payload")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn mst2_persisted_meta_warm_missing_and_corrupt_pages_never_reproject_from_git() {
    for remove in [true, false] {
        let fixture = Fixture::new_with_pg_config(true).await;
        let (id, _) = page_row(&fixture, "/nested").await;
        damage_page(&fixture, id, remove).await;
        let body = metadata_body(json!([{"directory_path":"/nested"}]), "identity");
        let mono = fixture.state.storage.mono_storage();
        let held = mono.get_connection().begin().await.unwrap();
        held.execute_unprepared(
            "LOCK TABLE mega_tree,mst2_verified_object IN ACCESS EXCLUSIVE MODE",
        )
        .await
        .unwrap();
        let response = tokio::time::timeout(
            Duration::from_secs(4),
            fixture.send("POST", "metadata/pages", Body::from(body)),
        )
        .await
        .expect("damaged persisted page must fail before any source read");
        error(
            response,
            if remove { 503 } else { 502 },
            if remove {
                "OBJECT_UNAVAILABLE"
            } else {
                "INTEGRITY_ERROR"
            },
            false,
        )
        .await;
        held.rollback().await.unwrap();
        fixture.counts.assert(0, 0);
    }
}

async fn wait_retention_waiter(txn: &sea_orm::DatabaseTransaction) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            txn.execute_unprepared("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            if scalar(
                txn,
                "SELECT count(*) FROM pg_locks l
              WHERE l.locktype='advisory' AND NOT l.granted
                AND l.database=(SELECT oid FROM pg_database WHERE datname=current_database())
                AND l.pid<>pg_backend_pid() AND l.classid=1296717362::oid
                AND l.objid=hashtext(current_schema())::oid AND l.objsubid=2",
            )
            .await
                > 0
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("META request did not wait for retention authority");
}

#[tokio::test]
async fn mst2_persisted_meta_rechecks_deadline_and_release_after_waiting_for_retention_lock() {
    for expire in [true, false] {
        let fixture = Fixture::new_with_pg_config(true).await;
        let mono = fixture.state.storage.mono_storage();
        let held = mono.get_connection().begin().await.unwrap();
        held.execute_unprepared(
            "SELECT pg_advisory_xact_lock(1296717362,hashtext(current_schema()))",
        )
        .await
        .unwrap();
        let pending = {
            let application = fixture.app.clone();
            let request = fixture.request(
                "POST",
                "metadata/pages",
                Body::from(metadata_body(
                    json!([{"directory_path":"/nested"}]),
                    "identity",
                )),
            );
            tokio::spawn(async move { application.oneshot(request).await.unwrap() })
        };
        wait_retention_waiter(&held).await;
        let sql = if expire {
            "UPDATE mst2_snapshot_lease SET expires_at_unix=0 WHERE lease_id=$1"
        } else {
            "UPDATE mst2_snapshot_lease SET state='RELEASED' WHERE lease_id=$1"
        };
        held.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            [fixture.lease.clone().into()],
        ))
        .await
        .unwrap();
        held.commit().await.unwrap();
        error(pending.await.unwrap(), 410, "LEASE_EXPIRED", false).await;
        fixture.counts.assert(0, 0);
    }
}

async fn bound_fixture() -> (
    Fixture,
    crate::ceres::snapshot::retention_dag::MetadataPagePayload,
) {
    let mut fixture = Fixture::new_with_pg_config(true).await;
    advance(&fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let head = mono
        .read_native_publication_head(
            fixture
                .state
                .storage
                .config()
                .mst2
                .instance_uuid
                .as_deref()
                .unwrap(),
        )
        .await
        .unwrap();
    let handler = fixture
        .state
        .api_handler(std::path::Path::new("/"))
        .await
        .unwrap();
    let tree = handler.get_tree_by_hash(&head.root.tree).await.unwrap();
    let prepared = crate::ceres::snapshot::pages::prepare_native_metadata_retention(
        handler.as_ref(),
        &tree,
        "/project",
        crate::ceres::snapshot::retention_dag::MetadataDagLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(prepared.dag().payloads().len(), 1);
    let expected = prepared.dag().payloads()[0].clone();
    let repository = crate::jupiter::storage::native_metadata_install::generations::PostgresMetadataGenerationRepository::new(
        mono.get_connection().clone()).await.unwrap();
    let intent = repository
        .begin_intent("persisted-meta-bound-seed", &prepared)
        .await
        .unwrap();
    repository
        .install_pages(&intent, prepared.dag().payloads())
        .await
        .unwrap();
    repository.finalize(&intent).await.unwrap();
    let resolved = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/project"))
            .await
            .unwrap(),
    )
    .await;
    assert_ne!(resolved["descriptor"]["snapshot_id"], fixture.snapshot);
    fixture.snapshot = resolved["descriptor"]["snapshot_id"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.lease = resolved["lease_id"].as_str().unwrap().to_owned();
    let db = mono.get_connection();
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation=1"
        )
        .await,
        1
    );
    db.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size,graph_domain)
         VALUES($1,$2,2,'RESERVED',1,$3,'generic-v1')",
        [expected.id.to_vec().into(), format!("page:sha256:{}", hex::encode(expected.id)).into(),
            (expected.size as i32).into()])).await.unwrap();
    let membership = db.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT pp.generation AS member_generation,c.generation AS current_generation
         FROM mst2_snapshot_context s JOIN mst2_metadata_prepare_page pp ON pp.prepare_id=s.prepare_id
         JOIN mst2_metadata_current c ON c.page_id=pp.page_id WHERE s.snapshot_id=$1",
        [fixture.snapshot.clone().into()])).await.unwrap().unwrap();
    assert_eq!(
        membership
            .try_get::<Option<i64>>("", "member_generation")
            .unwrap(),
        None
    );
    assert_eq!(
        membership.try_get::<i64>("", "current_generation").unwrap(),
        1
    );
    fixture.counts.reset();
    (fixture, expected)
}

#[tokio::test]
async fn mst2_persisted_meta_serves_bound_generic_pages_with_null_members_and_extra_history() {
    let (fixture, expected) = bound_fixture().await;
    let body = metadata_body(json!([{"directory_path":"/"}]), "identity");
    for application in [fixture.app.clone(), app(&rebuilt(&fixture).await)] {
        let bytes = metadata_bytes(&fixture, application, &body).await;
        assert_metadata(&bytes, &body, 1, &[(expected.id, expected.bytes.clone())]);
    }
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_persisted_meta_rejects_touched_graph_damage_and_prepare_membership_loss() {
    for case in 0..5 {
        let fixture = Fixture::new_with_pg_config(true).await;
        let (id, _) = page_row(&fixture, "/nested").await;
        let mono = fixture.state.storage.mono_storage();
        let db = mono.get_connection();
        let modes = handoff_trigger_modes_for_test(db).await;
        let txn = db.begin().await.unwrap();
        txn.execute_unprepared(
            "SELECT pg_advisory_xact_lock(1296717362,hashtext(current_schema()))",
        )
        .await
        .unwrap();
        let node = format!("page:sha256:{}", hex::encode(id));
        let (sql, values) = match case {
            0 => ("UPDATE mst2_retention_node SET state='DELETING' WHERE node_id=$1", vec![node.into()]),
            1 => ("UPDATE mst2_retention_node SET bytes=bytes+1 WHERE node_id=$1", vec![node.into()]),
            2 => ("DELETE FROM mst2_retention_edge WHERE child_id=$1", vec![node.into()]),
            3 => ("INSERT INTO mst2_retention_gc_op(operation_id,node_id,operation,state,attempts,created_at)
                VALUES('persisted-meta-tombstone',$1,'REMOVE','PENDING',0,now())", vec![node.into()]),
            4 => {
                txn.execute_unprepared("ALTER TABLE mst2_metadata_prepare_page DISABLE TRIGGER mst2_install_capability_mapping_guard").await.unwrap();
                ("DELETE FROM mst2_metadata_prepare_page WHERE page_id=$1 AND prepare_id=
                  (SELECT prepare_id FROM mst2_snapshot_context WHERE snapshot_id=$2)",
                  vec![id.to_vec().into(), fixture.snapshot.clone().into()])
            }
            _ => unreachable!(),
        };
        assert!(
            txn.execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                sql,
                values
            ))
            .await
            .unwrap()
            .rows_affected()
                > 0
        );
        if case == 4 {
            txn.execute_unprepared("ALTER TABLE mst2_metadata_prepare_page ENABLE TRIGGER mst2_install_capability_mapping_guard").await.unwrap();
        }
        txn.commit().await.unwrap();
        assert_eq!(handoff_trigger_modes_for_test(db).await, modes);
        let response = fixture
            .send(
                "POST",
                "metadata/pages",
                Body::from(metadata_body(
                    json!([{"directory_path":"/nested"}]),
                    "identity",
                )),
            )
            .await;
        error(
            response,
            if case == 2 { 502 } else { 503 },
            if case == 2 {
                "INTEGRITY_ERROR"
            } else {
                "OBJECT_UNAVAILABLE"
            },
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
}

#[tokio::test]
async fn mst2_persisted_meta_reader_holds_protection_until_all_route_bytes_are_owned() {
    use crate::jupiter::storage::native_snapshot_session::with_metadata_read_barriers;

    for release_lease in [false, true] {
        let fixture = Fixture::new_with_pg_config(true).await;
        let expected = vec![
            page_row(&fixture, "/nested").await,
            page_row(&fixture, "/directory").await,
        ];
        let context = fixture
            .state
            .storage
            .snapshot_context(&fixture.snapshot, &fixture.lease)
            .await
            .unwrap();
        let captured = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let reading = {
            let state = fixture.state.clone();
            let context = context.clone();
            let captured = captured.clone();
            let resume = resume.clone();
            tokio::spawn(async move {
                let requests = [
                    MetadataRouteRequest {
                        directory_path: "/nested",
                        route: &[],
                        expected_digest: None,
                    },
                    MetadataRouteRequest {
                        directory_path: "/directory",
                        route: &[],
                        expected_digest: None,
                    },
                ];
                with_metadata_read_barriers(
                    captured,
                    resume,
                    state
                        .storage
                        .snapshot_sessions()
                        .await
                        .metadata_routes(&context, &requests),
                )
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(4), captured.wait())
            .await
            .unwrap();
        let root = format!("page:{}", context.built.metadata_root);
        let mut competing = {
            let state = fixture.state.clone();
            let lease = fixture.lease.clone();
            let root = root.clone();
            tokio::spawn(async move {
                if release_lease {
                    assert!(state.storage.snapshot_release(&lease).await.unwrap());
                    None
                } else {
                    Some(
                        PostgresRetentionRepository::new(
                            state.storage.mono_storage().get_connection().clone(),
                        )
                        .mark_deleting("persisted-meta-reader-race", &root)
                        .await
                        .unwrap(),
                    )
                }
            })
        };
        let observer = fixture
            .state
            .storage
            .mono_storage()
            .get_connection()
            .begin()
            .await
            .unwrap();
        tokio::select! {
            () = wait_retention_waiter(&observer) => {},
            outcome = &mut competing => {
                panic!("retention competitor completed before waiting: release_lease={release_lease}, outcome={outcome:?}");
            },
        }
        assert!(!reading.is_finished());
        assert!(!competing.is_finished());
        resume.wait().await;
        let batch = reading.await.unwrap().unwrap();
        assert_eq!(batch.pages, expected);
        assert_eq!(
            batch.work.pages_loaded, 3,
            "root and both directories are read before unlock"
        );
        let outcome = competing.await.unwrap();
        observer.rollback().await.unwrap();
        if release_lease {
            assert!(outcome.is_none());
        } else {
            assert_eq!(outcome, Some(GcClaim::Unavailable));
            lease_control(&fixture, &fixture.lease, "DELETE", false).await;
        }
        let gc = PostgresRetentionRepository::new(
            fixture
                .state
                .storage
                .mono_storage()
                .get_connection()
                .clone(),
        );
        assert_eq!(
            gc.mark_deleting("persisted-meta-after-read", &root)
                .await
                .unwrap(),
            GcClaim::Marked
        );
        gc.complete_gc("persisted-meta-after-read").await.unwrap();
        let requests = [MetadataRouteRequest {
            directory_path: "/nested",
            route: &[],
            expected_digest: None,
        }];
        let rejected = fixture
            .state
            .storage
            .snapshot_sessions()
            .await
            .metadata_routes(&context, &requests)
            .await
            .err()
            .expect("released and collected root must fail before touching pages");
        assert_eq!(rejected.code, SnapshotErrorCode::LeaseExpired);
        assert_eq!(
            batch.pages, expected,
            "collected graph does not invalidate owned route bytes"
        );
        fixture.counts.assert(0, 0);
    }
}

async fn fault_binding(fixture: &Fixture, id: [u8; 32], case: usize, restore: bool) {
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let modes = handoff_trigger_modes_for_test(db).await;
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared("SELECT pg_advisory_xact_lock(1296717362,hashtext(current_schema()))")
        .await
        .unwrap();
    let (table, guard, sql) = match case {
        0 => (
            "mst2_metadata_payload",
            "mst2_metadata_payload_fenced",
            if restore {
                "UPDATE mst2_metadata_payload SET generation=1 WHERE page_id=$1"
            } else {
                "UPDATE mst2_metadata_payload SET generation=NULL WHERE page_id=$1"
            },
        ),
        1 => (
            "mst2_metadata_current",
            "mst2_metadata_current_guard",
            if restore {
                "UPDATE mst2_metadata_current SET generation=1 WHERE page_id=$1"
            } else {
                "UPDATE mst2_metadata_current SET generation=2 WHERE page_id=$1"
            },
        ),
        2 => (
            "mst2_metadata_lifetime",
            "mst2_metadata_lifetime_guard",
            if restore {
                "UPDATE mst2_metadata_lifetime SET state='LIVE' WHERE page_id=$1 AND generation=1"
            } else {
                "UPDATE mst2_metadata_lifetime SET state='DELETING' WHERE page_id=$1 AND generation=1"
            },
        ),
        3 => (
            "mst2_metadata_lifetime",
            "mst2_metadata_lifetime_guard",
            if restore {
                "UPDATE mst2_metadata_lifetime SET graph_domain='generic-v1' WHERE page_id=$1 AND generation=1"
            } else {
                "UPDATE mst2_metadata_lifetime SET graph_domain='qualified-v1' WHERE page_id=$1 AND generation=1"
            },
        ),
        4 => (
            "mst2_metadata_lifetime",
            "mst2_metadata_lifetime_guard",
            if restore {
                "UPDATE mst2_metadata_lifetime SET expected_size=expected_size-1 WHERE page_id=$1 AND generation=1"
            } else {
                "UPDATE mst2_metadata_lifetime SET expected_size=expected_size+1 WHERE page_id=$1 AND generation=1"
            },
        ),
        5 => (
            "mst2_metadata_prepare_page",
            "mst2_install_capability_mapping_guard",
            if restore {
                "UPDATE mst2_metadata_prepare_page SET generation=NULL WHERE page_id=$1 AND prepare_id=
                (SELECT prepare_id FROM mst2_snapshot_context WHERE snapshot_id=$2)"
            } else {
                "UPDATE mst2_metadata_prepare_page SET generation=2 WHERE page_id=$1 AND prepare_id=
                (SELECT prepare_id FROM mst2_snapshot_context WHERE snapshot_id=$2)"
            },
        ),
        _ => unreachable!(),
    };
    txn.execute_unprepared(&format!("ALTER TABLE {table} DISABLE TRIGGER {guard}"))
        .await
        .unwrap();
    let values = if case == 5 {
        vec![id.to_vec().into(), fixture.snapshot.clone().into()]
    } else {
        vec![id.to_vec().into()]
    };
    assert_eq!(
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values
        ))
        .await
        .unwrap()
        .rows_affected(),
        1
    );
    if case == 1 {
        let protection = txn
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT t.tgenabled::text AS mode,t.tgdeferrable AS deferrable,t.tginitdeferred AS deferred
                 FROM pg_trigger t WHERE t.tgrelid='mst2_metadata_current'::regclass
                   AND t.tgname='mst2_metadata_current_protected' AND NOT t.tgisinternal",
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            protection.try_get::<String>("", "mode").unwrap().as_str(),
            "O" | "A"
        ));
        assert!(protection.try_get::<bool>("", "deferrable").unwrap());
        assert!(protection.try_get::<bool>("", "deferred").unwrap());
        txn.execute_unprepared("SET CONSTRAINTS mst2_metadata_current_protected IMMEDIATE")
            .await
            .unwrap();
        txn.execute_unprepared("SET CONSTRAINTS mst2_metadata_current_protected DEFERRED")
            .await
            .unwrap();
    }
    txn.execute_unprepared(&format!("ALTER TABLE {table} ENABLE TRIGGER {guard}"))
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(handoff_trigger_modes_for_test(db).await, modes);
}

#[tokio::test]
async fn mst2_persisted_meta_warm_reads_require_exact_generic_current_lifetime_and_membership() {
    let (fixture, expected) = bound_fixture().await;
    let body = metadata_body(json!([{"directory_path":"/"}]), "identity");
    let bytes = metadata_bytes(&fixture, fixture.app.clone(), &body).await;
    assert_metadata(&bytes, &body, 1, &[(expected.id, expected.bytes.clone())]);
    for case in 0..6 {
        fault_binding(&fixture, expected.id, case, false).await;
        error(
            fixture
                .send("POST", "metadata/pages", Body::from(body.clone()))
                .await,
            if matches!(case, 0 | 1 | 5) { 502 } else { 503 },
            if matches!(case, 0 | 1 | 5) {
                "INTEGRITY_ERROR"
            } else {
                "OBJECT_UNAVAILABLE"
            },
            false,
        )
        .await;
        fault_binding(&fixture, expected.id, case, true).await;
        let bytes = metadata_bytes(&fixture, fixture.app.clone(), &body).await;
        assert_metadata(&bytes, &body, 1, &[(expected.id, expected.bytes.clone())]);
    }
    fixture.counts.assert(0, 0);
}
