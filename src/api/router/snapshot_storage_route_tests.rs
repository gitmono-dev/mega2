use mst2_codec::{
    descriptor::ServingDescriptor,
    metapage::{Entry, EntryKind, Page, page_id},
};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, IsolationLevel, Statement,
    TransactionTrait,
};
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    ceres::snapshot::{
        pages::PreparedNativeMetadataRetention,
        retention::RetentionRoot,
        retention_dag::{MetadataDagBuilder, MetadataDagLimits},
    },
    jupiter::{
        migration::Migrator,
        storage::{
            mst2_retention::{PostgresRetentionRepository, RETENTION_LOCK_KEY},
            native_metadata_install::generations::qualified::PostgresQualifiedMetadataRepository,
            push_queue_storage::MONO_WRITE_LOCK_KEY1,
        },
    },
};

const ROUTE_LOCK_KEY: i32 = 1_296_718_001;
const ROUTE_TABLES: [&str; 4] = [
    "mst2_metadata_namespace",
    "mst2_snapshot_storage_route",
    "mst2_generic_session_storage_binding",
    "mst2_lease_storage_route",
];

fn statement(sql: &str, values: impl IntoIterator<Item = sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

async fn scalar<C: ConnectionTrait>(db: &C, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn json_sql<C: ConnectionTrait>(db: &C, sql: &str) -> Value {
    let value: String = db
        .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    serde_json::from_str(&value).unwrap()
}

async fn routes<C: ConnectionTrait>(db: &C) -> Value {
    json_sql(
        db,
        "SELECT jsonb_build_object(
          'namespace',(SELECT jsonb_agg(to_jsonb(n) ORDER BY namespace_uuid) FROM mst2_metadata_namespace n),
          'snapshot',(SELECT jsonb_agg(to_jsonb(r) ORDER BY snapshot_id) FROM mst2_snapshot_storage_route r),
          'binding',(SELECT jsonb_agg(to_jsonb(b) ORDER BY snapshot_id) FROM mst2_generic_session_storage_binding b),
          'lease',(SELECT jsonb_agg(to_jsonb(l) ORDER BY lease_id) FROM mst2_lease_storage_route l))::text",
    )
    .await
}

async fn sources<C: ConnectionTrait>(db: &C) -> Value {
    json_sql(
        db,
        "SELECT jsonb_build_object(
          'context',(SELECT jsonb_agg(to_jsonb(s) ORDER BY snapshot_id) FROM mst2_snapshot_context s),
          'lease',(SELECT jsonb_agg(to_jsonb(l) ORDER BY lease_id) FROM mst2_snapshot_lease l),
          'prepare',(SELECT jsonb_agg(to_jsonb(p) ORDER BY prepare_id) FROM mst2_metadata_prepare p),
          'member',(SELECT jsonb_agg(to_jsonb(m) ORDER BY prepare_id,page_id) FROM mst2_metadata_prepare_page m),
          'payload',(SELECT jsonb_agg(to_jsonb(p) ORDER BY page_id) FROM mst2_metadata_payload p),
          'root',(SELECT jsonb_agg(to_jsonb(r) ORDER BY root_key,node_id) FROM mst2_retention_root r))::text",
    )
    .await
}

async fn roots<C: ConnectionTrait>(db: &C) -> Value {
    json_sql(
        db,
        "SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY root_key,node_id),'[]'::jsonb)::text FROM mst2_retention_root r",
    )
    .await
}

async fn domain_boundary_digests(db: &DatabaseConnection) -> Value {
    let mut inventory = serde_json::Map::new();
    for (table, keys) in [
        ("mst2_snapshot_context", "r.snapshot_id"),
        ("mst2_snapshot_lease", "r.lease_id"),
        ("mst2_metadata_prepare", "r.prepare_id"),
        ("mst2_metadata_prepare_page", "r.prepare_id,r.page_id"),
        ("mst2_metadata_payload", "r.page_id"),
        ("mst2_metadata_lifetime", "r.page_id,r.generation"),
        ("mst2_metadata_current", "r.page_id"),
        ("mst2_metadata_graph_node", "r.page_id,r.generation"),
        (
            "mst2_metadata_graph_edge",
            "r.parent_page,r.parent_generation,r.child_page,r.child_generation",
        ),
        (
            "mst2_metadata_graph_root",
            "r.prepare_id,r.page_id,r.generation",
        ),
        ("mst2_metadata_gc_op", "r.operation_id"),
        ("mst2_retention_node", "r.node_id"),
        ("mst2_retention_edge", "r.parent_id,r.child_id"),
        ("mst2_retention_root", "r.node_id,r.root_key"),
        ("mst2_retention_gc_op", "r.operation_id"),
        ("mst2_metadata_install_seal", "r.prepare_id"),
        ("mst2_metadata_storage_scope", "r.singleton"),
        ("mst2_metadata_namespace", "r.namespace_uuid"),
        ("mst2_snapshot_storage_route", "r.snapshot_id"),
        (
            "mst2_generic_session_storage_binding",
            "r.session_incarnation",
        ),
        ("mst2_lease_storage_route", "r.lease_id"),
    ] {
        // Keep plans, binding bytes and payloads in PostgreSQL; only their
        // keyed row digests leave the database for the rollback oracle.
        inventory.insert(
            table.to_owned(),
            json_sql(
                db,
                &format!(
                    "SELECT coalesce(jsonb_agg(jsonb_build_object('key',jsonb_build_array({keys}),
               'sha256',encode(sha256(convert_to(to_jsonb(r)::text,'UTF8')),'hex'))
               ORDER BY {keys}),'[]'::jsonb)::text FROM {table} r",
                ),
            )
            .await,
        );
    }
    Value::Object(inventory)
}

fn resolve_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v2/snapshots/resolve")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"target":{"kind":"latest"},"scope":"/project"}).to_string(),
        ))
        .unwrap()
}

async fn lease_control(fixture: &Fixture, lease: &str, renew: bool) -> Response {
    fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(if renew { "POST" } else { "DELETE" })
                .uri(format!(
                    "/api/v2/snapshots/leases/{lease}{}",
                    if renew { "/renew" } else { "" }
                ))
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn rebuilt(fixture: &Fixture) -> Router {
    let state = rebuilt_state(fixture).await;
    Router::new().nest("/api/v2", routers(state.clone()).with_state(state))
}

async fn rebuilt_state(fixture: &Fixture) -> MonoApiServiceState {
    let config = fixture.state.storage.config();
    let mut database = config.database.clone();
    database.max_connection = 1;
    database.min_connection = 1;
    let connection = crate::jupiter::storage::init::postgres_connection(&database)
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

async fn assert_exact_bindings<C: ConnectionTrait>(db: &C, leases: i64) {
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_metadata_namespace").await,
        1
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_snapshot_storage_route").await,
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_generic_session_storage_binding"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_lease_storage_route").await,
        leases
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_snapshot_context s
             JOIN mst2_metadata_prepare p ON p.prepare_id=s.prepare_id
             JOIN mst2_snapshot_storage_route r ON r.snapshot_id=s.snapshot_id
             JOIN mst2_generic_session_storage_binding b ON b.snapshot_id=s.snapshot_id
             JOIN mst2_metadata_namespace n ON n.namespace_uuid=r.namespace_uuid
             WHERE r.canonical_descriptor=s.canonical_descriptor
               AND r.instance_id=s.instance_id AND r.commit_oid=s.commit_oid
               AND r.root_tree_oid=s.root_tree_oid AND r.metadata_root=s.metadata_root
               AND b.namespace_uuid=r.namespace_uuid AND b.prepare_id=s.prepare_id
               AND b.metadata_root=s.metadata_root AND p.metadata_root=s.metadata_root
               AND r.source_profile=jsonb_build_object('source_domain',p.source_domain,
                 'tagged_root_tree_oid',p.tagged_root_tree_oid,'scope',p.scope,
                 'schema_version',p.schema_version,'metadata_codec',p.metadata_codec,
                 'materialization_policy',p.materialization_policy,'fs_semantics',p.fs_semantics,
                 'access_projection',p.access_projection,'verification_revision',p.verification_revision,
                 'projection_revision',p.projection_revision)",
        )
        .await,
        1,
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_snapshot_lease l
             JOIN mst2_lease_storage_route r ON r.lease_id=l.lease_id
             JOIN mst2_generic_session_storage_binding b ON b.snapshot_id=l.snapshot_id
             WHERE r.snapshot_id=l.snapshot_id AND r.namespace_uuid=b.namespace_uuid
               AND r.session_incarnation=b.session_incarnation AND r.prepare_id=b.prepare_id
               AND r.metadata_root=b.metadata_root AND r.authorization_epoch=l.authorization_epoch
               AND r.publication_sequence=l.publication_sequence AND r.writer_epoch=l.writer_epoch
               AND r.certificate_receipt_id=l.certificate_receipt_id",
        )
        .await,
        leases,
    );
    let stored = routes(db).await;
    uuid::Uuid::parse_str(
        stored["binding"][0]["session_incarnation"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let namespace = &stored["namespace"][0];
    assert_eq!(namespace["graph_domain"], "generic-v1");
    assert_eq!(namespace["family_identity"], "v3-generic-session-1");
    assert_eq!(namespace["admission_state"], "G_ADMITTED_Q_CLOSED");
    assert_eq!(namespace["collector_state"], "CLOSED");
    assert_eq!(scalar(db,
        "SELECT count(*) FROM mst2_metadata_namespace n JOIN mst2_metadata_storage_scope s ON s.singleton=1
         JOIN pg_namespace c ON c.nspname=current_schema() JOIN pg_database d ON d.datname=current_database()
         WHERE n.core_schema=c.nspname AND n.core_schema_oid=c.oid
           AND n.metadata_schema=c.nspname AND n.metadata_schema_oid=c.oid
           AND n.database_name=d.datname AND n.database_oid=d.oid AND n.storage_uuid=s.storage_uuid
           AND n.mono_lock_key2=hashtext(current_schema())
           AND n.server_address IS NOT DISTINCT FROM inet_server_addr()::text
           AND n.server_port IS NOT DISTINCT FROM inet_server_port()",
    ).await, 1);
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM information_schema.columns
             WHERE table_schema=current_schema() AND table_name='mst2_snapshot_storage_route'
               AND column_name IN ('prepare_id','generation','root_generation','session_incarnation')",
        )
        .await,
        0,
        "the permanent SID route must not freeze a physical incarnation",
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM pg_constraint WHERE conrelid='mst2_lease_storage_route'::regclass
         AND contype='f' AND confrelid='mst2_generic_session_storage_binding'::regclass",
        )
        .await,
        0,
        "future namespace-specific incarnations must not have a universal G ledger FK"
    );
}

async fn rejected(db: &DatabaseConnection, sql: &str) {
    let txn = db.begin().await.unwrap();
    match txn.execute_unprepared(sql).await {
        Ok(_) => {
            assert!(txn.commit().await.is_err(), "mutation committed: {sql}");
        }
        Err(error) => {
            assert!(
                error.to_string().contains("storage route"),
                "wrong rejection for {sql}: {error}"
            );
            txn.rollback().await.unwrap();
        }
    }
}

#[tokio::test]
async fn mst2_generic_storage_routes_actual_resolve_warm_and_fresh_service_keep_exact_tuple() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    assert_exact_bindings(db, 1).await;
    let initial = routes(db).await;
    assert_eq!(initial["snapshot"][0]["snapshot_id"], fixture.snapshot);
    assert_eq!(initial["lease"][0]["lease_id"], fixture.lease);
    let original = success_json(fixture.send("GET", "descriptor", Body::empty()).await).await;
    let warm = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request())
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(warm["descriptor"]["snapshot_id"], fixture.snapshot);
    assert_ne!(warm["lease_id"], fixture.lease);
    assert_exact_bindings(db, 2).await;
    let after = routes(db).await;
    for key in ["namespace", "snapshot", "binding"] {
        assert_eq!(after[key], initial[key], "warm resolve changed {key}");
    }
    let cold = rebuilt(&fixture).await;
    assert_eq!(
        success_json(
            cold.clone()
                .oneshot(fixture.request("GET", "descriptor", Body::empty()))
                .await
                .unwrap()
        )
        .await,
        original,
    );
    assert_eq!(
        cold.oneshot(fixture.request("HEAD", "blob?path=/file", Body::empty()))
            .await
            .unwrap()
            .status(),
        200,
    );
    assert_eq!(routes(db).await, after);
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_generic_storage_routes_every_member_delete_and_truncate_are_immutable() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let original = routes(db).await;
    let source = sources(db).await;
    for table in ROUTE_TABLES {
        let columns = db
            .query_all_raw(statement(
                "SELECT column_name,udt_name FROM information_schema.columns
             WHERE table_schema=current_schema() AND table_name=$1 ORDER BY ordinal_position",
                [table.into()],
            ))
            .await
            .unwrap();
        assert!(!columns.is_empty(), "missing route relation: {table}");
        for column in columns {
            let name: String = column.try_get("", "column_name").unwrap();
            let kind: String = column.try_get("", "udt_name").unwrap();
            let quoted = format!("\"{}\"", name.replace('"', "\"\""));
            let mutation = match kind.as_str() {
                "uuid" => format!("'{}'::uuid", uuid::Uuid::new_v4()),
                "text" | "varchar" | "bpchar" => format!("coalesce({quoted},'')||'-mutated'"),
                "int2" | "int4" | "int8" => format!("coalesce({quoted},0)+1"),
                "oid" => format!("({quoted}::bigint+1)::oid"),
                "bytea" => format!("set_byte({quoted},0,(get_byte({quoted},0)+1)%256)"),
                "jsonb" => format!("{quoted}||jsonb_build_object('_mutation',true)"),
                "timestamptz" => format!("{quoted}+interval '1 second'"),
                "bool" => format!("NOT {quoted}"),
                other => panic!("uncovered immutable route member {table}.{name}: {other}"),
            };
            rejected(db, &format!("UPDATE {table} SET {quoted}={mutation}")).await;
            assert_eq!(routes(db).await, original, "{table}.{name}");
        }
        rejected(db, &format!("DELETE FROM {table}")).await;
        rejected(db, &format!("TRUNCATE {table} CASCADE")).await;
        assert_eq!(routes(db).await, original, "{table}");
    }
    for table in ["mst2_snapshot_context", "mst2_snapshot_lease"] {
        rejected(db, &format!("DELETE FROM {table}")).await;
        rejected(db, &format!("TRUNCATE {table} CASCADE")).await;
    }
    assert_eq!(sources(db).await, source);
    fixture.counts.assert(0, 0);
}

fn lease_insert(lease: &str) -> String {
    format!(
        "INSERT INTO mst2_snapshot_lease(lease_id,snapshot_id,authorization_epoch,
         publication_sequence,writer_epoch,certificate_receipt_id,expires_at_unix,state)
         SELECT '{lease}',snapshot_id,authorization_epoch,publication_sequence,writer_epoch,
         certificate_receipt_id,floor(extract(epoch FROM clock_timestamp()))::bigint+3600,'ACTIVE'
         FROM mst2_snapshot_context",
    )
}

#[tokio::test]
async fn mst2_generic_storage_routes_raw_lease_derives_atomically_and_half_rows_roll_back() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let original = routes(db).await;
    let source = sources(db).await;
    let lease = uuid::Uuid::new_v4().to_string();
    let txn = db.begin().await.unwrap();
    assert_eq!(
        txn.execute_unprepared(&lease_insert(&lease))
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    assert_exact_bindings(&txn, 2).await;
    txn.rollback().await.unwrap();
    assert_eq!(routes(db).await, original);
    assert_eq!(sources(db).await, source);
    super::storage_route_fixture::reject_half_lease_commit_for_test(
        db,
        &lease,
        &lease_insert(&lease),
    )
    .await;
    assert_eq!(routes(db).await, original);
    assert_eq!(sources(db).await, source);
    for table in [
        "mst2_snapshot_storage_route",
        "mst2_generic_session_storage_binding",
    ] {
        assert_eq!(routes(db).await, original);
        rejected(
            db,
            &format!(
                "INSERT INTO {table} SELECT (jsonb_populate_record(NULL::{table},
             to_jsonb(r)||jsonb_build_object('snapshot_id','sha256:{}'))).* FROM {table} r",
                hex::encode([7; 32]),
            ),
        )
        .await;
    }
    rejected(db, &format!(
        "INSERT INTO mst2_lease_storage_route
         SELECT (jsonb_populate_record(NULL::mst2_lease_storage_route,
           to_jsonb(r)||jsonb_build_object('lease_id','{lease}'))).* FROM mst2_lease_storage_route r",
    )).await;
    rejected(db,
        "INSERT INTO mst2_metadata_namespace SELECT (jsonb_populate_record(NULL::mst2_metadata_namespace,
         to_jsonb(n)||jsonb_build_object('namespace_uuid','00000000-0000-4000-8000-000000000001',
           'graph_domain','qualified-v1'))).* FROM mst2_metadata_namespace n",
    ).await;
    assert_eq!(routes(db).await, original);
    assert_eq!(sources(db).await, source);
}

#[tokio::test]
async fn mst2_generic_storage_routes_renew_terminal_and_unknown_release_preserve_route_and_roots() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let original = routes(db).await;
    let unknown = uuid::Uuid::new_v4().to_string();
    let root: Vec<u8> = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT metadata_root FROM mst2_snapshot_context",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    let txn = db.begin().await.unwrap();
    PostgresRetentionRepository::acquire_existing_roots_in_txn(
        &txn,
        &format!("page:sha256:{}", hex::encode(root)),
        &[RetentionRoot::Lease(unknown.clone())],
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let protected = roots(db).await;
    assert_eq!(
        success_json(lease_control(&fixture, &unknown, false).await).await["released"],
        false
    );
    assert_eq!(
        roots(db).await,
        protected,
        "unknown release must not remove an unrelated protection root"
    );
    assert_eq!(routes(db).await, original);
    let renewed = success_json(lease_control(&fixture, &fixture.lease, true).await).await;
    assert_eq!(renewed["lease_id"], fixture.lease);
    assert_eq!(renewed["snapshot_id"], fixture.snapshot);
    assert_eq!(routes(db).await, original);
    assert_eq!(
        success_json(lease_control(&fixture, &fixture.lease, false).await).await["released"],
        true
    );
    let terminal_roots = roots(db).await;
    assert_eq!(routes(db).await, original);
    assert_eq!(
        success_json(lease_control(&fixture, &fixture.lease, false).await).await["released"],
        false
    );
    assert_eq!(roots(db).await, terminal_roots);
    error(
        lease_control(&fixture, &fixture.lease, true).await,
        410,
        "LEASE_EXPIRED",
        false,
    )
    .await;
    error(
        fixture.send("GET", "descriptor", Body::empty()).await,
        410,
        "LEASE_EXPIRED",
        false,
    )
    .await;
    assert_eq!(
        fixture
            .send("HEAD", "blob?path=/file", Body::empty())
            .await
            .status(),
        410
    );
    assert_eq!(routes(db).await, original);
    assert_eq!(roots(db).await, terminal_roots);
    assert_exact_bindings(db, 1).await;
    fixture.counts.assert(0, 0);
}

async fn wait_for_advisory_waiter(held: &DatabaseTransaction, key: i32) -> i64 {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            held.execute_unprepared("SELECT pg_stat_clear_snapshot()").await.unwrap();
            let row = held.query_one_raw(statement(
                "SELECT l.pid::bigint AS pid FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid
                 WHERE l.locktype='advisory' AND l.classid=$1::bigint::oid AND l.objid=hashtext(current_schema())::oid
                   AND l.objsubid=2 AND NOT l.granted AND a.datname=current_database()
                   AND a.application_name=current_schema() LIMIT 1",
                [i64::from(key).into()],
            )).await.unwrap();
            if let Some(row) = row { break row.try_get("", "pid").unwrap(); }
            tokio::task::yield_now().await;
        }
    }).await.expect("raw lease statement did not wait on the expected advisory barrier")
}

async fn lock_count(held: &DatabaseTransaction, pid: i64, key: i32) -> i64 {
    held.query_one_raw(statement(
        "SELECT count(*)::bigint FROM pg_locks WHERE pid::bigint=$1 AND locktype='advisory'
         AND classid=$2::bigint::oid AND objid=hashtext(current_schema())::oid AND objsubid=2 AND granted",
        [pid.into(), i64::from(key).into()],
    )).await.unwrap().unwrap().try_get_by_index(0).unwrap()
}

#[tokio::test]
async fn mst2_generic_storage_routes_raw_statement_waits_mono_then_route_then_retention() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    for held_key in [MONO_WRITE_LOCK_KEY1, ROUTE_LOCK_KEY, RETENTION_LOCK_KEY] {
        let held = db.begin().await.unwrap();
        held.execute_raw(statement(
            "SELECT pg_advisory_xact_lock($1,hashtext(current_schema()))",
            [held_key.into()],
        ))
        .await
        .unwrap();
        let writing = {
            let db = db.clone();
            let lease = fixture.lease.clone();
            tokio::spawn(async move {
                let txn = db.begin().await.unwrap();
                let result = txn.execute_unprepared(&format!(
                    "{} ON CONFLICT(lease_id) DO UPDATE SET expires_at_unix=EXCLUDED.expires_at_unix",
                    lease_insert(&lease),
                )).await;
                txn.rollback().await.unwrap();
                result
            })
        };
        let pid = wait_for_advisory_waiter(&held, held_key).await;
        assert_eq!(
            lock_count(&held, pid, MONO_WRITE_LOCK_KEY1).await,
            if held_key == MONO_WRITE_LOCK_KEY1 {
                0
            } else {
                1
            }
        );
        assert_eq!(
            lock_count(&held, pid, ROUTE_LOCK_KEY).await,
            if held_key == RETENTION_LOCK_KEY { 1 } else { 0 }
        );
        assert_eq!(lock_count(&held, pid, RETENTION_LOCK_KEY).await, 0);
        assert!(
            held.query_one_raw(statement(
                "SELECT lease_id FROM mst2_snapshot_lease WHERE lease_id=$1 FOR UPDATE NOWAIT",
                [fixture.lease.clone().into()],
            ))
            .await
            .unwrap()
            .is_some(),
            "the blocked statement locked its existing lease before the barrier"
        );
        assert_eq!(
            held.query_one_raw(statement(
                "SELECT count(*)::bigint FROM pg_locks WHERE pid::bigint=$1 AND locktype='tuple'
             AND relation IN ('mst2_snapshot_context'::regclass,'mst2_snapshot_lease'::regclass)",
                [pid.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index::<i64>(0)
            .unwrap(),
            0
        );
        held.rollback().await.unwrap();
        assert_eq!(writing.await.unwrap().unwrap().rows_affected(), 1);
        assert_exact_bindings(db, 1).await;
    }
    for (key, message) in [
        (
            RETENTION_LOCK_KEY,
            "cannot acquire core locks after retention",
        ),
        (ROUTE_LOCK_KEY, "lock was acquired before core mono"),
    ] {
        for lock in ["pg_advisory_xact_lock", "pg_advisory_xact_lock_shared"] {
            let held = db.begin().await.unwrap();
            held.execute_raw(statement(
                &format!("SELECT {lock}($1,hashtext(current_schema()))"),
                [key.into()],
            ))
            .await
            .unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                held.execute_unprepared(&lease_insert(&uuid::Uuid::new_v4().to_string())),
            )
            .await
            .expect("reverse-order statement waited instead of rejecting");
            let error = result.unwrap_err();
            assert!(
                error.to_string().contains(message),
                "wrong reverse-order {lock} rejection: {error}"
            );
            held.rollback().await.unwrap();
        }
    }
    assert_exact_bindings(db, 1).await;
}

#[tokio::test]
async fn mst2_generic_storage_routes_schema_isolation_wrong_caller_rr_and_temp_shadow_fail_closed()
{
    let fixture = Fixture::new_with_pg_config(true).await;
    let alien = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let original = routes(db).await;
    let other = routes(alien.state.storage.mono_storage().get_connection()).await;
    assert_ne!(
        original["namespace"][0]["namespace_uuid"],
        other["namespace"][0]["namespace_uuid"]
    );
    let transplanted = original["snapshot"][0].clone();
    let alien_db = alien.state.storage.mono_storage();
    let txn = alien_db.get_connection().begin().await.unwrap();
    let transplant = txn.execute_raw(statement(
        "INSERT INTO mst2_snapshot_storage_route SELECT (jsonb_populate_record(NULL::mst2_snapshot_storage_route,
         $1::jsonb||jsonb_build_object('namespace_uuid',(SELECT namespace_uuid FROM mst2_metadata_namespace)))).*",
        [transplanted.to_string().into()],
    )).await.unwrap_err();
    assert!(transplant.to_string().contains("actual generic context"));
    txn.rollback().await.unwrap();
    let schema = fixture._schema.as_ref().unwrap().schema();
    let qualified = format!("\"{}\".mst2_route_enter", schema.replace('"', "\"\""));
    let txn = db
        .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
        .await
        .unwrap();
    assert!(
        txn.execute_unprepared(&lease_insert(&uuid::Uuid::new_v4().to_string()))
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared("SET LOCAL search_path=pg_catalog")
        .await
        .unwrap();
    assert!(
        txn.execute_raw(statement(
            &format!("SELECT {qualified}($1)"),
            ["pg_catalog".into()]
        ))
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    let txn = db.begin().await.unwrap();
    assert!(
        txn.execute_raw(statement(
            &format!("SELECT {qualified}($1)"),
            [alien._schema.as_ref().unwrap().schema().into()]
        ))
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared(
        "CREATE TEMP TABLE mst2_metadata_namespace(namespace_uuid uuid);
         CREATE TEMP TABLE mst2_snapshot_storage_route(snapshot_id text);
         CREATE TEMP TABLE mst2_generic_session_storage_binding(snapshot_id text);
         CREATE TEMP TABLE mst2_lease_storage_route(lease_id text)",
    )
    .await
    .unwrap();
    txn.execute_raw(statement(
        &format!("SELECT {qualified}($1)"),
        [schema.into()],
    ))
    .await
    .unwrap();
    assert_eq!(
        scalar(
            &txn,
            &format!(
                "SELECT count(*) FROM \"{}\".mst2_metadata_namespace",
                schema.replace('"', "\"\"")
            )
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&txn, "SELECT count(*) FROM pg_temp.mst2_metadata_namespace").await,
        0
    );
    txn.rollback().await.unwrap();
    error(
        rebuilt(&alien)
            .await
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
        410,
        "LEASE_EXPIRED",
        false,
    )
    .await;
    assert_eq!(routes(db).await, original);
    assert_eq!(
        routes(alien.state.storage.mono_storage().get_connection()).await,
        other
    );
    fixture.counts.assert(0, 0);
    alien.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_generic_storage_routes_additive_backfill_keeps_active_terminal_sources_bytes_and_roots()
 {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let warm = success_json(
        fixture
            .app
            .clone()
            .oneshot(resolve_request())
            .await
            .unwrap(),
    )
    .await;
    let second = warm["lease_id"].as_str().unwrap();
    assert_eq!(
        success_json(lease_control(&fixture, second, false).await).await["released"],
        true
    );
    assert_exact_bindings(db, 2).await;
    let source = sources(db).await;
    let original = routes(db).await;
    let descriptor = success_json(fixture.send("GET", "descriptor", Body::empty()).await).await;
    super::storage_route_fixture::restore_pre_route_schema(db).await;
    assert_eq!(sources(db).await, source);
    Migrator::up(db, None).await.unwrap();
    assert_eq!(sources(db).await, source);
    assert_exact_bindings(db, 2).await;
    let restored = routes(db).await;
    for key in [
        "canonical_descriptor",
        "instance_id",
        "commit_oid",
        "root_tree_oid",
        "metadata_root",
        "source_profile",
    ] {
        assert_eq!(
            restored["snapshot"][0][key], original["snapshot"][0][key],
            "backfill changed {key}"
        );
    }
    assert_eq!(
        success_json(
            rebuilt(&fixture)
                .await
                .oneshot(fixture.request("GET", "descriptor", Body::empty()),)
                .await
                .unwrap()
        )
        .await,
        descriptor
    );
    assert_eq!(
        success_json(lease_control(&fixture, second, false).await).await["released"],
        false
    );
    assert_eq!(sources(db).await, source);
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_generic_storage_routes_actual_http_ignores_temp_source_and_ledger_shadows() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let state = rebuilt_state(&fixture).await;
    let app = Router::new().nest("/api/v2", routers(state.clone()).with_state(state.clone()));
    let mono = state.storage.mono_storage();
    let db = mono.get_connection();
    let original = routes(db).await;
    assert_eq!(
        app.clone()
            .oneshot(fixture.request("HEAD", "blob?path=/file", Body::empty()))
            .await
            .unwrap()
            .status(),
        200
    );
    let schema = fixture
        ._schema
        .as_ref()
        .unwrap()
        .schema()
        .replace('"', "\"\"");
    db.execute_unprepared(&format!(
        "CREATE TEMP TABLE mst2_snapshot_context(LIKE \"{schema}\".mst2_snapshot_context);
         CREATE TEMP TABLE mst2_snapshot_lease(LIKE \"{schema}\".mst2_snapshot_lease);
         CREATE TEMP TABLE mst2_metadata_namespace(namespace_uuid uuid);
         CREATE TEMP TABLE mst2_snapshot_storage_route(snapshot_id text);
         CREATE TEMP TABLE mst2_generic_session_storage_binding(snapshot_id text);
         CREATE TEMP TABLE mst2_lease_storage_route(lease_id text)",
    ))
    .await
    .unwrap();
    assert_eq!(
        app.clone()
            .oneshot(fixture.request("HEAD", "blob?path=/file", Body::empty()))
            .await
            .unwrap()
            .status(),
        200
    );
    let renewed = success_json(
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v2/snapshots/leases/{}/renew", fixture.lease))
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(renewed["lease_id"], fixture.lease);
    assert_eq!(renewed["snapshot_id"], fixture.snapshot);
    for table in [
        "mst2_snapshot_context",
        "mst2_snapshot_lease",
        "mst2_metadata_namespace",
        "mst2_snapshot_storage_route",
        "mst2_generic_session_storage_binding",
        "mst2_lease_storage_route",
    ] {
        assert_eq!(
            scalar(db, &format!("SELECT count(*) FROM pg_temp.{table}")).await,
            0
        );
    }
    let real = fixture.state.storage.mono_storage();
    assert_eq!(routes(real.get_connection()).await, original);
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_generic_storage_routes_temp_prepare_shadow_rejects_qualified_context_and_registered_generic_rebind()
 {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let original_routes = routes(db).await;
    let unique = uuid::Uuid::new_v4().to_string();
    let entries = [Entry::file(
        EntryKind::Regular,
        unique.as_bytes(),
        3,
        digest(unique.as_bytes()),
    )];
    let child = Page::build(&entries).unwrap();
    let roots = [Entry::dir(b"qualified-route-boundary", page_id(&child))];
    let root = Page::build(&roots).unwrap();
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&child, &entries).unwrap();
    builder.add_directory(&root, &roots).unwrap();
    let prepared = PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page_id(&root)).unwrap()),
        "/",
    );
    let tree = "a".repeat(40);
    assert_eq!(prepared.fixed_root_tree_oid(), format!("sha1:{tree}"));
    let qualified = PostgresQualifiedMetadataRepository::new(db.clone())
        .await
        .unwrap();
    let intent = qualified
        .begin_intent(&format!("qualified-route-boundary:{unique}"), &prepared)
        .await
        .unwrap();
    qualified
        .install_pages(&intent, prepared.dag().payloads())
        .await
        .unwrap();
    let receipt = qualified.finalize(&intent).await.unwrap();
    assert_eq!(receipt.metadata_root(), prepared.dag().root());
    let committed = db.query_one_raw(statement(
        "SELECT state,graph_domain,metadata_root,tagged_root_tree_oid FROM mst2_metadata_prepare WHERE prepare_id=$1",
        [intent.prepare_id().into()],
    )).await.unwrap().unwrap();
    assert_eq!(
        committed.try_get::<String>("", "state").unwrap(),
        "COMMITTED"
    );
    assert_eq!(
        committed.try_get::<String>("", "graph_domain").unwrap(),
        "qualified-v1"
    );
    assert_eq!(
        committed.try_get::<Vec<u8>>("", "metadata_root").unwrap(),
        receipt.metadata_root().to_vec()
    );
    assert_eq!(
        committed
            .try_get::<String>("", "tagged_root_tree_oid")
            .unwrap(),
        format!("sha1:{tree}")
    );
    assert_eq!(scalar(db,
        "SELECT count(*) FROM mst2_metadata_lifetime WHERE graph_domain='qualified-v1' AND state='LIVE'",
    ).await, prepared.dag().payloads().len() as i64);
    assert_eq!(routes(db).await, original_routes);
    let before = domain_boundary_digests(db).await;
    let descriptor = ServingDescriptor {
        instance_uuid: *uuid::Uuid::parse_str(
            fixture
                .state
                .storage
                .config()
                .mst2
                .instance_uuid
                .as_deref()
                .unwrap(),
        )
        .unwrap()
        .as_bytes(),
        namespace_view_id: digest(unique.as_bytes()),
        scope: prepared.scope().to_owned(),
        metadata_root: receipt.metadata_root(),
    };
    let sid = format!("sha256:{}", hex::encode(descriptor.snapshot_id().unwrap()));
    assert_ne!(sid, fixture.snapshot);
    let schema = format!(
        "\"{}\"",
        fixture
            ._schema
            .as_ref()
            .unwrap()
            .schema()
            .replace('"', "\"\"")
    );
    let shadow =
        format!("CREATE TEMP TABLE mst2_metadata_prepare(LIKE {schema}.mst2_metadata_prepare)");
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared(&shadow).await.unwrap();
    txn.execute_unprepared(&format!("SET LOCAL search_path={schema},pg_catalog"))
        .await
        .unwrap();
    assert_eq!(
        scalar(&txn, "SELECT count(*) FROM mst2_metadata_prepare").await,
        0
    );
    assert_eq!(scalar(&txn, "SELECT CASE WHEN to_regclass('mst2_metadata_prepare')='pg_temp.mst2_metadata_prepare'::regclass THEN 1::bigint ELSE 0::bigint END").await, 1);
    let rejected = txn.execute_raw(statement(&format!(
        "INSERT INTO {schema}.mst2_snapshot_context(snapshot_id,canonical_descriptor,instance_id,commit_oid,
           root_tree_oid,metadata_root,prepare_id,publication_sequence,writer_epoch,certificate_receipt_id,authorization_epoch,state)
         SELECT $1,$2,instance_id,commit_oid,$3,$4,$5,publication_sequence,writer_epoch,certificate_receipt_id,authorization_epoch,'READY'
         FROM {schema}.mst2_snapshot_context WHERE snapshot_id=$6",
    ), [sid.into(),descriptor.encode().unwrap().into(),tree.into(),receipt.metadata_root().to_vec().into(),
        intent.prepare_id().into(),fixture.snapshot.clone().into()],
    )).await.unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("storage route is not derived from its actual generic context"),
        "{rejected}"
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        domain_boundary_digests(db).await,
        before,
        "rejected Q adoption changed source, bytes, graph roots or route inventory"
    );
    let generic = db
        .query_one_raw(statement(
            &format!(
                "SELECT p.prepare_id,p.graph_domain FROM {schema}.mst2_metadata_prepare p
         JOIN {schema}.mst2_metadata_install_seal i ON i.prepare_id=p.prepare_id
         JOIN {schema}.mst2_snapshot_context s ON s.prepare_id=p.prepare_id WHERE s.snapshot_id=$1",
            ),
            [fixture.snapshot.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let prepare_id: String = generic.try_get("", "prepare_id").unwrap();
    let domain: Option<String> = generic.try_get("", "graph_domain").unwrap();
    assert!(
        domain
            .as_deref()
            .is_none_or(|domain| domain == "generic-v1")
    );
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared(&shadow).await.unwrap();
    assert_eq!(
        scalar(&txn, "SELECT count(*) FROM mst2_metadata_prepare").await,
        0
    );
    let rejected = txn.execute_raw(statement(&format!(
        "UPDATE {schema}.mst2_metadata_prepare SET graph_domain='qualified-v1' WHERE prepare_id=$1",
    ), [prepare_id.clone().into()])).await.unwrap_err();
    let error = rejected.to_string();
    assert!(
        error.contains("registered metadata preparation identity is immutable")
            || error.contains("metadata generation seal cannot be rebound"),
        "{error}"
    );
    txn.rollback().await.unwrap();
    let after_domain: Option<String> = db.query_one_raw(statement(&format!(
        "SELECT graph_domain FROM {schema}.mst2_metadata_prepare WHERE prepare_id=$1",
    ), [prepare_id.into()])).await.unwrap().unwrap().try_get_by_index(0).unwrap();
    assert_eq!(after_domain, domain);
    assert_eq!(
        domain_boundary_digests(db).await,
        before,
        "registered G rebind changed source, bytes, graph roots or route inventory"
    );
    assert_eq!(routes(db).await, original_routes);
    assert_exact_bindings(db, 1).await;
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
async fn mst2_generic_storage_routes_corrupt_original_incarnation_rejects_reads_renew_release_without_repair()
 {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    let source = sources(db).await;
    let original = routes(db).await;
    super::storage_route_fixture::overwrite_lease_route_incarnation_for_test(db, &fixture.lease)
        .await;
    let corrupted = routes(db).await;
    assert_ne!(corrupted["lease"], original["lease"]);
    for key in ["namespace", "snapshot", "binding"] {
        assert_eq!(corrupted[key], original[key]);
    }
    error(
        fixture.send("GET", "descriptor", Body::empty()).await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    assert_eq!(
        fixture
            .send("HEAD", "blob?path=/file", Body::empty())
            .await
            .status(),
        502
    );
    error(
        lease_control(&fixture, &fixture.lease, true).await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    error(
        lease_control(&fixture, &fixture.lease, false).await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    error(
        rebuilt(&fixture)
            .await
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    assert_eq!(
        routes(db).await,
        corrupted,
        "route corruption must not be silently repaired"
    );
    assert_eq!(
        sources(db).await,
        source,
        "failed access must not change leases, plans, bytes or roots"
    );
    fixture.counts.assert(0, 0);
}
