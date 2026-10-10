use sea_orm::{DbBackend, Statement, TransactionTrait};
use tokio::sync::Barrier;

use super::*;
use crate::jupiter::storage::qualified_metadata_family::{
    SnapshotMetadataFamily, with_rooted_reader_barriers, with_rooted_source_fact_barriers,
    with_rooted_source_temporary_shadow,
};

#[path = "snapshot_reader_retention_tests.rs"]
mod reader_retention;

#[path = "snapshot_descriptor_wire_tests.rs"]
mod descriptor_wire;

async fn q_schema(fixture: &Fixture) -> String {
    fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT namespace.metadata_schema FROM mst2_snapshot_storage_route route
             JOIN mst2_metadata_namespace namespace USING(namespace_uuid) WHERE route.snapshot_id=$1
               AND namespace.graph_domain='qualified-v1'",
            [fixture.snapshot.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn q_count(fixture: &Fixture, query: &str) -> i64 {
    let schema = q_schema(fixture).await;
    let query = query.replace("{q}", &format!("\"{}\"", schema.replace('"', "\"\"")));
    fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .query_one_raw(Statement::from_string(DbBackend::Postgres, query))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn resolve(fixture: &Fixture) -> Value {
    success_json(
        fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/snapshots/resolve")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"target":{"kind":"latest"},"scope":"/project"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await
}

async fn release(fixture: &Fixture, lease: &str) -> Value {
    success_json(
        fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/api/v2/snapshots/leases/{lease}"))
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await
}

async fn collect_unowned(fixture: &Fixture) {
    let writer = fixture
        .state
        .storage
        .rooted_qualified_metadata_writer()
        .await
        .unwrap();
    for _ in 0..8 {
        let work = writer.maintenance_tick(64).await.unwrap();
        assert!(work.collector_enabled && work.examined <= 64);
        if q_count(fixture, "SELECT count(*) FROM {q}.mst2_metadata_payload").await == 0 {
            return;
        }
    }
    panic!("bounded maintenance failed to collect the small unowned fixture");
}

#[tokio::test]
async fn default_rooted_http_handoff_renew_release_and_fresh_incarnation_keep_exact_ownership() {
    let fixture = Fixture::new_with_pg_config(true).await;
    assert_eq!(
        fixture
            .state
            .storage
            .snapshot_metadata_family(&fixture.lease, true)
            .await
            .unwrap(),
        Some(SnapshotMetadataFamily::Rooted)
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_qualified_session_incarnation WHERE state='READY'"
        )
        .await,
        1
    );
    let first_generation = q_count(
        &fixture,
        "SELECT root_generation FROM {q}.mst2_qualified_session_incarnation WHERE state='READY'",
    )
    .await;
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('PREPARE','REUSE')").await,0);
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_prepare WHERE plan_kind='ROOTED' AND state='COMMITTED' AND coverage_retired_at IS NOT NULL").await,1);
    let original_payloads =
        q_count(&fixture, "SELECT count(*) FROM {q}.mst2_metadata_payload").await;
    let next = resolve(&fixture).await;
    assert_eq!(next["descriptor"]["snapshot_id"], fixture.snapshot);
    let second = next["lease_id"].as_str().unwrap();
    assert_ne!(second, fixture.lease);
    assert_eq!(
        q_count(&fixture, "SELECT count(*) FROM {q}.mst2_metadata_prepare").await,
        1
    );
    assert_eq!(
        q_count(&fixture, "SELECT count(*) FROM {q}.mst2_metadata_payload").await,
        original_payloads
    );
    let previous_deadline = fixture
        .state
        .storage
        .snapshot_context(&fixture.snapshot, second)
        .await
        .unwrap()
        .lease_expires_at_unix;
    let renewed = fixture
        .state
        .storage
        .snapshot_renew(second, 1)
        .await
        .unwrap();
    assert_eq!(renewed.snapshot_id, fixture.snapshot);
    assert!(renewed.expires_at_unix >= previous_deadline);
    assert_eq!(release(&fixture, &fixture.lease).await["released"], true);
    assert_eq!(release(&fixture, &fixture.lease).await["released"], false);
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_qualified_lease_binding WHERE state='RELEASED' AND lease_epoch=2").await,1);
    let mut request = fixture.request("GET", "descriptor", Body::empty());
    request
        .headers_mut()
        .insert("x-mega-snapshot-lease", second.parse().unwrap());
    success_json(fixture.app.clone().oneshot(request).await.unwrap()).await;
    assert_eq!(release(&fixture, second).await["released"], true);
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_root_anchor"
        )
        .await,
        0
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_qualified_session_incarnation WHERE state='RETIRED'"
        )
        .await,
        1
    );
    collect_unowned(&fixture).await;
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_graph_node"
        )
        .await,
        0
    );
    assert!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_page_certificate"
        )
        .await
            > 0
    );
    let fresh = resolve(&fixture).await;
    assert_eq!(fresh["descriptor"]["snapshot_id"], fixture.snapshot);
    assert_ne!(fresh["lease_id"], next["lease_id"]);
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_qualified_session_incarnation"
        )
        .await,
        2
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_qualified_session_incarnation WHERE state='READY'"
        )
        .await,
        1
    );
    assert_eq!(
        q_count(&fixture, "SELECT count(*) FROM {q}.mst2_metadata_payload").await,
        original_payloads
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT root_generation FROM {q}.mst2_qualified_session_incarnation WHERE state='READY'"
        )
        .await,
        first_generation + 1
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn rooted_wide_directory_windows_and_lookup_survive_rebuild_with_valid_proofs() {
    let fixture = Fixture::new_with_pg_config_and_directories(true, 140).await;
    let config = fixture.state.storage.config();
    let connection = crate::jupiter::storage::init::postgres_connection(&config.database)
        .await
        .unwrap();
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
    let app = Router::new().nest("/api/v2", routers(state.clone()).with_state(state));
    let mut cursor = None;
    let mut names = Vec::new();
    for _ in 0..12 {
        let mut suffix = "directory?path=/&limit=17".to_string();
        if let Some(current) = &cursor {
            suffix.push_str(&format!("&cursor={current}"));
        }
        let response = app
            .clone()
            .oneshot(fixture.request("GET", &suffix, Body::empty()))
            .await
            .unwrap();
        let window = success_json(response).await;
        assert_eq!(window["entry_count"], "147");
        let entries = window["entries"].as_array().unwrap();
        assert!(!entries.is_empty() && entries.len() <= 17);
        names.extend(
            entries
                .iter()
                .map(|entry| entry["name"].as_str().unwrap().to_owned()),
        );
        for proof in window["proof_pages"].as_array().unwrap() {
            let bytes = STANDARD
                .decode(proof["data_base64"].as_str().unwrap())
                .unwrap();
            mst2_codec::metapage::Page::decode(&bytes).unwrap();
            assert_eq!(
                proof["digest"],
                format!("sha256:{}", hex_of(&mst2_codec::metapage::page_id(&bytes)))
            );
        }
        cursor = window["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert!(cursor.is_none());
    assert_eq!(names.len(), 147);
    assert!(
        names
            .windows(2)
            .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
    );
    let lookup=success_json(app.oneshot(fixture.request("POST","lookup",
        Body::from(json!({"paths":["/nested/file","/wide-139/file-139","/missing","/file/child","/link/child"]}).to_string())))
        .await.unwrap()).await;
    let statuses: Vec<_> = lookup["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        [
            "found",
            "found",
            "absent",
            "not_directory",
            "symlink_traversal"
        ]
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='ACTIVE'"
        )
        .await,
        0
    );
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,0);
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn rooted_reader_release_race_keeps_independent_roots_until_owned_buffers_finish() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let admitted = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let request = fixture.request(
        "POST",
        "metadata/pages",
        Body::from(
            json!({"encoding":"identity","items":[{"directory_path":"/nested"}]}).to_string(),
        ),
    );
    let app = fixture.app.clone();
    let pending = tokio::spawn(with_rooted_reader_barriers(
        admitted.clone(),
        resume.clone(),
        async move { app.oneshot(request).await.unwrap() },
    ));
    tokio::time::timeout(Duration::from_secs(4), admitted.wait())
        .await
        .unwrap();
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,2);
    let work = fixture
        .state
        .storage
        .rooted_qualified_metadata_writer()
        .await
        .unwrap()
        .maintenance_tick(64)
        .await
        .unwrap();
    assert_eq!(work.payload_pages_removed, 0);
    assert_eq!(release(&fixture, &fixture.lease).await["released"], true);
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,2);
    let work = fixture
        .state
        .storage
        .rooted_qualified_metadata_writer()
        .await
        .unwrap()
        .maintenance_tick(64)
        .await
        .unwrap();
    assert_eq!(work.payload_pages_removed, 0);
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind='LEASE'"
        )
        .await,
        1
    );
    resume.wait().await;
    error(
        tokio::time::timeout(Duration::from_secs(4), pending)
            .await
            .unwrap()
            .unwrap(),
        410,
        "LEASE_EXPIRED",
        false,
    )
    .await;
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='FINISHED'"
        )
        .await,
        1
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_root_anchor"
        )
        .await,
        0
    );
    collect_unowned(&fixture).await;
    fixture.counts.assert(0, 0);
}

async fn source_oid(fixture: &Fixture) -> String {
    let schema = q_schema(fixture).await;
    let quoted = format!("\"{}\"", schema.replace('"', "\"\""));
    fixture.state.storage.mono_storage().get_connection().query_one_raw(Statement::from_string(DbBackend::Postgres,
        format!("SELECT split_part(a.tagged_tree_oid,':',2) FROM {quoted}.mst2_qualified_session_incarnation s
            JOIN {quoted}.mst2_metadata_source_root_attestation a ON a.attestation_id=s.attestation_id WHERE s.state='READY'")))
        .await.unwrap().unwrap().try_get_by_index(0).unwrap()
}

async fn source_revision(fixture: &Fixture, oid: &str) -> String {
    fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT revision::text FROM mst2_rooted_source_tree_revision WHERE tree_id=$1",
            [oid.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

#[tokio::test]
async fn actual_q_body_callers_preserve_unchanged_source_revision_and_ignore_temp_shadow() {
    let fixture = Fixture::new_with_pg_config_and_directories(true, 140).await;
    fixture.map("/file").await;
    fixture.counts.reset();
    let oid = source_oid(&fixture).await;
    let revision = source_revision(&fixture, &oid).await;
    fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mega_tree SET sub_trees=sub_trees,pack_offset=pack_offset WHERE tree_id=$1",
            [oid.clone().into()],
        ))
        .await
        .unwrap();
    assert_eq!(source_revision(&fixture, &oid).await, revision);
    let response =
        with_rooted_source_temporary_shadow(fixture.send("HEAD", "blob?path=/file", Body::empty()))
            .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-mega-content-size"],
        fixture.raw.len().to_string()
    );
    fixture.counts.assert(0, 0);
    let response =
        with_rooted_source_temporary_shadow(fixture.send("GET", "blob?path=/file", Body::empty()))
            .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        to_bytes(response.into_body(), fixture.raw.len() + 1)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='ACTIVE'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn actual_q_body_callers_reject_changed_or_deleted_source_before_opening_a_body() {
    for delete in [false, true] {
        let fixture = Fixture::new_with_pg_config(true).await;
        let oid = source_oid(&fixture).await;
        let revision = source_revision(&fixture, &oid).await;
        fixture
            .state
            .storage
            .mono_storage()
            .get_connection()
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                if delete {
                    "DELETE FROM mega_tree WHERE tree_id=$1"
                } else {
                    "UPDATE mega_tree SET sub_trees=decode('00','hex') WHERE tree_id=$1"
                },
                [oid.clone().into()],
            ))
            .await
            .unwrap();
        assert_ne!(source_revision(&fixture, &oid).await, revision);
        let response = fixture.send("GET", "blob?path=/file", Body::empty()).await;
        assert!(response.status().is_client_error() || response.status().is_server_error());
        fixture.counts.assert(0, 0);
    }
}

#[tokio::test]
async fn actual_q_body_callers_recheck_only_returned_current_file_facts() {
    let fixture = Fixture::new_with_pg_config(true).await;
    fixture.state.storage.mono_storage().get_connection().execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE mst2_verified_object SET verification_version=1 WHERE storage_domain='git' AND object_kind='blob' AND git_oid=$1",
        [fixture.oid.clone().into()])).await.unwrap();
    let untouched = fixture
        .send("HEAD", "blob?path=/empty", Body::empty())
        .await;
    assert_eq!(untouched.status(), 200);
    assert_eq!(untouched.headers()["x-mega-content-size"], "0");
    error(
        fixture.send("GET", "blob?path=/file", Body::empty()).await,
        503,
        "METADATA_NOT_READY",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn actual_q_body_caller_source_mutation_after_reader_admission_cannot_serve_stale_content() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let oid = source_oid(&fixture).await;
    let admitted = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let app = fixture.app.clone();
    let request = fixture.request("GET", "blob?path=/file", Body::empty());
    let pending = tokio::spawn(with_rooted_reader_barriers(
        admitted.clone(),
        resume.clone(),
        async move { app.oneshot(request).await.unwrap() },
    ));
    tokio::time::timeout(Duration::from_secs(4), admitted.wait())
        .await
        .unwrap();
    fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mega_tree SET sub_trees=decode('00','hex') WHERE tree_id=$1",
            [oid.into()],
        ))
        .await
        .unwrap();
    resume.wait().await;
    let response = tokio::time::timeout(Duration::from_secs(4), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(response.status().is_client_error() || response.status().is_server_error());
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,0);
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='FINISHED'"
        )
        .await,
        1
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn actual_q_body_caller_retries_a_busy_current_file_fact_without_opening_a_body() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let writer = fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .begin()
        .await
        .unwrap();
    writer.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mst2_verified_object SET raw_sha256=raw_sha256 WHERE storage_domain='git' AND object_kind='blob' AND git_oid=$1",
        [fixture.oid.clone().into()],
    )).await.unwrap();
    error(
        fixture.send("GET", "blob?path=/file", Body::empty()).await,
        503,
        "TEMPORARY_UNAVAILABLE",
        true,
    )
    .await;
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,0);
    fixture.counts.assert(0, 0);
    writer.rollback().await.unwrap();
    assert_eq!(
        fixture
            .send("HEAD", "blob?path=/file", Body::empty())
            .await
            .status(),
        200
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn actual_q_body_caller_holds_the_selected_current_fact_until_the_read_transaction_finishes()
{
    let fixture = Fixture::new_with_pg_config(true).await;
    let admitted = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let app = fixture.app.clone();
    let request = fixture.request("HEAD", "blob?path=/file", Body::empty());
    let pending = tokio::spawn(with_rooted_source_fact_barriers(
        admitted.clone(),
        resume.clone(),
        async move { app.oneshot(request).await.unwrap() },
    ));
    tokio::time::timeout(Duration::from_secs(4), admitted.wait())
        .await
        .unwrap();
    let core_writer = fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .clone();
    let oid = fixture.oid.clone();
    let (ready, received) = tokio::sync::oneshot::channel();
    let update = tokio::spawn(async move {
        let txn = core_writer.begin().await.unwrap();
        let pid: i32 = txn
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT pg_backend_pid()",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index(0)
            .unwrap();
        ready.send(pid).unwrap();
        txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
            "UPDATE mst2_verified_object SET raw_sha256=raw_sha256 WHERE storage_domain='git' AND object_kind='blob' AND git_oid=$1",
            [oid.into()])).await.unwrap();
        txn.commit().await.unwrap();
    });
    let pid = received.await.unwrap();
    tokio::time::timeout(Duration::from_secs(4),async {
        loop {
            let waiting:bool=fixture.state.storage.mono_storage().get_connection().query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,"SELECT coalesce(wait_event_type='Lock',false) FROM pg_stat_activity WHERE pid=$1",[pid.into()]))
                .await.unwrap().unwrap().try_get_by_index(0).unwrap();
            if waiting {break;} tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(!update.is_finished());
    resume.wait().await;
    let response = tokio::time::timeout(Duration::from_secs(4), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 200);
    tokio::time::timeout(Duration::from_secs(4), update)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,0);
    fixture.counts.assert(0, 0);
}

async fn durable_lease_state(fixture: &Fixture) -> Value {
    let schema = q_schema(fixture).await;
    let quoted = format!("\"{}\"", schema.replace('"', "\"\""));
    fixture.state.storage.mono_storage().get_connection().query_one_raw(Statement::from_string(DbBackend::Postgres,
        format!("SELECT jsonb_build_object(
          'sessions',(SELECT jsonb_agg(to_jsonb(s) ORDER BY s.snapshot_id,s.session_incarnation) FROM {quoted}.mst2_qualified_session_incarnation s),
          'leases',(SELECT jsonb_agg(to_jsonb(l) ORDER BY l.lease_id) FROM {quoted}.mst2_qualified_lease_binding l),
          'anchors',(SELECT jsonb_agg(to_jsonb(a) ORDER BY a.anchor_id) FROM {quoted}.mst2_metadata_root_anchor a),
          'roots',(SELECT jsonb_agg(to_jsonb(r) ORDER BY r.prepare_id,r.page_id,r.generation) FROM {quoted}.mst2_metadata_graph_root r),
          'routes',(SELECT jsonb_agg(to_jsonb(r) ORDER BY r.lease_id) FROM mst2_lease_storage_route r))")))
        .await.unwrap().unwrap().try_get_by_index(0).unwrap()
}

#[tokio::test]
async fn actual_q_lease_http_and_direct_handoff_retry_source_lock_without_partial_durable_mutation()
{
    let fixture = Fixture::new_with_pg_config(true).await;
    let storage = &fixture.state.storage;
    let context = storage
        .snapshot_context(&fixture.snapshot, &fixture.lease)
        .await
        .unwrap();
    let config = storage.config();
    let instance = config.mst2.instance_uuid.as_deref().unwrap();
    let head = storage
        .mono_storage()
        .read_native_publication_head(instance)
        .await
        .unwrap();
    let repository = storage.rooted_qualified_metadata_writer().await.unwrap();
    let before = durable_lease_state(&fixture).await;
    let revision = source_revision(&fixture, &source_oid(&fixture).await).await;
    let writer = storage
        .mono_storage()
        .get_connection()
        .begin()
        .await
        .unwrap();
    let locked=writer.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT revision::text FROM mst2_rooted_source_tree_revision WHERE revision=$1::uuid FOR UPDATE",
        [revision.clone().into()])).await.unwrap().unwrap();
    assert_eq!(locked.try_get_by_index::<String>(0).unwrap(), revision);
    let direct_errors = tokio::time::timeout(Duration::from_secs(4), async {
        [
            repository
                .open_session(&head, &context.built, None, 600)
                .await
                .unwrap_err(),
            repository
                .renew(&fixture.lease, 600, instance)
                .await
                .unwrap_err(),
            repository.release(&fixture.lease).await.unwrap_err(),
        ]
    })
    .await
    .unwrap();
    for error in direct_errors {
        assert_eq!(error.code, SnapshotErrorCode::TemporaryUnavailable);
    }
    assert_eq!(durable_lease_state(&fixture).await, before);
    for (method, uri) in [
        ("POST", "/api/v2/snapshots/resolve".to_owned()),
        (
            "POST",
            format!("/api/v2/snapshots/leases/{}/renew", fixture.lease),
        ),
        (
            "DELETE",
            format!("/api/v2/snapshots/leases/{}", fixture.lease),
        ),
    ] {
        let body = if uri.ends_with("/resolve") {
            Body::from(json!({"target":{"kind":"latest"},"scope":"/project"}).to_string())
        } else {
            Body::empty()
        };
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(body)
            .unwrap();
        let response =
            tokio::time::timeout(Duration::from_secs(4), fixture.app.clone().oneshot(request))
                .await
                .unwrap()
                .unwrap();
        error(response, 503, "TEMPORARY_UNAVAILABLE", true).await;
        assert_eq!(durable_lease_state(&fixture).await, before);
    }
    writer.rollback().await.unwrap();
    assert_eq!(durable_lease_state(&fixture).await, before);
    let next = resolve(&fixture).await;
    assert_ne!(next["lease_id"], fixture.lease);
    assert_eq!(release(&fixture, &fixture.lease).await["released"], true);
}
