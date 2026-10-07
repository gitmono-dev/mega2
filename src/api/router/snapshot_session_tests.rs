use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use tokio::sync::Barrier;

use super::*;
use crate::{
    api::router::snapshot_router::{with_native_handoff_barriers, with_native_resolve_barriers},
    jupiter::storage::{
        mst2_retention::{GcClaim, PostgresRetentionRepository},
        push_queue_storage::PushQueueStorage,
    },
};

fn resolve_request(scope: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v2/snapshots/resolve")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"target":{"kind":"latest"},"scope":scope}).to_string(),
        ))
        .unwrap()
}

async fn scalar<C: ConnectionTrait>(db: &C, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(DbBackend::Postgres, sql.to_owned()))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn root_metadata(
    fixture: &Fixture,
) -> crate::ceres::snapshot::pages::PreparedNativeMetadataRetention {
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
    crate::ceres::snapshot::pages::prepare_native_metadata_retention(
        handler.as_ref(),
        &tree,
        "/",
        crate::ceres::snapshot::retention_dag::MetadataDagLimits::default(),
    )
    .await
    .unwrap()
}

async fn stored_metadata_ids(db: &DatabaseConnection) -> std::collections::BTreeSet<[u8; 32]> {
    db.query_all_raw(Statement::from_string(
        DbBackend::Postgres,
        "SELECT page_id FROM mst2_metadata_payload",
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        let id: Vec<u8> = row.try_get_by_index(0).unwrap();
        id.try_into().unwrap()
    })
    .collect()
}

async fn rebuilt(fixture: &Fixture) -> MonoApiServiceState {
    let config = fixture.state.storage.config();
    let connection = crate::jupiter::storage::init::postgres_connection(&config.database)
        .await
        .unwrap();
    let assembly = crate::jupiter::storage::Storage::new_with_connection(
        config,
        Arc::new(connection),
        fixture.state.storage.git_service.obj_storage.clone(),
    );
    let storage = if fixture.generic_history {
        crate::jupiter::storage::init::with_generic_history_bootstrap(assembly).await
    } else {
        assembly.await
    }
    .unwrap();
    MonoApiServiceState {
        storage,
        git_object_cache: Arc::new(GitObjectCache {
            connection: fixture.state.git_object_cache.connection.clone(),
            prefix: uuid::Uuid::new_v4().to_string(),
        }),
        ..fixture.state.clone()
    }
}

fn app(state: &MonoApiServiceState) -> Router {
    Router::new().nest(
        "/api/v2",
        crate::api::router::snapshot_router::generic_history_routers(state.clone())
            .with_state(state.clone()),
    )
}

async fn lease_control(fixture: &Fixture, lease: &str, method: &str, renew: bool) -> Value {
    let suffix = if renew { "/renew" } else { "" };
    success_json(
        fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("/api/v2/snapshots/leases/{lease}{suffix}"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await
}

async fn advance(fixture: &Fixture) {
    let mono = fixture.state.storage.mono_storage();
    let old = mono.get_main_ref("/project").await.unwrap().unwrap();
    let old_oid = ObjectHash::from_hex_for_kind(HashKind::Sha1, &old.ref_commit_hash).unwrap();
    let oid = ObjectHash::from_hex_for_kind(HashKind::Sha1, &fixture.oid).unwrap();
    let next_tree = tree(vec![item(TreeItemMode::Blob, oid, "new-only")]);
    let next = Commit::from_tree_id_with_kind(
        HashKind::Sha1,
        next_tree.id,
        vec![old_oid],
        "durable HTTP next view",
    )
    .unwrap();
    mono.save_mega_trees(vec![next_tree], next.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![next.clone()], None)
        .await
        .unwrap();
    publish_native_push(&fixture.state.storage, "/project", old_oid, &next).await;
}

#[tokio::test]
async fn mst2_durable_http_resolve_installs_complete_dag_and_warm_leases_share_it() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let pages = scalar(db, "SELECT count(*) FROM mst2_metadata_payload").await;
    assert!(
        pages >= 3,
        "scope includes nested and empty directory pages"
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_metadata_prepare_page").await,
        pages
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_snapshot_context").await,
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_snapshot_lease WHERE state='ACTIVE'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='prepare'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='lease'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='pin'"
        )
        .await,
        1
    );
    let edges = scalar(db, "SELECT count(*) FROM mst2_retention_edge").await;
    let refs = scalar(
        db,
        "SELECT sum(incoming_refs)::bigint FROM mst2_retention_node",
    )
    .await;
    assert_eq!(edges, refs);
    fixture.counts.reset();
    let again = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/project"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(again["descriptor"]["snapshot_id"], fixture.snapshot);
    assert_ne!(again["lease_id"], fixture.lease);
    fixture.counts.assert(0, 0);
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_metadata_payload").await,
        pages
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_metadata_prepare").await,
        1
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_snapshot_context").await,
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='lease'"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(
            db,
            "SELECT sum(incoming_refs)::bigint FROM mst2_retention_node"
        )
        .await,
        refs
    );

    let state = rebuilt(&fixture).await;
    assert!(state.storage.native_snapshot_sessions.get().is_none());
    assert!(!Arc::ptr_eq(
        &state.storage.native_projection_cache,
        &fixture.state.storage.native_projection_cache
    ));
    let replacement = app(&state);
    let descriptor = success_json(
        replacement
            .clone()
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(descriptor["snapshot_id"], fixture.snapshot);
    assert_eq!(descriptor["lease_id"], fixture.lease);
    assert_eq!(descriptor["descriptor"], again["descriptor"]);
    for (method, path, body) in [
        ("GET", "directory?path=/nested", Body::empty()),
        (
            "POST",
            "lookup",
            Body::from(json!({"paths":["/nested/file"]}).to_string()),
        ),
        (
            "POST",
            "metadata/pages",
            Body::from(json!({"items":[{"directory_path":"/nested"}]}).to_string()),
        ),
        ("HEAD", "blob?path=/file", Body::empty()),
    ] {
        let response = replacement
            .clone()
            .oneshot(fixture.request(method, path, body))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        to_bytes(response.into_body(), usize::MAX).await.unwrap();
    }
    fixture.counts.assert(0, 0);
    let response = replacement
        .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw.as_slice()
    );
    fixture.counts.assert(2, 2 * fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mst2_durable_http_old_sid_and_original_lease_survive_real_publication_and_fresh_service() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let old = success_json(fixture.send("GET", "descriptor", Body::empty()).await).await;
    advance(&fixture).await;
    let next = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/project"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(next["publication_sequence"], "2");
    assert_ne!(next["descriptor"]["snapshot_id"], fixture.snapshot);
    let state = rebuilt(&fixture).await;
    let replacement = app(&state);
    let restored = success_json(
        replacement
            .clone()
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(restored, old);
    let response = replacement
        .clone()
        .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw.as_slice()
    );
    error(
        replacement
            .oneshot(fixture.request("GET", "chunk-map?path=/new-only", Body::empty()))
            .await
            .unwrap(),
        404,
        "PATH_NOT_FOUND",
        false,
    )
    .await;
    let new_request = Request::builder()
        .uri(format!(
            "/api/v2/snapshots/{}/blob?path=/new-only",
            next["descriptor"]["snapshot_id"].as_str().unwrap()
        ))
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-mega-snapshot-lease", next["lease_id"].as_str().unwrap())
        .body(Body::empty())
        .unwrap();
    let response = app(&state).oneshot(new_request).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw.as_slice()
    );
}

#[tokio::test]
async fn mst2_durable_http_renew_release_expiry_and_last_lease_retire_protection() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let another = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/project"))
            .await
            .unwrap(),
    )
    .await;
    let other = another["lease_id"].as_str().unwrap();
    let renewed = lease_control(&fixture, &fixture.lease, "POST", true).await;
    assert_eq!(renewed["snapshot_id"], fixture.snapshot);
    let state = rebuilt(&fixture).await;
    let restored = success_json(
        app(&state)
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(restored["lease_expires_at"], renewed["lease_expires_at"]);
    let first = lease_control(&fixture, &fixture.lease, "DELETE", false).await;
    let second = lease_control(&fixture, &fixture.lease, "DELETE", false).await;
    assert_eq!(first["released"], true);
    assert_eq!(second["released"], false);
    error(
        app(&state)
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
        410,
        "LEASE_EXPIRED",
        false,
    )
    .await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='lease'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='pin'"
        )
        .await,
        1
    );
    let mut request = fixture.request("HEAD", "blob?path=/file", Body::empty());
    request
        .headers_mut()
        .insert("x-mega-snapshot-lease", other.parse().unwrap());
    assert_eq!(app(&state).oneshot(request).await.unwrap().status(), 200);
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mst2_snapshot_lease SET expires_at_unix=0 WHERE lease_id=$1",
        [other.into()],
    ))
    .await
    .unwrap();
    let renew = Request::builder()
        .method("POST")
        .uri(format!("/api/v2/snapshots/leases/{other}/renew"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    error(
        app(&state).oneshot(renew).await.unwrap(),
        410,
        "LEASE_EXPIRED",
        false,
    )
    .await;
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_snapshot_lease WHERE state='EXPIRED'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_retention_root").await,
        0
    );
    let stored =
        crate::callisto::mst2_snapshot_context::Entity::find_by_id(fixture.snapshot.clone())
            .one(db)
            .await
            .unwrap()
            .unwrap();
    let node = format!("page:sha256:{}", hex::encode(stored.metadata_root));
    assert_eq!(
        PostgresRetentionRepository::new(db.clone())
            .mark_deleting("last-lease-gc", &node)
            .await
            .unwrap(),
        GcClaim::Marked
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_metadata_payload").await,
        scalar(db, "SELECT count(*) FROM mst2_metadata_prepare_page").await,
        "release does not delete payload or Git bytes"
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_live_to_deleting_winner_rejects_paused_resolve_without_leak() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let captured = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let resolving = {
        let app = fixture.app.clone();
        let captured = captured.clone();
        let release = release.clone();
        tokio::spawn(async move {
            with_native_resolve_barriers(
                captured,
                release,
                app.oneshot(resolve_request("/project")),
            )
            .await
            .unwrap()
        })
    };
    captured.wait().await;
    lease_control(&fixture, &fixture.lease, "DELETE", false).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let context =
        crate::callisto::mst2_snapshot_context::Entity::find_by_id(fixture.snapshot.clone())
            .one(db)
            .await
            .unwrap()
            .unwrap();
    let node = format!("page:sha256:{}", hex::encode(context.metadata_root));
    let txn = db.begin().await.unwrap();
    assert_eq!(
        PostgresRetentionRepository::mark_deleting_in_txn(&txn, "http-gc-winner", &node)
            .await
            .unwrap(),
        GcClaim::Marked
    );
    let txn_id = scalar(db, "SELECT count(*) FROM mst2_snapshot_lease").await;
    release.wait().await;
    txn.commit().await.unwrap();
    error(resolving.await.unwrap(), 503, "OBJECT_UNAVAILABLE", false).await;
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_snapshot_lease").await,
        txn_id
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_retention_root").await,
        0
    );
}

#[tokio::test]
async fn mst2_durable_http_lease_winner_blocks_gc_until_the_last_release() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let another = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/project"))
            .await
            .unwrap(),
    )
    .await;
    let other = another["lease_id"].as_str().unwrap();
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let stored =
        crate::callisto::mst2_snapshot_context::Entity::find_by_id(fixture.snapshot.clone())
            .one(db)
            .await
            .unwrap()
            .unwrap();
    let node = format!("page:sha256:{}", hex::encode(stored.metadata_root));
    let retention = PostgresRetentionRepository::new(db.clone());
    assert_eq!(
        retention
            .mark_deleting("lease-winner-first", &node)
            .await
            .unwrap(),
        GcClaim::Unavailable
    );
    lease_control(&fixture, &fixture.lease, "DELETE", false).await;
    assert_eq!(
        retention
            .mark_deleting("lease-winner-second", &node)
            .await
            .unwrap(),
        GcClaim::Unavailable
    );
    lease_control(&fixture, other, "DELETE", false).await;
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_retention_root").await,
        0
    );
    assert_eq!(
        retention
            .mark_deleting("lease-winner-final", &node)
            .await
            .unwrap(),
        GcClaim::Marked
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_renew_waits_for_lock_before_checking_database_deadline() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let held = db.begin().await.unwrap();
    held.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1,hashtext(current_schema()))",
        [crate::jupiter::storage::mst2_retention::RETENTION_LOCK_KEY.into()],
    ))
    .await
    .unwrap();
    let renewing = {
        let app = fixture.app.clone();
        let lease = fixture.lease.clone();
        tokio::spawn(async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v2/snapshots/leases/{lease}/renew"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        })
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            held.execute_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT pg_stat_clear_snapshot()".to_owned(),
            ))
            .await
            .unwrap();
            let blocked = scalar(
                &held,
                "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid
              WHERE l.locktype='advisory' AND NOT l.granted AND a.datname=current_database()
                AND a.application_name=current_schema()",
            )
            .await;
            if blocked > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("HTTP renewal did not wait on the retention lock");
    held.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mst2_snapshot_lease SET expires_at_unix=0 WHERE lease_id=$1",
        [fixture.lease.clone().into()],
    ))
    .await
    .unwrap();
    held.commit().await.unwrap();
    error(renewing.await.unwrap(), 410, "LEASE_EXPIRED", false).await;
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_snapshot_lease WHERE state='EXPIRED'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_retention_root").await,
        0
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_publication_advance_rejects_mixed_resolve_then_retries_current() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let captured = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let resolving = {
        let app = fixture.app.clone();
        let captured = captured.clone();
        let release = release.clone();
        tokio::spawn(async move {
            with_native_resolve_barriers(
                captured,
                release,
                app.oneshot(resolve_request("/project")),
            )
            .await
            .unwrap()
        })
    };
    captured.wait().await;
    advance(&fixture).await;
    release.wait().await;
    error(resolving.await.unwrap(), 503, "SNAPSHOT_NOT_READY", true).await;
    let mono = fixture.state.storage.mono_storage();
    assert_eq!(
        scalar(
            mono.get_connection(),
            "SELECT count(*) FROM mst2_snapshot_lease"
        )
        .await,
        1
    );
    let next = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/project"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(next["publication_sequence"], "2");
    assert_ne!(next["descriptor"]["snapshot_id"], fixture.snapshot);
    let state = rebuilt(&fixture).await;
    assert_eq!(
        app(&state)
            .oneshot(fixture.request("HEAD", "blob?path=/file", Body::empty()))
            .await
            .unwrap()
            .status(),
        200
    );
}

#[tokio::test]
async fn mst2_durable_http_install_fault_never_hands_off_or_returns_success() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let prepared = root_metadata(&fixture).await;
    let existing = stored_metadata_ids(db).await;
    let missing: Vec<_> = prepared
        .dag()
        .payloads()
        .iter()
        .filter(|page| !existing.contains(&page.id))
        .map(|page| page.id)
        .collect();
    assert_eq!(missing, [prepared.dag().root()]);
    db.execute_unprepared(
        "CREATE FUNCTION reject_http_page() RETURNS trigger LANGUAGE plpgsql AS $$
      BEGIN RAISE EXCEPTION 'forced HTTP install interruption'; END $$;
      CREATE TRIGGER reject_http_page BEFORE INSERT ON mst2_metadata_payload
      FOR EACH ROW EXECUTE FUNCTION reject_http_page() ",
    )
    .await
    .unwrap();
    error(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/"))
            .await
            .unwrap(),
        503,
        "TEMPORARY_UNAVAILABLE",
        true,
    )
    .await;
    assert_eq!(stored_metadata_ids(db).await, existing);
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_snapshot_context").await,
        1
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_snapshot_lease").await,
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='prepare'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE state='PREPARING'"
        )
        .await,
        1
    );
    assert_eq!(
        fixture
            .send("HEAD", "blob?path=/file", Body::empty())
            .await
            .status(),
        200
    );
    db.execute_unprepared(
        "DROP TRIGGER reject_http_page ON mst2_metadata_payload; DROP FUNCTION reject_http_page() ",
    )
    .await
    .unwrap();
    let success = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(success["descriptor"]["scope"], "/");
    assert_eq!(
        stored_metadata_ids(db).await,
        prepared
            .dag()
            .payloads()
            .iter()
            .map(|page| page.id)
            .collect()
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE state='PREPARING'"
        )
        .await,
        0
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_frame_and_raw_delivery_recheck_after_release() {
    for raw in [false, true] {
        let fixture = Fixture::new_generic_history_with_pg_config(true).await;
        let response = if raw {
            fixture.send("GET", "blob?path=/file", Body::empty()).await
        } else {
            fixture
                .send(
                    "POST",
                    "metadata/pages",
                    Body::from(json!({"items":[{"directory_path":"/"}]}).to_string()),
                )
                .await
        };
        assert_eq!(response.status(), 200);
        let mut stream = response.into_body().into_data_stream();
        let first = stream.next().await.unwrap().unwrap();
        assert!(!first.is_empty());
        if raw {
            assert_eq!(first.len(), 1_048_576);
        }
        lease_control(&fixture, &fixture.lease, "DELETE", false).await;
        assert!(
            stream.next().await.unwrap().is_err(),
            "revocation must suppress the next raw block or END frame"
        );
        assert!(stream.next().await.is_none());
        // Cold raw first earns an immutable receipt, then opens the separate
        // authenticated delivery stream. Revocation still suppresses its tail.
        fixture.counts.assert(
            if raw { 2 } else { 0 },
            if raw {
                fixture.raw.len() + first.len()
            } else {
                0
            },
        );
    }
}

#[tokio::test]
async fn mst2_durable_http_current_state_wrong_lease_and_corrupt_source_reject_warm_reads() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    let warm = success_json(fixture.send("GET", "descriptor", Body::empty()).await).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    for (sid, lease) in [
        (fixture.snapshot.clone(), uuid::Uuid::new_v4().to_string()),
        (format!("sha256:{}", "1".repeat(64)), fixture.lease.clone()),
    ] {
        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/v2/snapshots/{sid}/descriptor"))
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-mega-snapshot-lease", lease)
            .header("if-none-match", format!("\"{}\"", fixture.digest_string()))
            .body(Body::empty())
            .unwrap();
        error(
            fixture.app.clone().oneshot(request).await.unwrap(),
            410,
            "LEASE_EXPIRED",
            false,
        )
        .await;
    }
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mst2_snapshot_context SET authorization_epoch=2 WHERE snapshot_id=$1",
        [fixture.snapshot.clone().into()],
    ))
    .await
    .unwrap();
    let request = fixture.request("GET", "descriptor", Body::empty());
    error(
        fixture.app.clone().oneshot(request).await.unwrap(),
        403,
        "SCOPE_FORBIDDEN",
        false,
    )
    .await;
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mst2_snapshot_context SET authorization_epoch=1 WHERE snapshot_id=$1",
        [fixture.snapshot.clone().into()],
    ))
    .await
    .unwrap();
    db.execute_unprepared("UPDATE mst2_native_publication SET writer_epoch=2")
        .await
        .unwrap();
    error(
        fixture.send("GET", "descriptor", Body::empty()).await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    let state = rebuilt(&fixture).await;
    error(
        app(&state)
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    assert_eq!(warm["snapshot_id"], fixture.snapshot);
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_large_dag_batches_handoff_without_scanning_pages_or_edges() {
    let fixture = Fixture::new_generic_history_with_pg_config_and_directories(true, 80).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let projected = root_metadata(&fixture).await;
    let existing = stored_metadata_ids(db).await;
    assert!(existing.len() > 64);
    let missing: Vec<_> = projected
        .dag()
        .payloads()
        .iter()
        .filter(|page| !existing.contains(&page.id))
        .map(|page| page.id)
        .collect();
    assert_eq!(missing, [projected.dag().root()]);
    let missing_batches = projected
        .dag()
        .payloads()
        .chunks(64)
        .filter(|batch| batch.iter().any(|page| !existing.contains(&page.id)))
        .count() as i64;
    assert_eq!(missing_batches, 1);
    db.execute_unprepared(
        "CREATE TABLE http_install_batch_count(singleton integer PRIMARY KEY,batches bigint NOT NULL,rows bigint NOT NULL);
         INSERT INTO http_install_batch_count VALUES(1,0,0);
         CREATE FUNCTION count_http_install_batch() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN UPDATE http_install_batch_count SET batches=batches+1 WHERE singleton=1; RETURN NULL; END $$;
         CREATE TRIGGER count_http_install_batch AFTER INSERT ON mst2_metadata_payload
         FOR EACH STATEMENT EXECUTE FUNCTION count_http_install_batch();
         CREATE FUNCTION count_http_install_row() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN UPDATE http_install_batch_count SET rows=rows+1 WHERE singleton=1; RETURN NULL; END $$;
         CREATE TRIGGER count_http_install_row AFTER INSERT ON mst2_metadata_payload
         FOR EACH ROW EXECUTE FUNCTION count_http_install_row()",
    ).await.unwrap();
    let prepared = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let resolving = {
        let app = fixture.app.clone();
        let prepared = prepared.clone();
        let release = release.clone();
        tokio::spawn(async move {
            with_native_handoff_barriers(prepared, release, app.oneshot(resolve_request("/")))
                .await
                .unwrap()
        })
    };
    prepared.wait().await;
    let members = scalar(
        db,
        "SELECT count(*) FROM mst2_metadata_prepare_page pp JOIN mst2_metadata_prepare p
        ON p.prepare_id=pp.prepare_id WHERE p.scope='/'",
    )
    .await;
    assert!(members > 64);
    assert_eq!(members as usize, projected.dag().payloads().len());
    assert_eq!(
        scalar(db, "SELECT rows FROM http_install_batch_count").await,
        missing.len() as i64
    );
    assert_eq!(
        stored_metadata_ids(db).await,
        projected
            .dag()
            .payloads()
            .iter()
            .map(|page| page.id)
            .collect()
    );
    assert_eq!(
        scalar(db, "SELECT batches FROM http_install_batch_count").await,
        missing_batches
    );
    let writer = db.begin().await.unwrap();
    assert!(
        PushQueueStorage::try_mono_write_lock(&writer)
            .await
            .unwrap(),
        "full projection and installation completed without retaining the mono writer lock"
    );
    writer.rollback().await.unwrap();
    let blocked_scans = db.begin().await.unwrap();
    blocked_scans.execute_unprepared(
        "LOCK TABLE mst2_metadata_prepare_page,mst2_metadata_payload,mst2_retention_edge IN ACCESS EXCLUSIVE MODE",
    ).await.unwrap();
    release.wait().await;
    let response = tokio::time::timeout(Duration::from_secs(5), resolving)
        .await
        .expect("handoff must not read page membership, payload, or edge tables")
        .unwrap();
    let resolved = success_json(response).await;
    let writer = db.begin().await.unwrap();
    assert!(
        PushQueueStorage::try_mono_write_lock(&writer)
            .await
            .unwrap()
    );
    writer.rollback().await.unwrap();
    blocked_scans.rollback().await.unwrap();
    assert_eq!(resolved["descriptor"]["scope"], "/");
    assert_eq!(
        scalar(db, "SELECT batches FROM http_install_batch_count").await,
        missing_batches
    );
    fixture.counts.assert(0, 0);
    let warm = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request("/"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(
        warm["descriptor"]["snapshot_id"],
        resolved["descriptor"]["snapshot_id"]
    );
    assert_eq!(
        scalar(db, "SELECT batches FROM http_install_batch_count").await,
        missing_batches
    );
    advance(&fixture).await;
    let state = rebuilt(&fixture).await;
    let directory = success_json(
        app(&state)
            .oneshot(fixture.request("GET", "directory?path=/wide-079", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(directory["entries"][0]["name"], "file-079");
    assert_eq!(
        app(&state)
            .oneshot(fixture.request("HEAD", "blob?path=/wide-079/file-079", Body::empty()))
            .await
            .unwrap()
            .status(),
        200
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_durable_http_handoff_rejects_changed_plan_or_receipt_summary() {
    for mutation in [
        "canonical_plan=canonical_plan||decode('00','hex')",
        "projection_revision=projection_revision+1",
        "total_bytes=total_bytes+1",
    ] {
        let fixture = Fixture::new_generic_history_with_pg_config_and_directories(true, 80).await;
        let prepared = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let resolving = {
            let app = fixture.app.clone();
            let prepared = prepared.clone();
            let release = release.clone();
            tokio::spawn(async move {
                with_native_handoff_barriers(prepared, release, app.oneshot(resolve_request("/")))
                    .await
                    .unwrap()
            })
        };
        prepared.wait().await;
        let mono = fixture.state.storage.mono_storage();
        let db = mono.get_connection();
        corrupt_registered_handoff_plan_for_test(db, mutation).await;
        release.wait().await;
        error(resolving.await.unwrap(), 502, "INTEGRITY_ERROR", false).await;
        assert_eq!(
            scalar(db, "SELECT count(*) FROM mst2_snapshot_context").await,
            1
        );
        assert_eq!(
            scalar(db, "SELECT count(*) FROM mst2_snapshot_lease").await,
            1
        );
        assert_eq!(
            scalar(
                db,
                "SELECT count(*) FROM mst2_retention_root WHERE root_kind='lease'"
            )
            .await,
            1
        );
        assert!(
            scalar(
                db,
                "SELECT count(*) FROM mst2_retention_root WHERE root_kind='prepare'"
            )
            .await
                > 64
        );
        assert_eq!(
            fixture
                .send("HEAD", "blob?path=/file", Body::empty())
                .await
                .status(),
            200
        );
        fixture.counts.assert(0, 0);
    }
}

async fn corrupt_registered_handoff_plan_for_test(db: &DatabaseConnection, mutation: &str) {
    let guard_modes = handoff_trigger_modes_for_test(db).await;
    assert!(guard_modes.iter().any(|(table, trigger, mode)| {
        table == "mst2_metadata_prepare"
            && trigger == "mst2_install_capability_prepare_guard"
            && mode == "O"
    }));
    let prepares = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT prepare_id FROM mst2_metadata_prepare WHERE scope='/'",
        ))
        .await
        .unwrap();
    assert_eq!(prepares.len(), 1);
    let prepare_id: String = prepares[0].try_get("", "prepare_id").unwrap();
    let protected_update = || {
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mst2_metadata_prepare SET total_bytes=total_bytes+1 WHERE prepare_id=$1",
            [prepare_id.clone().into()],
        )
    };
    assert!(
        db.execute_raw(protected_update())
            .await
            .unwrap_err()
            .to_string()
            .contains("registered metadata preparation identity is immutable")
    );
    // The isolated fault injection restores this exact production guard in the
    // same transaction. Acquire the statement barrier before ALTER's table lock.
    let txn = db
        .begin_with_config(Some(sea_orm::IsolationLevel::ReadCommitted), None)
        .await
        .unwrap();
    txn.execute_unprepared("SELECT pg_advisory_xact_lock(1296717362,hashtext(current_schema()))")
        .await
        .unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_prepare DISABLE TRIGGER mst2_install_capability_prepare_guard",
    )
    .await
    .unwrap();
    let changed = txn
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!("UPDATE mst2_metadata_prepare SET {mutation} WHERE prepare_id=$1"),
            [prepare_id.clone().into()],
        ))
        .await
        .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_prepare ENABLE TRIGGER mst2_install_capability_prepare_guard",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(handoff_trigger_modes_for_test(db).await, guard_modes);
    assert!(
        db.execute_raw(protected_update())
            .await
            .unwrap_err()
            .to_string()
            .contains("registered metadata preparation identity is immutable")
    );
}

async fn handoff_trigger_modes_for_test(db: &DatabaseConnection) -> Vec<(String, String, String)> {
    db.query_all_raw(Statement::from_string(
        DbBackend::Postgres,
        "SELECT c.relname,t.tgname,t.tgenabled::text AS mode FROM pg_trigger t
         JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=current_schema() AND NOT t.tgisinternal ORDER BY c.relname,t.tgname",
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.try_get("", "relname").unwrap(),
            row.try_get("", "tgname").unwrap(),
            row.try_get("", "mode").unwrap(),
        )
    })
    .collect()
}

async fn overwrite_primary_scope_for_test(db: &DatabaseConnection, storage_uuid: String) {
    // Fault injection owns this isolated schema. Restore the production
    // immutable trigger before commit; failed injection rolls back its DDL.
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_storage_scope DISABLE TRIGGER mst2_metadata_scope_immutable",
    )
    .await
    .unwrap();
    let changed = txn
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mst2_metadata_storage_scope SET storage_uuid=$1 WHERE singleton=1",
            [storage_uuid.into()],
        ))
        .await
        .unwrap();
    assert_eq!(changed.rows_affected(), 1);
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_storage_scope ENABLE TRIGGER mst2_metadata_scope_immutable",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn mst2_durable_http_warm_reads_renew_and_frame_delivery_reject_primary_scope_drift() {
    let fixture = Fixture::new_generic_history_with_pg_config(true).await;
    success_json(fixture.send("GET", "descriptor", Body::empty()).await).await;
    let response = fixture
        .send(
            "POST",
            "metadata/pages",
            Body::from(json!({"items":[{"directory_path":"/nested"}]}).to_string()),
        )
        .await;
    assert_eq!(response.status(), 200);
    let mut body = response.into_body().into_data_stream();
    assert!(body.next().await.unwrap().is_ok());
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let original: String = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT storage_uuid FROM mst2_metadata_storage_scope WHERE singleton=1".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    overwrite_primary_scope_for_test(db, uuid::Uuid::new_v4().to_string()).await;
    error(
        fixture.send("GET", "descriptor", Body::empty()).await,
        500,
        "INTERNAL",
        true,
    )
    .await;
    error(
        fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v2/snapshots/leases/{}/renew", fixture.lease))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
        500,
        "INTERNAL",
        true,
    )
    .await;
    assert!(
        body.next().await.unwrap().is_err(),
        "cached validation cannot emit END after scope drift"
    );
    assert!(body.next().await.is_none());
    overwrite_primary_scope_for_test(db, original).await;
    success_json(fixture.send("GET", "descriptor", Body::empty()).await).await;
    fixture.counts.assert(0, 0);
}

#[path = "snapshot_lookup_metadata_tests.rs"]
mod metadata_lookup;

#[path = "snapshot_persisted_metadata_tests.rs"]
mod persisted_metadata;
