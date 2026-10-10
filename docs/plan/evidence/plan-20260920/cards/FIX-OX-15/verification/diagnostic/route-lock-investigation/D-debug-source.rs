use sea_orm::{DatabaseConnection, DbBackend, IsolationLevel, Statement, TransactionTrait};
use tokio::{sync::Notify, time::timeout};

use super::*;
use crate::{
    ceres::snapshot::{
        chunks::{ChunkMapSource, ChunkProjection},
        content_budget::MemoryBudget,
    },
    jupiter::storage::native_chunk_map::PostgresChunkMapRepository,
};

fn statement<const N: usize>(sql: &str, values: [sea_orm::Value; N]) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
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

async fn reconstructed(fixture: &Fixture) -> MonoApiServiceState {
    let mut config = (*fixture.state.storage.config()).clone();
    config.database.max_connection = 1;
    config.database.min_connection = 1;
    let config = Arc::new(config);
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
    MonoApiServiceState {
        storage,
        git_object_cache: Arc::new(GitObjectCache {
            connection: fixture.state.git_object_cache.connection.clone(),
            prefix: uuid::Uuid::new_v4().to_string(),
        }),
        ..fixture.state.clone()
    }
}

fn router(state: &MonoApiServiceState) -> Router {
    Router::new().nest("/api/v2", routers(state.clone()).with_state(state.clone()))
}

#[tokio::test]
async fn persisted_map_rebuilt_actual_http_uses_canonical_pages_and_only_requested_raw_range() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let original = fixture.map("/file").await;
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
    let oracle = ChunkProjection::build(fixture.digest, fixture.raw.clone()).unwrap();
    assert_eq!(
        original["map"]["map_id"],
        format!("sha256:{}", hex_of(&oracle.map_id))
    );
    let state = reconstructed(&fixture).await;
    assert!(state.storage.native_chunk_maps.get().is_none());
    let app = router(&state);
    fixture.counts.reset();
    let map = success_json(
        app.clone()
            .oneshot(fixture.request("GET", "chunk-map?path=/alias", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(map["map"], original["map"]);
    let map_id = map["map"]["map_id"].as_str().unwrap();
    let page = success_json(
        app.clone()
            .oneshot(fixture.request(
                "GET",
                &format!("chunk-map/pages?path=/alias&map_id={map_id}&page_index=0"),
                Body::empty(),
            ))
            .await
            .unwrap(),
    )
    .await;
    let bytes = STANDARD
        .decode(page["leaf_base64"].as_str().unwrap())
        .unwrap();
    let (leaf, proof) = oracle.leaf_and_proof(0).unwrap();
    assert_eq!(bytes, leaf.encode().unwrap());
    assert!(proof.is_empty());
    assert_eq!(page["proof"], json!([]));
    fixture.counts.assert(0, 0);
    assert!(fixture.counts.receipt_reads.load(Ordering::SeqCst) >= 2);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    let body = fixture.chunk_body("/alias", map_id, "1").to_string();
    let response = app
        .oneshot(fixture.request("POST", "chunks", Body::from(body.clone())))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let wire = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let frames = parse_stream(&wire).unwrap();
    let [Frame::Chunk(chunk), Frame::End(end)] = frames.as_slice() else {
        panic!("expected exact CHUNK and END");
    };
    assert_eq!(chunk.chunk_bytes, fixture.raw[CHUNK_SIZE as usize..]);
    assert_eq!(chunk.chunk_index, 1);
    assert_eq!(chunk.file_content_id, fixture.digest);
    assert_eq!(chunk.map_id, oracle.map_id);
    assert_eq!(end.request_item_count, 1);
    assert_eq!(end.unique_unit_count, 1);
    assert_eq!(end.logical_bytes, 113);
    assert_eq!(
        end.request_body_sha256,
        <[u8; 32]>::from(Sha256::digest(body.as_bytes()))
    );
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.bytes.load(Ordering::SeqCst), 113);
}

#[tokio::test]
async fn persisted_map_current_fact_tuple_receipt_and_leaf_corruption_fail_closed_without_fallback()
{
    let fixture = Fixture::new().await;
    let map = fixture.map("/file").await;
    let original = fixture.fact().await;
    let map_id = map["map"]["map_id"].as_str().unwrap();
    fixture.counts.reset();
    for case in 0..3 {
        let mut fact = original.clone();
        match case {
            0 => fact.id += 1_000_000,
            1 => fact.created_at += chrono::Duration::seconds(1),
            _ => fact.raw_sha256[0] ^= 1,
        }
        fixture.replace_fact(fact).await;
        error(
            fixture
                .send("GET", "chunk-map?path=/file", Body::empty())
                .await,
            502,
            "INTEGRITY_ERROR",
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    fixture.replace_fact(original).await;
    fixture
        .counts
        .receipt_read_failure
        .store(true, Ordering::SeqCst);
    error(
        fixture
            .send("GET", "chunk-map?path=/file", Body::empty())
            .await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture
        .counts
        .receipt_read_failure
        .store(false, Ordering::SeqCst);
    *fixture.counts.receipt_read_corruption.lock().unwrap() =
        Some(Bytes::from_static(b"forged DB proof"));
    error(
        fixture
            .send("GET", "chunk-map?path=/file", Body::empty())
            .await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    *fixture.counts.receipt_read_corruption.lock().unwrap() = None;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let leaf: Vec<u8> = db
        .query_one_raw(statement(
            "SELECT payload FROM mst2_chunk_map_leaf WHERE map_id=$1 AND page_index=0",
            [hex::decode(map_id.trim_start_matches("sha256:"))
                .unwrap()
                .into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "payload")
        .unwrap();
    db.execute_unprepared("ALTER TABLE mst2_chunk_map_leaf DISABLE TRIGGER USER")
        .await
        .unwrap();
    let mut bad = leaf.clone();
    bad[16] ^= 1;
    db.execute_raw(statement(
        "UPDATE mst2_chunk_map_leaf SET payload=$1",
        [bad.into()],
    ))
    .await
    .unwrap();
    db.execute_unprepared("ALTER TABLE mst2_chunk_map_leaf ENABLE TRIGGER USER")
        .await
        .unwrap();
    for (method, suffix, body) in [
        (
            "GET",
            format!("chunk-map/pages?path=/file&map_id={map_id}&page_index=0"),
            Body::empty(),
        ),
        (
            "POST",
            "chunks".to_string(),
            Body::from(fixture.chunk_body("/file", map_id, "0").to_string()),
        ),
    ] {
        error(
            fixture.send(method, &suffix, body).await,
            502,
            "INTEGRITY_ERROR",
            false,
        )
        .await;
    }
    fixture.counts.assert(0, 0);
    db.execute_unprepared("ALTER TABLE mst2_chunk_map_leaf DISABLE TRIGGER USER")
        .await
        .unwrap();
    db.execute_raw(statement(
        "UPDATE mst2_chunk_map_leaf SET payload=$1",
        [leaf.into()],
    ))
    .await
    .unwrap();
    db.execute_unprepared("ALTER TABLE mst2_chunk_map_leaf ENABLE TRIGGER USER")
        .await
        .unwrap();
    assert_eq!(fixture.map("/file").await["map"], map["map"]);
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn ordinary_db_dml_cannot_forge_full_body_admission_or_mutate_admitted_indexes() {
    let fixture = Fixture::new().await;
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let repository = PostgresChunkMapRepository::new(db.clone()).await.unwrap();
    let scope = repository.test_primary_scope();
    let source_bytes = source.canonical_bytes().unwrap();
    let leaf = ChunkLeaf {
        page_index: 0,
        chunk_sha256: vec![[9; 32]; 2],
    };
    let root = leaf.leaf_hash().unwrap();
    let map = mst2_codec::chunkmap::ChunkMap::new(fixture.digest, fixture.raw.len() as u64, root)
        .unwrap();
    let mut receipt = b"MST2-CHUNK-MAP-RECEIPT\0".to_vec();
    receipt.extend_from_slice(&(scope.len() as u32).to_be_bytes());
    receipt.extend_from_slice(scope);
    receipt.extend_from_slice(&(source_bytes.len() as u32).to_be_bytes());
    receipt.extend_from_slice(&source_bytes);
    let source_id: [u8; 32] = Sha256::digest(&receipt).into();
    receipt.extend_from_slice(&map.encode());
    let txn = db
        .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
        .await
        .unwrap();
    txn.execute_raw(statement(
        "INSERT INTO mst2_chunk_map(map_id,descriptor,page_count,pages_root) VALUES($1,$2,1,$3)",
        [
            map.map_id().to_vec().into(),
            map.encode().into(),
            root.to_vec().into(),
        ],
    ))
    .await
    .unwrap();
    txn.execute_raw(statement(
        "INSERT INTO mst2_chunk_map_leaf(map_id,page_index,payload) VALUES($1,0,$2)",
        [map.map_id().to_vec().into(), leaf.encode().unwrap().into()],
    ))
    .await
    .unwrap();
    txn.execute_raw(statement(
        "INSERT INTO mst2_chunk_map_node(map_id,first_page,page_count,digest) VALUES($1,0,1,$2)",
        [map.map_id().to_vec().into(), root.to_vec().into()],
    ))
    .await
    .unwrap();
    txn.execute_raw(statement("INSERT INTO mst2_chunk_map_source(storage_domain,git_oid,object_kind,fact_id,source_id,source_bytes,primary_scope,map_id,receipt_digest) VALUES('git',$1,'blob',$2,$3,$4,$5,$6,$7)", [fixture.oid.clone().into(),source.fact().id.into(),source_id.to_vec().into(),source_bytes.into(),scope.to_vec().into(),map.map_id().to_vec().into(),Sha256::digest(&receipt).to_vec().into()])).await.unwrap();
    txn.commit().await.unwrap();
    // All row checks and even a self-computed receipt digest pass. They
    // still cannot create the trusted writer's independent object receipt.
    fixture.counts.reset();
    error(
        fixture
            .send("GET", "chunk-map?path=/file", Body::empty())
            .await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    for table in [
        "mst2_chunk_map",
        "mst2_chunk_map_leaf",
        "mst2_chunk_map_node",
        "mst2_chunk_map_source",
    ] {
        assert!(
            db.execute_unprepared(&format!("DELETE FROM {table}"))
                .await
                .is_err()
        );
        assert!(
            db.execute_unprepared(&format!("TRUNCATE {table} CASCADE"))
                .await
                .is_err()
        );
    }
    assert!(
        db.execute_unprepared("UPDATE mst2_chunk_map_source SET fact_id=fact_id")
            .await
            .is_err()
    );
    assert!(db.execute_raw(statement("INSERT INTO mst2_chunk_map_node(map_id,first_page,page_count,digest) VALUES($1,1,1,$2)", [map.map_id().to_vec().into(),root.to_vec().into()])).await.is_err());
}

#[tokio::test]
async fn receipt_orphan_failure_and_cancelled_install_release_owned_credit_and_replay_atomically() {
    for cancel in [false, true] {
        let fixture = Fixture::new().await;
        let mono = fixture.state.storage.mono_storage();
        let budget = MemoryBudget::new(8 * 1024 * 1024);
        let repository = PostgresChunkMapRepository::new(mono.get_connection().clone())
            .await
            .unwrap()
            .with_test_budget(budget.clone());
        assert!(
            fixture
                .state
                .storage
                .native_chunk_maps
                .set(repository)
                .is_ok()
        );
        if cancel {
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            *fixture.counts.receipt_write_holds.lock().unwrap() = Some((entered.clone(), release));
            let app = fixture.app.clone();
            let request = fixture.request("GET", "chunk-map?path=/file", Body::empty());
            let task = tokio::spawn(async move { app.oneshot(request).await });
            timeout(Duration::from_secs(10), entered.notified())
                .await
                .unwrap();
            assert!(budget.used() > 4 * 1024 * 1024);
            assert_eq!(
                count(mono.get_connection(), "mst2_chunk_map_source").await,
                0
            );
            task.abort();
            assert!(task.await.err().unwrap().is_cancelled());
            *fixture.counts.receipt_write_holds.lock().unwrap() = None;
        } else {
            fixture
                .counts
                .receipt_write_fail_after_create
                .store(true, Ordering::SeqCst);
            error(
                fixture
                    .send("GET", "chunk-map?path=/file", Body::empty())
                    .await,
                503,
                "TEMPORARY_UNAVAILABLE",
                true,
            )
            .await;
            fixture
                .counts
                .receipt_write_fail_after_create
                .store(false, Ordering::SeqCst);
        }
        assert_eq!(budget.used(), 0);
        fixture.counts.assert(1, fixture.raw.len());
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
        assert_eq!(count(mono.get_connection(), "mst2_chunk_map").await, 0);
        fixture.counts.reset();
        fixture.map("/file").await;
        fixture.counts.assert(1, fixture.raw.len());
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
        assert_eq!(count(mono.get_connection(), "mst2_chunk_map").await, 1);
        assert_eq!(
            count(mono.get_connection(), "mst2_chunk_map_source").await,
            1
        );
        assert_eq!(count(mono.get_connection(), "mst2_chunk_map_leaf").await, 1);
        assert_eq!(count(mono.get_connection(), "mst2_chunk_map_node").await, 1);
        fixture.counts.reset();
        fixture.map("/file").await;
        fixture.counts.assert(0, 0);
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
        assert_eq!(budget.used(), 0);
    }
}

#[tokio::test]
async fn map_json_transport_bytes_keep_owned_credit_until_the_last_clone_drops() {
    let budget = MemoryBudget::new(4096);
    let bytes = super::super::map_json_bytes(
        &json!({"map_id":"sha256:owned"}),
        budget.reserve(4096).unwrap(),
    )
    .unwrap();
    let mut stream = Body::from(bytes).into_data_stream();
    let transport = stream.next().await.unwrap().unwrap();
    let clone = transport.clone();
    drop(transport);
    drop(stream);
    assert_eq!(budget.used(), 4096);
    assert!(budget.reserve(1).is_err());
    drop(clone);
    assert_eq!(budget.used(), 0);
    assert!(budget.reserve(4096).is_ok());
}

#[test]
fn json_wire_limit_rejects_growth_and_refunds_credit() {
    let budget = MemoryBudget::new(4096);
    let value = json!({"path":"\u{1}".repeat(1000)});
    let error = super::super::map_json_bytes(&value, budget.reserve(4096).unwrap())
        .err()
        .unwrap();
    assert_eq!(error.code, SnapshotErrorCode::Internal);
    assert_eq!(budget.used(), 0);
}

async fn observer(fixture: &Fixture) -> crate::ceres::snapshot::chunk_map_gate::InstallFlight {
    let repository = fixture.state.storage.chunk_maps().await.unwrap();
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    crate::ceres::snapshot::chunk_map_gate::InstallFlight::acquire(
        repository.source_identity(&source).unwrap(),
    )
    .unwrap()
}

async fn wait_owners(
    flight: &crate::ceres::snapshot::chunk_map_gate::InstallFlight,
    owners: usize,
) {
    timeout(Duration::from_secs(10), async {
        loop {
            if flight.test_owner_count() == owners {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "actual HTTP callers did not reach the same-source install gate: expected {owners} owners, observed {}",
            flight.test_owner_count()
        )
    });
}

fn held_leader(fixture: &Fixture) -> (Arc<Notify>, Arc<Notify>, tokio::task::JoinHandle<Response>) {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    *fixture.counts.receipt_write_holds.lock().unwrap() = Some((entered.clone(), release.clone()));
    let app = fixture.app.clone();
    let request = fixture.request("GET", "chunk-map?path=/file", Body::empty());
    let task = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    (entered, release, task)
}

#[tokio::test]
async fn same_source_cold_actual_http_callers_share_one_full_pass_and_each_recheck_their_receipt() {
    let fixture = Fixture::new().await;
    let budget = MemoryBudget::new(8 * 1024 * 1024);
    let repository = PostgresChunkMapRepository::new(
        fixture
            .state
            .storage
            .mono_storage()
            .get_connection()
            .clone(),
    )
    .await
    .unwrap()
    .with_test_budget(budget.clone());
    assert!(
        fixture
            .state
            .storage
            .native_chunk_maps
            .set(repository)
            .is_ok()
    );
    let (entered, release, leader) = held_leader(&fixture);
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    assert!(budget.used() > 4 * 1024 * 1024);
    let flight = observer(&fixture).await;
    let mut joined = Vec::new();
    for _ in 0..6 {
        let app = fixture.app.clone();
        let request = fixture.request("GET", "chunk-map?path=/alias", Body::empty());
        joined.push(tokio::spawn(
            async move { app.oneshot(request).await.unwrap() },
        ));
    }
    let other_counts = Arc::new(ReadCounts::default());
    other_counts
        .receipt_read_failure
        .store(true, Ordering::SeqCst);
    let mut other_state = fixture.state.clone();
    other_state.storage.git_service = GitService {
        obj_storage: MegaObjectStorageWrapper::new(Arc::new(CountingStorage {
            inner: fixture.state.storage.git_service.obj_storage.clone(),
            counts: other_counts.clone(),
        })),
    };
    let other_app = router(&other_state);
    let request = fixture.request("GET", "chunk-map?path=/file", Body::empty());
    let rejected = tokio::spawn(async move { other_app.oneshot(request).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(500)).await;
    eprintln!(
        "FIX-OX-15 diagnostic: owners={}, whole={}, receipt_reads={}, receipt_writes={}, same_finished={:?}, other_finished={}",
        flight.test_owner_count(),
        fixture.counts.whole.load(Ordering::SeqCst),
        fixture.counts.receipt_reads.load(Ordering::SeqCst),
        fixture.counts.receipt_writes.load(Ordering::SeqCst),
        joined.iter().map(tokio::task::JoinHandle::is_finished).collect::<Vec<_>>(),
        rejected.is_finished(),
    );
    let reached_callers = timeout(Duration::from_secs(10), async {
        loop {
            if flight.test_owner_count() == 9 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    eprintln!(
        "FIX-OX-15 timeout diagnostic: reached_callers={reached_callers}, owners={}, whole={}, receipt_reads={}, receipt_writes={}, same_finished={:?}, other_finished={}",
        flight.test_owner_count(),
        fixture.counts.whole.load(Ordering::SeqCst),
        fixture.counts.receipt_reads.load(Ordering::SeqCst),
        fixture.counts.receipt_writes.load(Ordering::SeqCst),
        joined.iter().map(tokio::task::JoinHandle::is_finished).collect::<Vec<_>>(),
        rejected.is_finished(),
    );
    assert!(reached_callers, "same-source callers never reached install gate");
    fixture.counts.assert(1, fixture.raw.len());
    release.notify_one();
    let expected = success_json(leader.await.unwrap()).await;
    for task in joined {
        assert_eq!(
            success_json(task.await.unwrap()).await["map"],
            expected["map"]
        );
    }
    error(rejected.await.unwrap(), 502, "INTEGRITY_ERROR", false).await;
    assert_eq!(other_counts.receipt_reads.load(Ordering::SeqCst), 1);
    assert_eq!(other_counts.whole.load(Ordering::SeqCst), 0);
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 8);
    wait_owners(&flight, 1).await;
}

#[tokio::test]
async fn failed_or_cancelled_leader_and_cancelled_waiter_leave_actual_http_retry_capacity() {
    for mode in 0..3 {
        let fixture = Fixture::new().await;
        let budget = MemoryBudget::new(8 * 1024 * 1024);
        let repository = PostgresChunkMapRepository::new(
            fixture
                .state
                .storage
                .mono_storage()
                .get_connection()
                .clone(),
        )
        .await
        .unwrap()
        .with_test_budget(budget.clone());
        assert!(
            fixture
                .state
                .storage
                .native_chunk_maps
                .set(repository)
                .is_ok()
        );
        let (entered, release, leader) = held_leader(&fixture);
        timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        let flight = observer(&fixture).await;
        let app = fixture.app.clone();
        let request = fixture.request("GET", "chunk-map?path=/alias", Body::empty());
        let waiter = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
        wait_owners(&flight, 3).await;
        assert!(budget.used() > 4 * 1024 * 1024);
        *fixture.counts.receipt_write_holds.lock().unwrap() = None;
        match mode {
            0 => {
                fixture
                    .counts
                    .receipt_write_fail_after_create
                    .store(true, Ordering::SeqCst);
                release.notify_one();
                error(leader.await.unwrap(), 503, "TEMPORARY_UNAVAILABLE", true).await;
                success_json(waiter.await.unwrap()).await;
            }
            1 => {
                leader.abort();
                assert!(leader.await.err().unwrap().is_cancelled());
                success_json(waiter.await.unwrap()).await;
            }
            _ => {
                waiter.abort();
                assert!(waiter.await.err().unwrap().is_cancelled());
                wait_owners(&flight, 2).await;
                release.notify_one();
                success_json(leader.await.unwrap()).await;
            }
        }
        wait_owners(&flight, 1).await;
        drop(flight);
        assert_eq!(budget.used(), 0);
        let passes = if mode == 2 { 1 } else { 2 };
        fixture.counts.assert(passes, passes * fixture.raw.len());
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), passes);
        fixture.counts.reset();
        fixture.map("/file").await;
        fixture.counts.assert(0, 0);
        assert_eq!(budget.used(), 0);
    }
}

#[tokio::test]
async fn actual_http_long_escaped_legal_path_has_bounded_owned_json_and_empty_files_have_no_map() {
    let component = "\u{1}".repeat(255);
    let mut components = vec![component; 15];
    components.push("\u{1}".repeat(239));
    let name = components.join("/");
    let path = format!("/{name}");
    assert_eq!(path.len(), 4080);
    crate::ceres::snapshot::view::validate_scope_relative_path(&path).unwrap();
    let fixture = Fixture::new_with_pg_config_directories_and_objects(
        false,
        0,
        &[(name, b"escaped path body".to_vec())],
    )
    .await;
    let encoded = url::form_urlencoded::byte_serialize(path.as_bytes()).collect::<String>();
    for whole in [1, 0] {
        fixture.counts.reset();
        let response = fixture
            .send("GET", &format!("chunk-map?path={encoded}"), Body::empty())
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["cache-control"],
            "private, no-cache, no-transform"
        );
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(bytes.len() > 4 * 1024);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["path"], path);
        assert_eq!(value["map"]["file_size"], "17");
        fixture
            .counts
            .assert(whole, if whole == 0 { 0 } else { 17 });
    }
    fixture.counts.reset();
    for suffix in ["chunk-map?path=/empty", "chunk-map/pages?path=/empty"] {
        error(
            fixture.send("GET", suffix, Body::empty()).await,
            400,
            "SCOPE_INVALID",
            false,
        )
        .await;
    }
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn actual_http_and_repository_initialization_ignore_poisoned_temp_fact_scope_and_map_shadows()
{
    let fixture = Fixture::new_with_pg_config(true).await;
    let expected = fixture.map("/file").await;
    let state = reconstructed(&fixture).await;
    assert!(state.storage.native_chunk_maps.get().is_none());
    let mono = state.storage.mono_storage();
    let db = mono.get_connection();
    let schema = fixture
        ._schema
        .as_ref()
        .unwrap()
        .schema()
        .replace('"', "\"\"");
    let tables = [
        "mst2_verified_object",
        "mst2_metadata_storage_scope",
        "mst2_chunk_map",
        "mst2_chunk_map_source",
        "mst2_chunk_map_leaf",
        "mst2_chunk_map_node",
    ];
    for table in tables {
        db.execute_unprepared(&format!("CREATE TEMP TABLE {table}(LIKE \"{schema}\".{table}); INSERT INTO pg_temp.{table} SELECT * FROM \"{schema}\".{table}")).await.unwrap();
    }
    db.execute_unprepared("UPDATE pg_temp.mst2_verified_object SET raw_sha256=decode(repeat('09',32),'hex'); UPDATE pg_temp.mst2_metadata_storage_scope SET storage_uuid='temp-poison'; UPDATE pg_temp.mst2_chunk_map SET descriptor=decode('00','hex'); UPDATE pg_temp.mst2_chunk_map_leaf SET payload=decode('00','hex'); UPDATE pg_temp.mst2_chunk_map_node SET digest=decode(repeat('09',32),'hex')").await.unwrap();
    fixture.counts.reset();
    let app = router(&state);
    let map = success_json(
        app.clone()
            .oneshot(fixture.request("GET", "chunk-map?path=/file", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(map, expected);
    let map_id = map["map"]["map_id"].as_str().unwrap();
    let page = success_json(
        app.clone()
            .oneshot(fixture.request(
                "GET",
                &format!("chunk-map/pages?path=/file&map_id={map_id}&page_index=0"),
                Body::empty(),
            ))
            .await
            .unwrap(),
    )
    .await;
    let oracle = ChunkProjection::build(fixture.digest, fixture.raw.clone()).unwrap();
    assert_eq!(
        STANDARD
            .decode(page["leaf_base64"].as_str().unwrap())
            .unwrap(),
        oracle.leaf_and_proof(0).unwrap().0.encode().unwrap()
    );
    let body = fixture.chunk_body("/file", map_id, "1").to_string();
    let response = app
        .clone()
        .oneshot(fixture.request("POST", "chunks", Body::from(body)))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let wire = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let frames = parse_stream(&wire).unwrap();
    let [Frame::Chunk(chunk), Frame::End(end)] = frames.as_slice() else {
        panic!("expected CHUNK and END");
    };
    assert_eq!(chunk.chunk_bytes, fixture.raw[CHUNK_SIZE as usize..]);
    assert_eq!(end.logical_bytes, 113);
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.counts.bytes.load(Ordering::SeqCst), 113);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    // Real primary changes still fail despite an apparently healthy shadow.
    let actual = fixture.state.storage.mono_storage();
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    let repository = state.storage.chunk_maps().await.unwrap();
    actual.get_connection().execute_unprepared("ALTER TABLE mst2_metadata_storage_scope DISABLE TRIGGER USER; UPDATE mst2_metadata_storage_scope SET storage_uuid='changed-real-primary'; ALTER TABLE mst2_metadata_storage_scope ENABLE TRIGGER USER").await.unwrap();
    assert_eq!(
        repository
            .read(&source, &state.storage.git_service.obj_storage)
            .await
            .err()
            .unwrap()
            .code,
        crate::ceres::snapshot::error::SnapshotErrorCode::IntegrityError
    );
    error(
        app.oneshot(fixture.request("GET", "chunk-map?path=/file", Body::empty()))
            .await
            .unwrap(),
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
}

pub(super) async fn assert_three_page_proofs_and_selected_sibling_faults(
    fixture: &Fixture,
    map_id: &str,
    digest: [u8; 32],
    pattern: &[u8],
) {
    use mst2_codec::chunkmap::{ChunkMap, ProofSide, leaf_proof, merkle_root};
    let full: [u8; 32] = Sha256::digest(pattern).into();
    let final_chunk: [u8; 32] = Sha256::digest(&pattern[..7]).into();
    let leaves = [
        ChunkLeaf {
            page_index: 0,
            chunk_sha256: vec![full; 256],
        },
        ChunkLeaf {
            page_index: 1,
            chunk_sha256: vec![full; 256],
        },
        ChunkLeaf {
            page_index: 2,
            chunk_sha256: vec![final_chunk],
        },
    ];
    let hashes: Vec<_> = leaves
        .iter()
        .map(|leaf| leaf.leaf_hash().unwrap())
        .collect();
    let root = merkle_root(&hashes).unwrap();
    let oracle = ChunkMap::new(digest, 512 * CHUNK_SIZE as u64 + 7, root).unwrap();
    assert_eq!(map_id, format!("sha256:{}", hex_of(&oracle.map_id())));
    for index in 0..3 {
        let page = success_json(
            fixture
                .send(
                    "GET",
                    &format!("chunk-map/pages?path=/file&map_id={map_id}&page_index={index}"),
                    Body::empty(),
                )
                .await,
        )
        .await;
        assert_eq!(
            STANDARD
                .decode(page["leaf_base64"].as_str().unwrap())
                .unwrap(),
            leaves[index as usize].encode().unwrap()
        );
        let proof = leaf_proof(&hashes, index).unwrap();
        let expected: Vec<_> = proof.iter().map(|step| json!({
            "side": if step.side == ProofSide::Left { "left" } else { "right" },
            "sibling_pages": step.sibling_pages.to_string(), "digest": format!("sha256:{}", hex_of(&step.digest)),
        })).collect();
        assert_eq!(page["proof"], json!(expected));
        verify_leaf(
            3,
            index,
            leaves[index as usize].leaf_hash().unwrap(),
            &proof,
            root,
        )
        .unwrap();
    }
    fixture.counts.assert(0, 0);
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let id = oracle.map_id().to_vec();
    for missing in [false, true] {
        db.execute_unprepared("ALTER TABLE mst2_chunk_map_node DISABLE TRIGGER USER")
            .await
            .unwrap();
        if missing {
            db.execute_raw(statement(
                "DELETE FROM mst2_chunk_map_node WHERE map_id=$1 AND first_page=2 AND page_count=1",
                [id.clone().into()],
            ))
            .await
            .unwrap();
        } else {
            db.execute_raw(statement("UPDATE mst2_chunk_map_node SET digest=$2 WHERE map_id=$1 AND first_page=2 AND page_count=1", [id.clone().into(), vec![9u8; 32].into()])).await.unwrap();
        }
        db.execute_unprepared("ALTER TABLE mst2_chunk_map_node ENABLE TRIGGER USER")
            .await
            .unwrap();
        for (method, suffix, body) in [
            (
                "GET",
                format!("chunk-map/pages?path=/file&map_id={map_id}&page_index=0"),
                Body::empty(),
            ),
            (
                "POST",
                "chunks".to_string(),
                Body::from(fixture.chunk_body("/file", map_id, "0").to_string()),
            ),
        ] {
            error(
                fixture.send(method, &suffix, body).await,
                502,
                "INTEGRITY_ERROR",
                false,
            )
            .await;
        }
        fixture.counts.assert(0, 0);
        // Page 2 proves itself through [0,2); the unrelated damaged node is
        // absent from its selected SQL and does not turn into a full-map scan.
        let page = success_json(
            fixture
                .send(
                    "GET",
                    &format!("chunk-map/pages?path=/file&map_id={map_id}&page_index=2"),
                    Body::empty(),
                )
                .await,
        )
        .await;
        assert_eq!(
            STANDARD
                .decode(page["leaf_base64"].as_str().unwrap())
                .unwrap(),
            leaves[2].encode().unwrap()
        );
        db.execute_unprepared("ALTER TABLE mst2_chunk_map_node DISABLE TRIGGER USER")
            .await
            .unwrap();
        if missing {
            db.execute_raw(statement("INSERT INTO mst2_chunk_map_node(map_id,first_page,page_count,digest) VALUES($1,2,1,$2)", [id.clone().into(), hashes[2].to_vec().into()])).await.unwrap();
        } else {
            db.execute_raw(statement("UPDATE mst2_chunk_map_node SET digest=$2 WHERE map_id=$1 AND first_page=2 AND page_count=1", [id.clone().into(), hashes[2].to_vec().into()])).await.unwrap();
        }
        db.execute_unprepared("ALTER TABLE mst2_chunk_map_node ENABLE TRIGGER USER")
            .await
            .unwrap();
    }
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn failed_digest_failed_body_and_cancelled_cold_producer_allow_the_joined_current_source_to_retry()
 {
    use super::bounded_objects::{FaultKind, StreamFault};
    for mode in 0..3 {
        let fixture = Fixture::new().await;
        let budget = MemoryBudget::new(8 * 1024 * 1024);
        let repository = PostgresChunkMapRepository::new(
            fixture
                .state
                .storage
                .mono_storage()
                .get_connection()
                .clone(),
        )
        .await
        .unwrap()
        .with_test_budget(budget.clone());
        assert!(
            fixture
                .state
                .storage
                .native_chunk_maps
                .set(repository)
                .is_ok()
        );
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let kind = if mode == 1 {
            FaultKind::HeldThenError {
                entered: entered.clone(),
                release: release.clone(),
                drops: drops.clone(),
            }
        } else {
            let mut raw = fixture.raw.clone();
            if mode == 0 {
                raw[0] ^= 1;
            }
            FaultKind::Held {
                raw: Bytes::from(raw),
                entered: entered.clone(),
                release: release.clone(),
                drops: drops.clone(),
            }
        };
        *fixture.counts.object_fault.lock().unwrap() = Some(StreamFault {
            oid: fixture.oid.clone(),
            kind,
        });
        let app = fixture.app.clone();
        let request = fixture.request("GET", "chunk-map?path=/file", Body::empty());
        let leader = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
        timeout(Duration::from_secs(10), entered.notified())
            .await
            .unwrap();
        let flight = observer(&fixture).await;
        let app = fixture.app.clone();
        let request = fixture.request("GET", "chunk-map?path=/alias", Body::empty());
        let waiter = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
        wait_owners(&flight, 3).await;
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
        assert!(budget.used() > 4 * 1024 * 1024);
        *fixture.counts.object_fault.lock().unwrap() = None;
        if mode == 2 {
            leader.abort();
            assert!(leader.await.err().unwrap().is_cancelled());
        } else {
            release.notify_one();
            error(
                leader.await.unwrap(),
                if mode == 0 { 502 } else { 503 },
                if mode == 0 {
                    "INTEGRITY_ERROR"
                } else {
                    "OBJECT_UNAVAILABLE"
                },
                false,
            )
            .await;
        }
        let map = success_json(waiter.await.unwrap()).await;
        assert_eq!(
            map["map"]["file_content_id"],
            format!("sha256:{}", hex_of(&fixture.digest))
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
        let failed_bytes = match mode {
            0 => fixture.raw.len(),
            1 => 0,
            _ => 1,
        };
        assert_eq!(
            fixture.counts.bytes.load(Ordering::SeqCst),
            fixture.raw.len() + failed_bytes
        );
        wait_owners(&flight, 1).await;
        drop(flight);
        assert_eq!(budget.used(), 0);
        fixture.counts.reset();
        fixture.map("/file").await;
        fixture.counts.assert(0, 0);
    }
}

#[tokio::test]
async fn different_current_sources_enter_cold_installations_independently() {
    let fixture = Fixture::new_with_pg_config_directories_and_objects(
        false,
        0,
        &[("other".into(), b"independent source".to_vec())],
    )
    .await;
    let (entered, release, leader) = held_leader(&fixture);
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    let app = fixture.app.clone();
    let request = fixture.request("GET", "chunk-map?path=/other", Body::empty());
    let other = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
    timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 2);
    release.notify_waiters();
    let map = success_json(leader.await.unwrap()).await;
    let second = success_json(other.await.unwrap()).await;
    assert_ne!(map["map"]["map_id"], second["map"]["map_id"]);
    fixture.counts.assert(2, fixture.raw.len() + 18);
}

#[tokio::test]
async fn persisted_descriptors_and_authenticated_pages_charge_until_the_last_live_reader_drops() {
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    fixture.counts.reset();
    let budget = MemoryBudget::new(96 * 1024);
    let repository = PostgresChunkMapRepository::new(
        fixture
            .state
            .storage
            .mono_storage()
            .get_connection()
            .clone(),
    )
    .await
    .unwrap()
    .with_test_budget(budget.clone());
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    let map = repository
        .read(&source, &fixture.state.storage.git_service.obj_storage)
        .await
        .unwrap()
        .unwrap();
    let page = repository.selected_page(&map, 0).await.unwrap();
    let other_map = map.clone();
    let other_page = page.clone();
    drop(map);
    drop(page);
    assert_eq!(budget.used(), 96 * 1024);
    assert_eq!(
        budget.reserve(1).err().unwrap().code,
        SnapshotErrorCode::TemporaryUnavailable
    );
    other_page
        .verify_chunk(&other_map.map, 1, &fixture.raw[CHUNK_SIZE as usize..])
        .unwrap();
    drop(other_map);
    assert_eq!(budget.used(), 64 * 1024);
    drop(other_page);
    assert_eq!(budget.used(), 0);
    let replay = repository
        .read(&source, &fixture.state.storage.git_service.obj_storage)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(budget.used(), 32 * 1024);
    drop(replay);
    assert_eq!(budget.used(), 0);
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn actual_install_sql_failure_rolls_back_every_index_and_exact_retry_earns_admission_again() {
    let fixture = Fixture::new().await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let budget = MemoryBudget::new(8 * 1024 * 1024);
    let repository = PostgresChunkMapRepository::new(db.clone())
        .await
        .unwrap()
        .with_test_budget(budget.clone());
    assert!(
        fixture
            .state
            .storage
            .native_chunk_maps
            .set(repository)
            .is_ok()
    );
    db.execute_unprepared("CREATE FUNCTION chunk_map_install_test_failure() RETURNS trigger LANGUAGE plpgsql AS $test$ BEGIN RAISE EXCEPTION 'injected source-row failure after complete index insertion'; END $test$; CREATE TRIGGER chunk_map_install_test_failure BEFORE INSERT ON mst2_chunk_map_source FOR EACH ROW EXECUTE FUNCTION chunk_map_install_test_failure()").await.unwrap();
    error(
        fixture
            .send("GET", "chunk-map?path=/file", Body::empty())
            .await,
        503,
        "TEMPORARY_UNAVAILABLE",
        true,
    )
    .await;
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
    assert_eq!(budget.used(), 0);
    for table in [
        "mst2_chunk_map",
        "mst2_chunk_map_leaf",
        "mst2_chunk_map_node",
        "mst2_chunk_map_source",
    ] {
        assert_eq!(count(db, table).await, 0);
    }
    db.execute_unprepared("DROP TRIGGER chunk_map_install_test_failure ON mst2_chunk_map_source; DROP FUNCTION chunk_map_install_test_failure()").await.unwrap();
    fixture.counts.reset();
    let map = fixture.map("/file").await;
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
    assert_eq!(budget.used(), 0);
    for table in [
        "mst2_chunk_map",
        "mst2_chunk_map_leaf",
        "mst2_chunk_map_node",
        "mst2_chunk_map_source",
    ] {
        assert_eq!(count(db, table).await, 1);
    }
    fixture.counts.reset();
    assert_eq!(fixture.map("/alias").await["map"], map["map"]);
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ordinary_incomplete_source_dml_cannot_commit_an_admitted_partial_map() {
    let fixture = Fixture::new().await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    let repository = PostgresChunkMapRepository::new(db.clone()).await.unwrap();
    let map = ChunkProjection::build(fixture.digest, fixture.raw.clone()).unwrap();
    let scope = repository.test_primary_scope();
    let txn = db
        .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
        .await
        .unwrap();
    txn.execute_raw(statement(
        "INSERT INTO mst2_chunk_map(map_id,descriptor,page_count,pages_root) VALUES($1,$2,$3,$4)",
        [
            map.map_id.to_vec().into(),
            map.map.encode().into(),
            (map.map.page_count as i32).into(),
            map.map.pages_root.to_vec().into(),
        ],
    ))
    .await
    .unwrap();
    txn.execute_raw(statement("INSERT INTO mst2_chunk_map_source(storage_domain,git_oid,object_kind,fact_id,source_id,source_bytes,primary_scope,map_id,receipt_digest) VALUES('git',$1,'blob',$2,$3,$4,$5,$6,$7)", [fixture.oid.clone().into(), source.fact().id.into(), repository.source_identity(&source).unwrap().to_vec().into(), source.canonical_bytes().unwrap().into(), scope.to_vec().into(), map.map_id.to_vec().into(), vec![9u8;32].into()])).await.unwrap();
    assert!(txn.commit().await.is_err());
    for table in [
        "mst2_chunk_map",
        "mst2_chunk_map_leaf",
        "mst2_chunk_map_node",
        "mst2_chunk_map_source",
    ] {
        assert_eq!(count(db, table).await, 0);
    }
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn actual_map_and_page_json_revalidate_lease_before_the_first_transport_poll() {
    for page in [false, true] {
        let fixture = Fixture::new().await;
        let map = fixture.map("/file").await;
        let suffix = if page {
            format!(
                "chunk-map/pages?path=/file&map_id={}&page_index=0",
                map["map"]["map_id"].as_str().unwrap()
            )
        } else {
            "chunk-map?path=/file".into()
        };
        fixture.counts.reset();
        let response = fixture.send("GET", &suffix, Body::empty()).await;
        assert_eq!(response.status(), 200);
        let revoked = fixture
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
        assert_eq!(revoked.status(), 200);
        let mut stream = response.into_body().into_data_stream();
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
        fixture.counts.assert(0, 0);
    }
}

#[tokio::test]
async fn oversized_current_fact_digest_fails_before_source_or_receipt_io_and_recovers_canonical_bytes()
 {
    let fixture = Fixture::new().await;
    let expected = fixture.map("/file").await;
    let original = fixture.fact().await;
    let mono = fixture.state.storage.mono_storage();
    mono.get_connection().execute_raw(statement("UPDATE mst2_verified_object SET raw_sha256=decode(repeat('ab',1048576),'hex') WHERE id=$1", [original.id.into()])).await.unwrap();
    fixture.counts.reset();
    error(
        fixture
            .send("GET", "chunk-map?path=/file", Body::empty())
            .await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 0);
    fixture.replace_fact(original).await;
    assert_eq!(fixture.map("/file").await, expected);
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn warm_admission_requires_receipt_exact_bytes_size_and_final_eof_without_fallback() {
    let fixture = Fixture::new().await;
    let map = fixture.map("/file").await;
    let repository = fixture.state.storage.chunk_maps().await.unwrap();
    let source = ChunkMapSource::from_fact(fixture.fact().await, &fixture.oid).unwrap();
    let key = ObjectKey {
        namespace: ObjectNamespace::ChunkMapReceipt,
        key: hex_of(&repository.source_identity(&source).unwrap()),
    };
    let (mut stream, meta) = fixture
        .state
        .storage
        .git_service
        .obj_storage
        .inner
        .get_stream(&key)
        .await
        .unwrap();
    let mut original = Vec::new();
    while let Some(part) = stream.next().await {
        original.extend_from_slice(&part.unwrap());
    }
    assert_eq!(original.len() as i64, meta.size);
    let mut wrong = original.clone();
    wrong[0] ^= 1;
    let mut long = original.clone();
    long.push(0);
    let cases = [
        original[..original.len() - 1].to_vec(),
        long,
        wrong,
        original.clone(),
    ];
    fixture
        .counts
        .receipt_read_meta_size
        .store(meta.size, Ordering::SeqCst);
    for (index, bytes) in cases.into_iter().enumerate() {
        fixture.counts.reset();
        *fixture.counts.receipt_read_corruption.lock().unwrap() = Some(Bytes::from(bytes));
        fixture
            .counts
            .receipt_read_late_error
            .store(index == 3, Ordering::SeqCst);
        error(
            fixture
                .send("GET", "chunk-map?path=/file", Body::empty())
                .await,
            502,
            "INTEGRITY_ERROR",
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
        assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    }
    *fixture.counts.receipt_read_corruption.lock().unwrap() = None;
    fixture
        .counts
        .receipt_read_late_error
        .store(false, Ordering::SeqCst);
    assert_eq!(fixture.map("/file").await, map);
    fixture.counts.assert(0, 0);
}
