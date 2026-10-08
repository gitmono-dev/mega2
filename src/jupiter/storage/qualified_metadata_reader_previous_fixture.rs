//! An explicit owned-schema fixture; never changes production rendering or policy.

use std::collections::BTreeSet;

use sea_orm::{DatabaseConnection, DatabaseTransaction, TransactionTrait};
use serde_json::{Value, json};

use super::*;

const PREVIOUS: &str = "6d7095dc052e60bf6de21a987b72e23fdfbfe819355f0e847d2a7f3c1cbd3f27";
const CAPTURE: &str = include_str!("../migration/m20261008_000500_previous_reader_family.sql");

fn function(source: &str, name: &str) -> (usize, usize) {
    let start = source.find(&format!("CREATE FUNCTION {name}(")).unwrap();
    let f = &source[start..];
    let body = f.find("AS $").unwrap() + 3;
    let tag_end = f[body + 1..].find('$').unwrap() + body + 2;
    let tag = &f[body..tag_end];
    let end = f[tag_end..].find(tag).unwrap() + tag_end + tag.len() + 1;
    assert_eq!(f.as_bytes()[end - 1], b';');
    (start, start + end)
}

fn render_previous(
    core: &str,
    core_oid: i64,
    q: &str,
    q_oid: i64,
    namespace: &str,
    storage: &str,
) -> String {
    crate::jupiter::migration::qualified_native_runtime_upgrade::render_prior_family(
        core, core_oid, q, q_oid, namespace, storage, PREVIOUS,
    )
    .unwrap()
}

struct Table {
    name: String,
    rows: Value,
    dependencies: BTreeSet<String>,
}

async fn fingerprint(txn: &DatabaseTransaction, core_oid: i64, q_oid: i64, exempt: i64) -> Vec<u8> {
    catalog_with_exemption(txn, core_oid, q_oid, exempt)
        .await
        .unwrap()
}

/// Recreate Q relations from captured old DDL within an owned test schema.
/// Preserve its namespace OID, scope and real source/certificate/session rows.
pub(crate) async fn restore_previous(core: &DatabaseConnection, q_schema: &str, keep_q: bool) {
    let txn = core.begin().await.unwrap();
    let (core_schema, core_oid) = captured_core(&txn).await.unwrap();
    assert!(core_schema.starts_with("mega2_test_"));
    assert!(q_schema.starts_with("mst2q_"));
    let c = identifier(&core_schema);
    let q = identifier(q_schema);
    txn.execute_unprepared(&format!(
        "SELECT {c}.mst2_route_enter({})",
        literal(&core_schema)
    ))
    .await
    .unwrap();
    let registry=txn.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        format!("SELECT metadata_schema_oid::bigint,namespace_uuid::text,metadata_storage_uuid FROM {c}.mst2_metadata_namespace WHERE metadata_schema=$1 AND graph_domain='qualified-v1'"),
        [q_schema.into()])).await.unwrap().unwrap();
    let q_oid: i64 = registry.try_get("", "metadata_schema_oid").unwrap();
    let namespace: String = registry.try_get("", "namespace_uuid").unwrap();
    let storage: String = registry.try_get("", "metadata_storage_uuid").unwrap();
    let table_rows=txn.query_all_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT relname FROM pg_catalog.pg_class WHERE relnamespace=$1::bigint::oid AND relkind='r' ORDER BY relname",[q_oid.into()])).await.unwrap();
    let mut tables = Vec::new();
    let mut all_names = Vec::new();
    for row in table_rows {
        let name: String = row.try_get("", "relname").unwrap();
        all_names.push(name.clone());
        if [
            "mst2_metadata_storage_scope",
            "mst2_metadata_family_identity",
            "mst2_metadata_reader_issuance",
        ]
        .contains(&name.as_str())
        {
            continue;
        }
        let rows:Value=txn.query_one_raw(Statement::from_string(DbBackend::Postgres,format!(
            "SELECT coalesce(jsonb_agg(to_jsonb(data)),'[]'::jsonb) AS rows FROM {q}.{} data",identifier(&name))))
            .await.unwrap().unwrap().try_get("","rows").unwrap();
        if !keep_q {
            assert_eq!(
                rows,
                json!([]),
                "Q-less fixture must have no history to discard"
            );
        }
        let dependencies=txn.query_all_raw(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT target.relname FROM pg_catalog.pg_constraint fk JOIN pg_catalog.pg_class source ON source.oid=fk.conrelid
             JOIN pg_catalog.pg_class target ON target.oid=fk.confrelid WHERE source.relnamespace=$1::bigint::oid
               AND target.relnamespace=source.relnamespace AND source.relname=$2 AND target.relname<>source.relname AND fk.contype='f'",
            [q_oid.into(),name.clone().into()])).await.unwrap().into_iter()
            .map(|row|row.try_get("","relname").unwrap()).collect();
        tables.push(Table {
            name,
            rows,
            dependencies,
        });
    }
    txn.execute_unprepared(&format!(
        "DROP TABLE {} CASCADE",
        all_names
            .iter()
            .map(|name| format!("{q}.{}", identifier(name)))
            .collect::<Vec<_>>()
            .join(",")
    ))
    .await
    .unwrap();
    let funcs=txn.query_all_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT proname,pg_catalog.pg_get_function_identity_arguments(oid) AS args FROM pg_catalog.pg_proc WHERE pronamespace=$1::bigint::oid",[q_oid.into()])).await.unwrap();
    for row in funcs {
        let name: String = row.try_get("", "proname").unwrap();
        let args: String = row.try_get("", "args").unwrap();
        txn.execute_unprepared(&format!(
            "DROP FUNCTION IF EXISTS {q}.{}({args}) CASCADE",
            identifier(&name)
        ))
        .await
        .unwrap();
    }
    txn.execute_unprepared(&render_previous(
        &core_schema,
        core_oid,
        q_schema,
        q_oid,
        &namespace,
        &storage,
    ))
    .await
    .unwrap();
    let old_tables=txn.query_all_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT relname FROM pg_catalog.pg_class WHERE relnamespace=$1::bigint::oid AND relkind='r'",[q_oid.into()])).await.unwrap();
    let old_names: Vec<String> = old_tables
        .into_iter()
        .map(|row| row.try_get("", "relname").unwrap())
        .collect();
    for name in &old_names {
        txn.execute_unprepared(&format!(
            "ALTER TABLE {q}.{} DISABLE TRIGGER USER",
            identifier(name)
        ))
        .await
        .unwrap();
    }
    let mut restored = BTreeSet::from([
        "mst2_metadata_storage_scope".to_owned(),
        "mst2_metadata_family_identity".to_owned(),
    ]);
    while !tables.is_empty() {
        let index = tables
            .iter()
            .position(|table| table.dependencies.is_subset(&restored))
            .expect("captured Q foreign keys are acyclic");
        let table = tables.remove(index);
        txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
            "INSERT INTO {q}.{t} SELECT * FROM jsonb_populate_recordset(NULL::{q}.{t},$1::jsonb)",t=identifier(&table.name)),[table.rows.into()])).await.unwrap();
        restored.insert(table.name);
    }
    for name in &old_names {
        txn.execute_unprepared(&format!(
            "ALTER TABLE {q}.{} ENABLE TRIGGER USER",
            identifier(name)
        ))
        .await
        .unwrap();
    }
    txn.execute_unprepared(&format!("SET LOCAL search_path={c},pg_catalog,pg_temp"))
        .await
        .unwrap();
    let capture = CAPTURE.replace("\r\n", "\n");
    let (a, z) = function(&capture, "mst2_route_family_registration_guard");
    let registration = capture[a..z]
        .replacen("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION", 1)
        .replace("$CORE_SCHEMA$", &c)
        .replace("$IMPLEMENTATION_SHA$", PREVIOUS);
    txn.execute_unprepared(&registration).await.unwrap();
    let old_shape: Vec<u8> = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT {c}.mst2_route_family_shape($1::bigint::oid,$2::uuid,$3) AS fingerprint"
            ),
            [q_oid.into(), namespace.clone().into(), storage.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "fingerprint")
        .unwrap();
    txn.execute_unprepared(&format!("SET LOCAL search_path={c},pg_catalog,pg_temp"))
        .await
        .unwrap();
    if !keep_q {
        txn.execute_unprepared(&format!(
            "ALTER TABLE {c}.mst2_metadata_namespace DISABLE TRIGGER mst2_route_immutable"
        ))
        .await
        .unwrap();
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!("DELETE FROM {c}.mst2_metadata_namespace WHERE namespace_uuid=$1::uuid"),
            [namespace.clone().into()],
        ))
        .await
        .unwrap();
        txn.execute_unprepared(&format!("ALTER TABLE {c}.mst2_metadata_namespace ENABLE TRIGGER mst2_route_immutable; DROP SCHEMA {q} CASCADE")).await.unwrap();
    }
    let authority = fingerprint(&txn, core_oid, 0, if keep_q { q_oid } else { 0 }).await;
    let full = if keep_q {
        Some(fingerprint(&txn, core_oid, q_oid, 0).await)
    } else {
        None
    };
    txn.execute_unprepared(&format!("ALTER TABLE {c}.mst2_qualified_family_policy DISABLE TRIGGER mst2_route_family_policy_immutable")).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
        "UPDATE {c}.mst2_qualified_family_policy SET implementation_fingerprint=$1,expected_shape=$2,authority_catalog=$3 WHERE singleton=1"),
        [hex::decode(PREVIOUS).unwrap().into(),old_shape.into(),authority.clone().into()])).await.unwrap();
    txn.execute_unprepared(&format!("ALTER TABLE {c}.mst2_qualified_family_policy ENABLE TRIGGER mst2_route_family_policy_immutable")).await.unwrap();
    if let Some(full) = &full {
        txn.execute_unprepared(&format!(
            "ALTER TABLE {c}.mst2_metadata_namespace DISABLE TRIGGER mst2_route_immutable"
        ))
        .await
        .unwrap();
        txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
            "UPDATE {c}.mst2_metadata_namespace SET implementation_fingerprint=$1,catalog_fingerprint=$2 WHERE namespace_uuid=$3::uuid"),
            [hex::decode(PREVIOUS).unwrap().into(),full.clone().into(),namespace.into()])).await.unwrap();
        txn.execute_unprepared(&format!(
            "ALTER TABLE {c}.mst2_metadata_namespace ENABLE TRIGGER mst2_route_immutable"
        ))
        .await
        .unwrap();
    }
    assert_eq!(
        fingerprint(&txn, core_oid, 0, if keep_q { q_oid } else { 0 }).await,
        authority
    );
    if let Some(full) = full {
        assert_eq!(fingerprint(&txn, core_oid, q_oid, 0).await, full);
    }
    txn.commit().await.unwrap();
}

/// Restore the exact cc90 functions and stamps in an explicitly owned schema.
/// Existing reader generations, rows, relations and function OIDs are retained.
pub(crate) async fn restore_retention(
    core: &DatabaseConnection,
    q_schema: &str,
    keep_q: bool,
    legacy_shape: bool,
) {
    use crate::jupiter::migration::qualified_native_runtime_upgrade::{
        RETENTION_IMPLEMENTATION, render_prior_family,
    };

    assert!(!legacy_shape || !keep_q);
    let txn = core.begin().await.unwrap();
    let (core_schema, core_oid) = captured_core(&txn).await.unwrap();
    assert!(core_schema.starts_with("mega2_test_"));
    assert!(q_schema.starts_with("mst2q_"));
    let c = identifier(&core_schema);
    let q = identifier(q_schema);
    txn.execute_unprepared(&format!(
        "SELECT {c}.mst2_route_enter({})",
        literal(&core_schema)
    ))
    .await
    .unwrap();
    let registry = txn.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        format!("SELECT metadata_schema_oid::bigint,namespace_uuid::text,metadata_storage_uuid
            FROM {c}.mst2_metadata_namespace WHERE metadata_schema=$1 AND graph_domain='qualified-v1'"),
        [q_schema.into()])).await.unwrap().unwrap();
    let oid: i64 = registry.try_get("", "metadata_schema_oid").unwrap();
    let namespace: String = registry.try_get("", "namespace_uuid").unwrap();
    let storage: String = registry.try_get("", "metadata_storage_uuid").unwrap();
    let rendered = render_prior_family(
        &core_schema,
        core_oid,
        q_schema,
        oid,
        &namespace,
        &storage,
        RETENTION_IMPLEMENTATION,
    )
    .unwrap();
    txn.execute_unprepared(&format!("SET LOCAL search_path={q},pg_catalog,pg_temp"))
        .await
        .unwrap();
    for name in [
        "mst2_metadata_decode_rooted_plan",
        "mst2_metadata_dml_barrier",
        "mst2_metadata_gc_enabled",
    ] {
        let (a, z) = function(&rendered, name);
        txn.execute_unprepared(&rendered[a..z].replacen(
            "CREATE FUNCTION",
            "CREATE OR REPLACE FUNCTION",
            1,
        ))
        .await
        .unwrap();
    }
    txn.execute_unprepared(&format!("SET LOCAL search_path={c},pg_catalog,pg_temp"))
        .await
        .unwrap();
    let capture = CAPTURE.replace("\r\n", "\n");
    let (a, z) = function(&capture, "mst2_route_family_registration_guard");
    txn.execute_unprepared(
        &capture[a..z]
            .replacen("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION", 1)
            .replace("$CORE_SCHEMA$", &c)
            .replace("$IMPLEMENTATION_SHA$", RETENTION_IMPLEMENTATION),
    )
    .await
    .unwrap();
    let old_shape: Vec<u8> = if legacy_shape {
        txn.execute_unprepared(&format!("SET LOCAL search_path={q},pg_catalog,pg_temp"))
            .await
            .unwrap();
        txn.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            include_str!("qualified_family_shape.sql")
                .replace("q_oid", "$1::bigint::oid")
                .replace("n_uuid", "$2::uuid")
                .replace("s_uuid", "$3::text"),
            [oid.into(), namespace.clone().into(), storage.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "fingerprint")
        .unwrap()
    } else {
        txn.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!(
                "SELECT {c}.mst2_route_family_shape($1::bigint::oid,$2::uuid,$3) AS fingerprint"
            ),
            [oid.into(), namespace.clone().into(), storage.into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get("", "fingerprint")
        .unwrap()
    };
    txn.execute_unprepared(&format!("SET LOCAL search_path={c},pg_catalog,pg_temp"))
        .await
        .unwrap();
    if !keep_q {
        let tables = txn.query_all_raw(Statement::from_sql_and_values(DbBackend::Postgres,
            "SELECT relname FROM pg_catalog.pg_class WHERE relnamespace=$1::bigint::oid AND relkind='r'",
            [oid.into()])).await.unwrap();
        for row in tables {
            let name: String = row.try_get("", "relname").unwrap();
            let count: i64 = txn
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    format!("SELECT count(*) FROM {q}.{}", identifier(&name)),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get_by_index(0)
                .unwrap();
            assert_eq!(
                count,
                if [
                    "mst2_metadata_storage_scope",
                    "mst2_metadata_family_identity",
                    "mst2_metadata_reader_issuance"
                ]
                .contains(&name.as_str())
                {
                    1
                } else {
                    0
                },
                "Q-less cc90 fixture must have no history to discard: {name}"
            );
        }
        let water: i64 = txn
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT high_water FROM {q}.mst2_metadata_reader_issuance"),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index(0)
            .unwrap();
        assert_eq!(water, 0);
        txn.execute_unprepared(&format!(
            "ALTER TABLE {c}.mst2_metadata_namespace DISABLE TRIGGER mst2_route_immutable"
        ))
        .await
        .unwrap();
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!("DELETE FROM {c}.mst2_metadata_namespace WHERE namespace_uuid=$1::uuid"),
            [namespace.clone().into()],
        ))
        .await
        .unwrap();
        txn.execute_unprepared(&format!("ALTER TABLE {c}.mst2_metadata_namespace ENABLE TRIGGER mst2_route_immutable; DROP SCHEMA {q} CASCADE")).await.unwrap();
    }
    let authority = fingerprint(&txn, core_oid, 0, if keep_q { oid } else { 0 }).await;
    let full = if keep_q {
        Some(fingerprint(&txn, core_oid, oid, 0).await)
    } else {
        None
    };
    txn.execute_unprepared(&format!("ALTER TABLE {c}.mst2_qualified_family_policy DISABLE TRIGGER mst2_route_family_policy_immutable")).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        format!("UPDATE {c}.mst2_qualified_family_policy SET implementation_fingerprint=$1,expected_shape=$2,authority_catalog=$3 WHERE singleton=1"),
        [hex::decode(RETENTION_IMPLEMENTATION).unwrap().into(), old_shape.into(), authority.clone().into()])).await.unwrap();
    txn.execute_unprepared(&format!("ALTER TABLE {c}.mst2_qualified_family_policy ENABLE TRIGGER mst2_route_family_policy_immutable")).await.unwrap();
    if let Some(full) = &full {
        txn.execute_unprepared(&format!(
            "ALTER TABLE {q}.mst2_metadata_family_identity DISABLE TRIGGER mst2_00_family_barrier;
             ALTER TABLE {q}.mst2_metadata_family_identity DISABLE TRIGGER mst2_metadata_identity_immutable;
             ALTER TABLE {c}.mst2_metadata_namespace DISABLE TRIGGER mst2_00_route_statement_barrier;
             ALTER TABLE {c}.mst2_metadata_namespace DISABLE TRIGGER mst2_route_immutable")).await.unwrap();
        txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
            format!("UPDATE {q}.mst2_metadata_family_identity SET implementation_fingerprint=$1 WHERE singleton=1"),
            [hex::decode(RETENTION_IMPLEMENTATION).unwrap().into()])).await.unwrap();
        txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
            format!("UPDATE {c}.mst2_metadata_namespace SET implementation_fingerprint=$1,catalog_fingerprint=$2 WHERE namespace_uuid=$3::uuid"),
            [hex::decode(RETENTION_IMPLEMENTATION).unwrap().into(), full.clone().into(), namespace.into()])).await.unwrap();
        txn.execute_unprepared(&format!(
            "ALTER TABLE {q}.mst2_metadata_family_identity ENABLE TRIGGER mst2_00_family_barrier;
             ALTER TABLE {q}.mst2_metadata_family_identity ENABLE TRIGGER mst2_metadata_identity_immutable;
             ALTER TABLE {c}.mst2_metadata_namespace ENABLE TRIGGER mst2_00_route_statement_barrier;
             ALTER TABLE {c}.mst2_metadata_namespace ENABLE TRIGGER mst2_route_immutable")).await.unwrap();
        assert_eq!(fingerprint(&txn, core_oid, oid, 0).await, *full);
    }
    assert_eq!(
        fingerprint(&txn, core_oid, 0, if keep_q { oid } else { 0 }).await,
        authority
    );
    txn.commit().await.unwrap();
}
