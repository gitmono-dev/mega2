use std::{sync::Arc, time::Duration};

use mst2_codec::metapage::{Entry, EntryKind, Page, page_id};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    ceres::snapshot::retention_dag::{MetadataDagBuilder, MetadataDagLimits},
    jupiter::{
        migration::Migrator,
        tests::{TestSchemaGuard, test_db_config},
    },
};

#[path = "qualified_metadata_canonical_tests.rs"]
mod canonical_tests;
#[path = "qualified_metadata_gc_tests.rs"]
mod gc_tests;
#[path = "qualified_metadata_source_revision_tests.rs"]
mod source_revision_tests;

fn prepared() -> PreparedNativeMetadataRetention {
    let entries = [Entry::file(EntryKind::Regular, b"file", 3, [42; 32])];
    let child = Page::build(&entries).unwrap();
    let root_entries = [
        Entry::dir(b"one", page_id(&child)),
        Entry::dir(b"two", page_id(&child)),
    ];
    let root = Page::build(&root_entries).unwrap();
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&child, &entries).unwrap();
    builder.add_directory(&root, &root_entries).unwrap();
    PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page_id(&root)).unwrap()),
        "/",
    )
}

async fn fixture() -> (
    DbConfig,
    DatabaseConnection,
    VerifiedQualifiedNamespace,
    DatabaseConnection,
    TestSchemaGuard,
) {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, guard) = test_db_config(temp.path()).await;
    config.max_connection = 2;
    config.min_connection = 1;
    // Exercise the actual production bootstrap, not a test-only DDL shortcut.
    let core = super::super::init::database_connection(&config)
        .await
        .unwrap();
    let namespace = provision_or_verify_rooted_qualified_family(&core)
        .await
        .unwrap();
    let mut q_config = config.clone();
    q_config.db_url = pool_url(&config.db_url, &namespace).unwrap();
    q_config.max_connection = 1;
    let q = postgres_connection(&q_config).await.unwrap();
    (config, core, namespace, q, guard)
}

async fn count<C: ConnectionTrait>(db: &C, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}
async fn write(
    writer: &ShadowQualifiedMetadataWriter,
    operation: &str,
    pages: &PreparedNativeMetadataRetention,
) -> GenerationMetadataReceipt {
    let intent = writer.begin_intent(operation, pages).await.unwrap();
    for batch in pages.dag().payloads().chunks(64) {
        writer.install_pages(&intent, batch).await.unwrap();
    }
    writer.finalize(&intent).await.unwrap()
}

#[tokio::test]
async fn physical_q_shadow_writer_is_distinct_and_restart_reuses_its_exact_catalog() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let pages = prepared();
    let generic=super::super::native_metadata_install::generations::PostgresMetadataGenerationRepository::new(core.clone()).await.unwrap();
    let g = generic.begin_intent("same-pages-g", &pages).await.unwrap();
    for batch in pages.dag().payloads().chunks(64) {
        generic.install_pages(&g, batch).await.unwrap();
    }
    let g_receipt = generic.finalize(&g).await.unwrap();
    let assembly_dir = tempfile::tempdir().unwrap();
    let mut app_config = crate::config::testing::isolated_config(assembly_dir.path());
    app_config.database = config.clone();
    let storage = super::super::Storage::new_with_connection(
        Arc::new(app_config),
        Arc::new(core.clone()),
        super::super::object_storage::mock_object_storage(),
    )
    .await
    .unwrap();
    let writer = storage.shadow_qualified_metadata_writer().await.unwrap();
    let clone = storage.clone();
    assert!(std::ptr::eq(
        writer,
        clone.shadow_qualified_metadata_writer().await.unwrap()
    ));
    let q_receipt = write(writer, "same-pages-q", &pages).await;
    assert_eq!(g_receipt.metadata_root(), q_receipt.metadata_root());
    assert_ne!(g.prepare_id(), q_receipt.intent().prepare_id());
    assert_eq!(
        count(&core, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation=1"
        )
        .await,
        2
    );
    assert_eq!(count(&q,"SELECT count(*) FROM mst2_metadata_prepare WHERE graph_domain='qualified-v1' AND state='COMMITTED'").await,1);
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_qualified_session_incarnation"
        )
        .await,
        0
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_qualified_lease_binding").await,
        0
    );
    assert_eq!(count(&q,"SELECT count(*) FROM pg_catalog.pg_class WHERE relnamespace=(SELECT oid FROM pg_catalog.pg_namespace WHERE nspname=current_schema()) AND relkind='r'").await,22);
    assert_eq!(count(&q,"SELECT count(*) FROM pg_catalog.pg_class WHERE relnamespace=(SELECT oid FROM pg_catalog.pg_namespace WHERE nspname=current_schema()) AND relname IN ('mst2_retention_node','mst2_retention_edge','mst2_snapshot_context','mst2_snapshot_lease','mega_refs','git_repo','seaql_migrations')").await,0);
    let q_scope = q
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT primary_scope,storage_seal FROM mst2_metadata_prepare",
        ))
        .await
        .unwrap()
        .unwrap();
    let g_scope = core
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT primary_scope,storage_seal FROM mst2_metadata_prepare WHERE prepare_id=$1",
            [g.prepare_id().into()],
        ))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        q_scope.try_get::<Vec<u8>>("", "primary_scope").unwrap(),
        g_scope.try_get::<Vec<u8>>("", "primary_scope").unwrap()
    );
    assert_ne!(
        q_scope.try_get::<Vec<u8>>("", "storage_seal").unwrap(),
        g_scope.try_get::<Vec<u8>>("", "storage_seal").unwrap()
    );
    let restarted = super::super::init::database_connection(&config)
        .await
        .unwrap();
    assert_eq!(
        provision_or_verify_rooted_qualified_family(&restarted)
            .await
            .unwrap(),
        namespace
    );
    let fresh = ShadowQualifiedMetadataWriter::open(&restarted, &config)
        .await
        .unwrap();
    assert_eq!(fresh.finalize(q_receipt.intent()).await.unwrap(), q_receipt);
    assert_eq!(
        catalog(&q, namespace.core_oid, namespace.schema_oid)
            .await
            .unwrap(),
        namespace.catalog_fingerprint
    );
}

#[tokio::test]
async fn q_catalog_trigger_function_and_index_tamper_are_never_refreshed() {
    for tamper in [
        "ALTER TABLE {q}.mst2_metadata_payload DISABLE TRIGGER mst2_metadata_payload_fenced",
        "CREATE OR REPLACE FUNCTION {q}.mst2_metadata_has_generic_overlap(p bytea) RETURNS boolean LANGUAGE sql AS 'SELECT true'",
        "DROP INDEX {q}.idx_mst2_metadata_graph_edge_child",
        "CREATE RULE forged_skip AS ON INSERT TO {q}.mst2_metadata_prepare DO INSTEAD NOTHING",
    ] {
        let (config, core, namespace, q, _guard) = fixture().await;
        let writer = ShadowQualifiedMetadataWriter::open(&core, &config)
            .await
            .unwrap();
        core.execute_unprepared(&tamper.replace("{q}", &identifier(&namespace.schema)))
            .await
            .unwrap();
        assert!(writer.begin_intent("tampered", &prepared()).await.is_err());
        let error = provision_or_verify_rooted_qualified_family(&core)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("catalog fingerprint changed"),
            "{error}"
        );
        assert_eq!(
            count(&q, "SELECT count(*) FROM mst2_metadata_prepare").await,
            0
        );
        assert_eq!(
            count(&core, "SELECT count(*) FROM mst2_metadata_namespace").await,
            2
        );
        let stored:Vec<u8>=core.query_one_raw(Statement::from_string(DbBackend::Postgres,
            "SELECT catalog_fingerprint FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'"))
            .await.unwrap().unwrap().try_get_by_index(0).unwrap();
        assert_eq!(stored, namespace.catalog_fingerprint);
    }
}

#[tokio::test]
async fn q_schema_rename_fails_closed_without_reprovisioning_or_g_fallback() {
    let (config, core, namespace, _q, _guard) = fixture().await;
    let writer = ShadowQualifiedMetadataWriter::open(&core, &config)
        .await
        .unwrap();
    let renamed = format!("{}_renamed", namespace.schema);
    core.execute_unprepared(&format!(
        "ALTER SCHEMA {} RENAME TO {}",
        identifier(&namespace.schema),
        identifier(&renamed)
    ))
    .await
    .unwrap();
    assert!(writer.begin_intent("renamed", &prepared()).await.is_err());
    assert!(
        provision_or_verify_rooted_qualified_family(&core)
            .await
            .is_err()
    );
    assert_eq!(
        count(&core, "SELECT count(*) FROM mst2_metadata_namespace").await,
        2
    );
    assert_eq!(
        count(&core, "SELECT count(*) FROM mst2_metadata_prepare").await,
        0
    );
    assert_eq!(
        count(
            &core,
            &format!(
                "SELECT count(*) FROM {}.mst2_metadata_prepare",
                identifier(&renamed)
            )
        )
        .await,
        0
    );
}

#[tokio::test]
async fn q_actual_schema_rr_temp_shadow_and_admitted_ledgers_keep_their_guards() {
    let (_config, core, namespace, q, _guard) = fixture().await;
    let bad = core
        .execute_unprepared(&format!(
            "INSERT INTO {}.mst2_metadata_gc_op SELECT * FROM {}.mst2_metadata_gc_op WHERE false",
            identifier(&namespace.schema),
            identifier(&namespace.schema)
        ))
        .await
        .unwrap_err();
    assert!(bad.to_string().contains("captured primary family"), "{bad}");
    let rr = q
        .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
        .await
        .unwrap();
    let error = rr
        .execute_unprepared(
            "INSERT INTO mst2_metadata_gc_op SELECT * FROM mst2_metadata_gc_op WHERE false",
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("READ COMMITTED"), "{error}");
    rr.rollback().await.unwrap();
    for statement in [
        "SELECT mst2_metadata_gc_apply(gen_random_uuid())",
        "SELECT mst2_metadata_gc_finish(gen_random_uuid())",
    ] {
        let error = q.execute_unprepared(statement).await.unwrap_err();
        assert!(
            error.to_string().contains("query returned no rows"),
            "{error}"
        );
    }
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_gc_op").await,
        0
    );
    for table in [
        "mst2_qualified_session_incarnation",
        "mst2_qualified_lease_binding",
    ] {
        assert!(
            q.execute_unprepared(&format!("INSERT INTO {table} DEFAULT VALUES"))
                .await
                .is_err()
        );
        assert_eq!(count(&q, &format!("SELECT count(*) FROM {table}")).await, 0);
    }
    q.execute_unprepared("CREATE TEMP TABLE mst2_metadata_prepare (prepare_id text)")
        .await
        .unwrap();
    let repository =
        PostgresQualifiedMetadataRepository::registered_shadow(q.clone(), namespace.clone())
            .await
            .unwrap();
    repository
        .begin_intent("temp-shadow", &prepared())
        .await
        .unwrap();
    assert_eq!(
        count(
            &q,
            &format!(
                "SELECT count(*) FROM {}.mst2_metadata_prepare",
                identifier(&namespace.schema)
            )
        )
        .await,
        1
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM pg_temp.mst2_metadata_prepare").await,
        0
    );
    let duplicate=core.execute_unprepared("INSERT INTO mst2_metadata_namespace SELECT * FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'").await.unwrap_err();
    assert!(
        duplicate
            .to_string()
            .contains("exact admitted rooted family scope"),
        "{duplicate}"
    );
    assert_eq!(
        count(&core, "SELECT count(*) FROM mst2_metadata_namespace").await,
        2
    );
    let pk:String=q.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT conkey::text FROM pg_catalog.pg_constraint WHERE conrelid='mst2_qualified_session_incarnation'::regclass AND contype='p'"))
        .await.unwrap().unwrap().try_get_by_index(0).unwrap();
    assert_eq!(
        pk, "{1,2}",
        "same SID can have future distinct incarnations"
    );
}

#[tokio::test]
async fn provisioning_locks_candidate_and_registered_union_in_uuid_order() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _guard) = test_db_config(temp.path()).await;
    let core = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&core, None).await.unwrap();
    let other = Database::connect(config.db_url).await.unwrap();
    let captured = captured_core(&core).await.unwrap();
    let candidate = "00000000-0000-4000-8000-000000000001";
    let schema = format!("mst2q_{}", candidate.replace('-', ""));
    let held = core.begin().await.unwrap();
    held.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_catalog.pg_advisory_xact_lock(1296717362,pg_catalog.hashtext($1))",
        [schema.clone().into()],
    ))
    .await
    .unwrap();
    let (core_name, q_name) = (captured.0.clone(), schema.clone());
    let waiter = tokio::spawn(async move {
        let txn = other.begin().await.unwrap();
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT {}.mst2_route_family_candidate_enter($1,$2::uuid,$3)",
                identifier(&core_name)
            ),
            [core_name.into(), candidate.into(), q_name.into()],
        ))
        .await
        .unwrap();
        txn.rollback().await.unwrap();
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let row=held.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_locks w WHERE w.locktype='advisory' AND NOT w.granted
             AND w.classid=1296717362::oid AND w.objid=pg_catalog.hashtext($1)::oid AND w.objsubid=2
             AND w.database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=current_database())
             AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_locks g WHERE g.pid=w.pid AND g.locktype='advisory'
               AND g.classid=1296717362::oid AND g.objid=pg_catalog.hashtext($2)::oid AND g.objsubid=2 AND g.granted)) AS sorted_wait",
            [schema.clone().into(),captured.0.clone().into()])).await.unwrap().unwrap();
        if row.try_get::<bool>("", "sorted_wait").unwrap() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "candidate must be locked before G when its UUID sorts first"
        );
        tokio::task::yield_now().await;
    }
    held.rollback().await.unwrap();
    waiter.await.unwrap();
    assert_eq!(
        count(&core, "SELECT count(*) FROM mst2_metadata_namespace").await,
        1
    );
}

#[tokio::test]
async fn q_incomplete_commit_null_binding_and_unsealed_plan_are_rejected() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let writer = ShadowQualifiedMetadataWriter::open(&core, &config)
        .await
        .unwrap();
    let intent = writer
        .begin_intent("fixed-incomplete", &prepared())
        .await
        .unwrap();
    let error=q.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE mst2_metadata_prepare SET state='COMMITTED',committed_at=clock_timestamp() WHERE prepare_id=$1",[intent.prepare_id().into()])).await.unwrap_err();
    assert!(
        error.to_string().contains("complete exact graph payload"),
        "{error}"
    );
    for mutation in [
        "graph_domain='generic-v1'",
        "canonical_plan=canonical_plan||decode('ff','hex')",
        "storage_seal=NULL",
        "primary_scope=NULL",
    ] {
        let error = q
            .execute_unprepared(&format!("UPDATE mst2_metadata_prepare SET {mutation}"))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("complete identity is immutable"),
            "{error}"
        );
    }
    for batch in prepared().dag().payloads().chunks(64) {
        writer.install_pages(&intent, batch).await.unwrap();
    }
    writer.finalize(&intent).await.unwrap();
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE state='COMMITTED'"
        )
        .await,
        1
    );
    assert_eq!(
        catalog(&q, namespace.core_oid, namespace.schema_oid)
            .await
            .unwrap(),
        namespace.catalog_fingerprint
    );
}

#[tokio::test]
async fn first_registration_cannot_self_sign_a_minimal_or_half_installed_family() {
    for complete in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (config, _guard) = test_db_config(temp.path()).await;
        let core = Database::connect(config.db_url).await.unwrap();
        Migrator::up(&core, None).await.unwrap();
        let captured = captured_core(&core).await.unwrap();
        let candidate = uuid::Uuid::new_v4().to_string();
        let schema = format!("mst2q_{}", candidate.replace('-', ""));
        let storage = uuid::Uuid::new_v4().to_string();
        let txn = core.begin().await.unwrap();
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT {}.mst2_route_family_candidate_enter($1,$2::uuid,$3)",
                identifier(&captured.0)
            ),
            [
                captured.0.clone().into(),
                candidate.clone().into(),
                schema.clone().into(),
            ],
        ))
        .await
        .unwrap();
        txn.execute_unprepared(&format!("CREATE SCHEMA {}", identifier(&schema)))
            .await
            .unwrap();
        let oid: i64 = txn
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT oid::bigint FROM pg_catalog.pg_namespace WHERE nspname=$1",
                [schema.clone().into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index(0)
            .unwrap();
        if complete {
            txn.execute_unprepared(&render_family(
                &captured.0,
                captured.1,
                &schema,
                oid,
                &candidate,
                &storage,
            ))
            .await
            .unwrap();
        } else {
            txn.execute_unprepared(&format!(
                "CREATE TABLE {q}.mst2_metadata_storage_scope(singleton integer,storage_uuid text);
                 INSERT INTO {q}.mst2_metadata_storage_scope VALUES(1,{storage});
                 CREATE TABLE {q}.mst2_metadata_family_identity(singleton integer,namespace_uuid uuid,storage_uuid text,
                   core_schema_oid oid,metadata_schema_oid oid,family_identity text,implementation_fingerprint bytea);
                 INSERT INTO {q}.mst2_metadata_family_identity VALUES(1,{candidate}::uuid,{storage},{core_oid},{oid},'{family}',decode('{implementation}','hex'))",
                q=identifier(&schema),storage=literal(&storage),candidate=literal(&candidate),core_oid=captured.1,
                family=FAMILY,implementation=hex::encode(implementation_fingerprint()))).await.unwrap();
        }
        let fingerprint = if complete {
            vec![0xff; 32]
        } else {
            catalog(&txn, captured.1, oid).await.unwrap()
        };
        txn.execute_unprepared(&format!(
            "SET LOCAL search_path={},pg_catalog,pg_temp",
            identifier(&captured.0)
        ))
        .await
        .unwrap();
        let error=txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
            "INSERT INTO {c}.mst2_metadata_namespace(singleton,namespace_uuid,core_schema,core_schema_oid,database_name,
             database_oid,storage_uuid,server_address,server_port,mono_lock_key2,metadata_schema,metadata_schema_oid,
             family_identity,graph_domain,admission_state,collector_state,metadata_storage_uuid,implementation_fingerprint,catalog_fingerprint)
             SELECT NULL,$1::uuid,g.core_schema,g.core_schema_oid,g.database_name,g.database_oid,g.storage_uuid,g.server_address,
             g.server_port,g.mono_lock_key2,$2,$3::bigint::oid,$4,'qualified-v1','ROOTED_Q_ADMITTED','ENABLED',$5,$6,$7
             FROM {c}.mst2_metadata_namespace g WHERE singleton=1",c=identifier(&captured.0)),
            [candidate.into(),schema.clone().into(),oid.into(),FAMILY.into(),storage.into(),implementation_fingerprint().into(),fingerprint.into()])).await.unwrap_err();
        let expected = if complete {
            "fingerprint disagrees"
        } else {
            "trusted complete physical family shape"
        };
        assert!(error.to_string().contains(expected), "{error}");
        txn.rollback().await.unwrap();
        assert_eq!(
            count(&core, "SELECT count(*) FROM mst2_metadata_namespace").await,
            1
        );
        assert_eq!(
            count(
                &core,
                &format!(
                    "SELECT count(*) FROM pg_catalog.pg_namespace WHERE nspname={}",
                    literal(&schema)
                )
            )
            .await,
            0
        );
        assert_eq!(
            count(&core, "SELECT count(*) FROM mst2_qualified_family_policy").await,
            1
        );
    }
}

#[tokio::test]
async fn simultaneous_production_bootstraps_reuse_one_physical_q_namespace() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _guard) = test_db_config(temp.path()).await;
    let core = postgres_connection(&config).await.unwrap();
    Migrator::up(&core, None).await.unwrap();
    let (left, right) = tokio::join!(
        super::super::init::database_connection(&config),
        super::super::init::database_connection(&config)
    );
    let left = left.unwrap();
    let right = right.unwrap();
    let a = provision_or_verify_rooted_qualified_family(&left)
        .await
        .unwrap();
    let b = provision_or_verify_rooted_qualified_family(&right)
        .await
        .unwrap();
    assert_eq!(a, b);
    assert_eq!(
        count(&core, "SELECT count(*) FROM mst2_metadata_namespace").await,
        2
    );
    assert_eq!(
        count(
            &core,
            &format!(
                "SELECT count(*) FROM pg_catalog.pg_namespace WHERE nspname={}",
                literal(&a.schema)
            )
        )
        .await,
        1
    );
}

#[tokio::test]
async fn trusted_shape_policy_and_core_validation_functions_are_immutable_or_fail_closed() {
    let (config, core, namespace, _q, _guard) = fixture().await;
    for sql in [
        "UPDATE mst2_qualified_family_policy SET expected_shape=decode(repeat('ff',32),'hex')",
        "DELETE FROM mst2_qualified_family_policy",
        "TRUNCATE mst2_qualified_family_policy",
    ] {
        let error = core.execute_unprepared(sql).await.unwrap_err();
        assert!(error.to_string().contains("immutable"), "{error}");
    }
    let writer = ShadowQualifiedMetadataWriter::open(&core, &config)
        .await
        .unwrap();
    core.execute_unprepared(&format!(
        "CREATE OR REPLACE FUNCTION {}.mst2_route_family_shape(q_oid oid,n_uuid uuid,s_uuid text)
        RETURNS bytea LANGUAGE sql AS 'SELECT decode(repeat(''ff'',32),''hex'')'",
        identifier(&namespace.core_schema)
    ))
    .await
    .unwrap();
    let error = provision_or_verify_rooted_qualified_family(&core)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("trusted core authority catalog changed"),
        "{error}"
    );
    assert!(
        writer
            .begin_intent("bad-core-validator", &prepared())
            .await
            .is_err()
    );
    assert_eq!(
        count(&core, "SELECT count(*) FROM mst2_metadata_namespace").await,
        2
    );
}

#[tokio::test]
async fn random_q_templates_have_one_trusted_shape_and_changed_composite_signature_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _guard) = test_db_config(temp.path()).await;
    let core = postgres_connection(&config).await.unwrap();
    Migrator::up(&core, None).await.unwrap();
    let captured = captured_core(&core).await.unwrap();
    let expected: Vec<u8> = core
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT expected_shape FROM mst2_qualified_family_policy WHERE singleton=1",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    let authority = catalog(&core, captured.1, 0).await.unwrap();
    let mut schemas = Vec::new();
    for _ in 0..2 {
        let namespace = uuid::Uuid::new_v4().to_string();
        let schema = format!("mst2q_{}", namespace.replace('-', ""));
        let storage = uuid::Uuid::new_v4().to_string();
        let txn = core.begin().await.unwrap();
        txn.execute_unprepared(&format!("CREATE SCHEMA {}", identifier(&schema)))
            .await
            .unwrap();
        let oid = count(
            &txn,
            &format!(
                "SELECT oid::bigint FROM pg_catalog.pg_namespace WHERE nspname={}",
                literal(&schema),
            ),
        )
        .await;
        txn.execute_unprepared(&render_family(
            &captured.0,
            captured.1,
            &schema,
            oid,
            &namespace,
            &storage,
        ))
        .await
        .unwrap();
        let shape = || {
            Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!(
                    "SELECT {}.mst2_route_family_shape($1::bigint::oid,$2::uuid,$3)",
                    identifier(&captured.0),
                ),
                [oid.into(), namespace.clone().into(), storage.clone().into()],
            )
        };
        let actual: Vec<u8> = txn
            .query_one_raw(shape())
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index(0)
            .unwrap();
        assert_eq!(
            actual, expected,
            "random template must retain its trusted structural signature"
        );
        assert_eq!(
            catalog_with_exemption(&txn, captured.1, 0, oid)
                .await
                .unwrap(),
            authority
        );
        assert_ne!(
            catalog(&txn, captured.1, 0).await.unwrap(),
            authority,
            "an unregistered candidate is never implicitly exempted"
        );
        txn.execute_unprepared(&format!(
            "DROP FUNCTION {q}.mst2_metadata_rooted_manifest({q}.mst2_metadata_prepare);
             CREATE FUNCTION {q}.mst2_metadata_rooted_manifest(q text) RETURNS jsonb
               LANGUAGE sql IMMUTABLE STRICT AS 'SELECT to_jsonb(q)'",
            q = identifier(&schema),
        ))
        .await
        .unwrap();
        let altered: Vec<u8> = txn
            .query_one_raw(shape())
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index(0)
            .unwrap();
        assert_ne!(
            altered, expected,
            "normalization must preserve the composite argument type"
        );
        txn.rollback().await.unwrap();
        schemas.push(schema);
    }
    assert_ne!(schemas[0], schemas[1]);
    assert_eq!(catalog(&core, captured.1, 0).await.unwrap(), authority);
    assert!(registered(&core, &captured).await.unwrap().is_none());
}

#[tokio::test]
async fn fresh_q_core_authority_exempts_only_exact_q_ri_and_restart_keeps_full_catalog_binding() {
    for tamper in ["fk", "ri", "external"] {
        let (config, core, namespace, _q, _guard) = fixture().await;
        let captured = (namespace.core_schema.clone(), namespace.core_oid);
        let authority: Vec<u8> = core
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT authority_catalog FROM mst2_qualified_family_policy WHERE singleton=1",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index(0)
            .unwrap();
        assert_eq!(
            catalog_with_exemption(&core, captured.1, 0, namespace.schema_oid)
                .await
                .unwrap(),
            authority
        );
        assert_ne!(catalog(&core, captured.1, 0).await.unwrap(), authority);
        for _ in 0..2 {
            assert_eq!(
                registered(&core, &captured).await.unwrap(),
                Some(namespace.clone())
            );
        }
        let restarted = postgres_connection(&config).await.unwrap();
        assert_eq!(
            provision_or_verify_rooted_qualified_family(&restarted)
                .await
                .unwrap(),
            namespace
        );
        let txn = core.begin().await.unwrap();
        if tamper == "external" {
            let external = format!("hostile_{}", uuid::Uuid::new_v4().simple());
            txn.execute_unprepared(&format!(
                "CREATE SCHEMA {external}; CREATE TABLE {external}.forged_route(snapshot_id text,namespace_uuid uuid,
                   FOREIGN KEY(snapshot_id,namespace_uuid) REFERENCES {c}.mst2_snapshot_storage_route(snapshot_id,namespace_uuid))",
                external=identifier(&external),c=identifier(&captured.0),
            )).await.unwrap();
            assert_ne!(
                catalog_with_exemption(&txn, captured.1, 0, namespace.schema_oid)
                    .await
                    .unwrap(),
                authority,
                "another namespace's internal RI triggers remain part of core authority"
            );
        } else {
            let row=txn.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
                "SELECT fk.conname,t.tgname FROM pg_catalog.pg_constraint fk
                 JOIN pg_catalog.pg_trigger t ON t.tgconstraint=fk.oid AND t.tgrelid=fk.confrelid AND t.tgisinternal
                 WHERE fk.connamespace=$1::bigint::oid AND fk.confrelid=$2::regclass AND fk.contype='f' LIMIT 1",
                [namespace.schema_oid.into(),format!("{}.mst2_snapshot_storage_route",identifier(&captured.0)).into()]))
                .await.unwrap().unwrap();
            let conname: String = row.try_get("", "conname").unwrap();
            let trigger: String = row.try_get("", "tgname").unwrap();
            let sql = if tamper == "fk" {
                format!(
                    "ALTER TABLE {}.mst2_qualified_session_incarnation DROP CONSTRAINT {}",
                    identifier(&namespace.schema),
                    identifier(&conname)
                )
            } else {
                format!(
                    "ALTER TABLE {}.mst2_snapshot_storage_route DISABLE TRIGGER {}",
                    identifier(&captured.0),
                    identifier(&trigger)
                )
            };
            txn.execute_unprepared(&sql).await.unwrap();
            assert_eq!(
                catalog_with_exemption(&txn, captured.1, 0, namespace.schema_oid)
                    .await
                    .unwrap(),
                authority
            );
            assert_ne!(
                catalog(&txn, captured.1, namespace.schema_oid)
                    .await
                    .unwrap(),
                namespace.catalog_fingerprint
            );
        }
        assert!(
            registered(&txn, &captured).await.is_err(),
            "{tamper} tampering cannot refresh admission"
        );
        txn.rollback().await.unwrap();
        assert_eq!(registered(&core, &captured).await.unwrap(), Some(namespace));
    }
}
