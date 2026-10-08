//! Upgrade only the trusted Q reader lifecycle, preserving fixed source history.

use sea_orm::{ConnectionTrait, DbBackend, Statement, Value};
use sea_orm_migration::prelude::*;

use crate::jupiter::storage::qualified_metadata_family::{
    implementation_fingerprint, render_family,
};

const PREVIOUS_IMPLEMENTATION: &str =
    "6d7095dc052e60bf6de21a987b72e23fdfbfe819355f0e847d2a7f3c1cbd3f27";

#[derive(DeriveMigrationName)]
pub struct Migration;

fn rejected(message: &str) -> DbErr {
    DbErr::Custom(message.into())
}

fn identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn trusted_function(source: &str, name: &str) -> Result<String, DbErr> {
    let prefix = format!("CREATE FUNCTION {name}(");
    let start = source
        .find(&prefix)
        .ok_or_else(|| rejected("reader migration trusted function is missing"))?;
    let function = &source[start..];
    let body = function
        .find("AS $")
        .ok_or_else(|| rejected("reader migration function body is missing"))?
        + 3;
    let tag_end = function[body + 1..]
        .find('$')
        .ok_or_else(|| rejected("reader migration function delimiter is missing"))?
        + body
        + 2;
    let tag = &function[body..tag_end];
    let end = function[tag_end..]
        .find(tag)
        .ok_or_else(|| rejected("reader migration function terminator is missing"))?
        + tag_end
        + tag.len();
    if function.as_bytes().get(end) != Some(&b';') {
        return Err(rejected("reader migration function terminator is invalid"));
    }
    Ok(function[..=end].replacen("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION", 1))
}

async fn digest<C: ConnectionTrait>(
    connection: &C,
    sql: String,
    values: Vec<Value>,
) -> Result<Vec<u8>, DbErr> {
    connection
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            values,
        ))
        .await?
        .ok_or_else(|| rejected("reader migration physical fingerprint is missing"))?
        .try_get("", "fingerprint")
}

async fn catalog<C: ConnectionTrait>(
    connection: &C,
    core_oid: i64,
    q_oid: i64,
    exempt_oid: i64,
) -> Result<Vec<u8>, DbErr> {
    let source = include_str!("../storage/qualified_family_catalog.sql")
        .replace("$CORE_OID$", "$1::bigint::oid")
        .replace("$Q_OID$", "$2::bigint::oid")
        .replace("$EXEMPT_Q_OID$", "$3::bigint::oid");
    digest(
        connection,
        source,
        vec![core_oid.into(), q_oid.into(), exempt_oid.into()],
    )
    .await
}

async fn shape<C: ConnectionTrait>(
    connection: &C,
    q_oid: i64,
    namespace: &str,
    storage: &str,
) -> Result<Vec<u8>, DbErr> {
    let source = include_str!("../storage/qualified_family_shape.sql")
        .replace("q_oid", "$1::bigint::oid")
        .replace("n_uuid", "$2::uuid")
        .replace("s_uuid", "$3::text");
    digest(
        connection,
        source,
        vec![q_oid.into(), namespace.into(), storage.into()],
    )
    .await
}

struct Family {
    schema: String,
    oid: i64,
    namespace: String,
    storage: String,
    catalog: Vec<u8>,
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() != DbBackend::Postgres {
            return Err(rejected(
                "qualified reader retention requires primary PostgreSQL",
            ));
        }
        let connection = manager.get_connection();
        let captured = connection.query_one_raw(Statement::from_string(DbBackend::Postgres,
            "SELECT n.nspname,n.oid::bigint,registry.mono_lock_key2 FROM pg_catalog.pg_namespace n
             JOIN mst2_metadata_namespace registry ON registry.singleton=1 AND registry.core_schema=n.nspname
               AND registry.core_schema_oid=n.oid WHERE n.nspname=current_schema()"))
            .await?.ok_or_else(|| rejected("reader migration requires its captured core schema"))?;
        let core: String = captured.try_get("", "nspname")?;
        let core_oid: i64 = captured.try_get("", "oid")?;
        let mono: i32 = captured.try_get("", "mono_lock_key2")?;
        let c = identifier(&core);
        connection
            .execute_unprepared("SELECT pg_catalog.set_config('lock_timeout','5000ms',true)")
            .await?;
        connection
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT pg_catalog.pg_advisory_xact_lock(1297043024,$1)",
                [mono.into()],
            ))
            .await?;
        connection
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT pg_catalog.pg_advisory_xact_lock(1296718001,pg_catalog.hashtext($1))",
                [core.clone().into()],
            ))
            .await?;
        let namespaces = connection.query_all_raw(Statement::from_string(DbBackend::Postgres,
            format!("SELECT metadata_schema FROM {c}.mst2_metadata_namespace ORDER BY namespace_uuid"))).await?;
        if !(1..=2).contains(&namespaces.len()) {
            return Err(rejected(
                "reader migration namespace registry is not bounded",
            ));
        }
        for namespace in namespaces {
            let schema: String = namespace.try_get("", "metadata_schema")?;
            connection
                .execute_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT pg_catalog.pg_advisory_xact_lock(1296717362,pg_catalog.hashtext($1))",
                    [schema.into()],
                ))
                .await?;
        }
        connection.execute_unprepared(&format!(
            "LOCK TABLE {c}.mst2_metadata_namespace,{c}.mst2_qualified_family_policy IN ACCESS EXCLUSIVE MODE"
        )).await?;
        let policy=connection.query_one_raw(Statement::from_string(DbBackend::Postgres,
            format!("SELECT implementation_fingerprint,expected_shape,authority_catalog FROM {c}.mst2_qualified_family_policy WHERE singleton=1")))
            .await?.ok_or_else(|| rejected("reader migration trusted policy is missing"))?;
        let implementation: Vec<u8> = policy.try_get("", "implementation_fingerprint")?;
        let current = implementation_fingerprint();
        let previous =
            hex::decode(PREVIOUS_IMPLEMENTATION).map_err(|error| rejected(&error.to_string()))?;
        if implementation != previous && implementation != current {
            return Err(rejected(
                "reader migration refuses an unsupported prior implementation",
            ));
        }
        let rows=connection.query_all_raw(Statement::from_string(DbBackend::Postgres,format!(
            "SELECT namespace_uuid::text,metadata_schema,metadata_schema_oid::bigint,metadata_storage_uuid,
               implementation_fingerprint,catalog_fingerprint FROM {c}.mst2_metadata_namespace WHERE graph_domain='qualified-v1'"))).await?;
        let family = match rows.as_slice() {
            [] => None,
            [row] => Some(Family {
                schema: row.try_get("", "metadata_schema")?,
                oid: row.try_get("", "metadata_schema_oid")?,
                namespace: row.try_get("", "namespace_uuid")?,
                storage: row.try_get("", "metadata_storage_uuid")?,
                catalog: row.try_get("", "catalog_fingerprint")?,
            }),
            _ => return Err(rejected("reader migration requires at most one Q family")),
        };
        let q_oid = family.as_ref().map_or(0, |family| family.oid);
        if policy.try_get::<Vec<u8>>("", "authority_catalog")?
            != catalog(connection, core_oid, 0, q_oid).await?
        {
            return Err(rejected(
                "reader migration prior core authority catalog changed",
            ));
        }
        let scope = connection
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!("SELECT {c}.mst2_route_scope_valid($1) AS valid"),
                [core.clone().into()],
            ))
            .await?
            .ok_or_else(|| rejected("reader migration core scope is missing"))?;
        if !scope.try_get::<bool>("", "valid")? {
            return Err(rejected("reader migration prior core scope changed"));
        }
        if let Some(family) = &family {
            let q = identifier(&family.schema);
            let identity=connection.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
                "SELECT EXISTS(SELECT 1 FROM {c}.mst2_metadata_namespace n
                   JOIN {c}.mst2_metadata_namespace g ON g.singleton=1
                   JOIN {q}.mst2_metadata_family_identity i ON i.singleton=1
                   JOIN {q}.mst2_metadata_storage_scope s ON s.singleton=1
                   JOIN pg_catalog.pg_namespace physical ON physical.oid=n.metadata_schema_oid AND physical.nspname=n.metadata_schema
                   WHERE n.namespace_uuid=$1::uuid AND n.metadata_schema='mst2q_'||replace(n.namespace_uuid::text,'-','')
                     AND n.singleton IS NULL AND n.family_identity='v3-rooted-qualified-1' AND n.graph_domain='qualified-v1'
                     AND n.admission_state='ROOTED_Q_ADMITTED' AND n.collector_state='ENABLED'
                     AND ROW(n.core_schema,n.core_schema_oid,n.database_name,n.database_oid,n.storage_uuid,n.server_address,n.server_port,n.mono_lock_key2)
                       IS NOT DISTINCT FROM ROW(g.core_schema,g.core_schema_oid,g.database_name,g.database_oid,g.storage_uuid,g.server_address,g.server_port,g.mono_lock_key2)
                     AND n.metadata_storage_uuid<>g.storage_uuid AND n.implementation_fingerprint=$2
                     AND ROW(i.namespace_uuid,i.storage_uuid,i.core_schema_oid,i.metadata_schema_oid,i.family_identity,i.implementation_fingerprint)
                       IS NOT DISTINCT FROM ROW(n.namespace_uuid,n.metadata_storage_uuid,n.core_schema_oid,n.metadata_schema_oid,n.family_identity,n.implementation_fingerprint)
                     AND s.storage_uuid=i.storage_uuid) AS valid"),[family.namespace.clone().into(),implementation.clone().into()])).await?
                .ok_or_else(|| rejected("reader migration prior Q identity is missing"))?;
            if !identity.try_get::<bool>("", "valid")?
                || catalog(connection, core_oid, family.oid, 0).await? != family.catalog
                || shape(connection, family.oid, &family.namespace, &family.storage).await?
                    != policy.try_get::<Vec<u8>>("", "expected_shape")?
            {
                return Err(rejected(
                    "reader migration prior Q physical stamp or shape changed",
                ));
            }
        }
        if implementation == current {
            return Ok(());
        }

        let registration = include_str!("m20261008_000200_rooted_qualified_family.sql")
            .replace("$CORE_SCHEMA$", &c)
            .replace("$CORE_LITERAL$", &literal(&core))
            .replace("$IMPLEMENTATION_SHA$", &hex::encode(&current));
        connection
            .execute_unprepared(&trusted_function(
                &registration,
                "mst2_route_family_registration_guard",
            )?)
            .await?;

        let template_namespace = uuid::Uuid::new_v4().to_string();
        let template_schema = format!("mst2q_{}", template_namespace.replace('-', ""));
        let template_storage = uuid::Uuid::new_v4().to_string();
        let t = identifier(&template_schema);
        connection
            .execute_unprepared(&format!("CREATE SCHEMA {t}"))
            .await?;
        let template_oid: i64 = connection
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT oid::bigint FROM pg_catalog.pg_namespace WHERE nspname=$1",
                [template_schema.clone().into()],
            ))
            .await?
            .ok_or_else(|| rejected("reader migration trusted template schema is missing"))?
            .try_get("", "oid")?;
        connection
            .execute_unprepared(&render_family(
                &core,
                core_oid,
                &template_schema,
                template_oid,
                &template_namespace,
                &template_storage,
            ))
            .await?;
        let expected_shape = shape(
            connection,
            template_oid,
            &template_namespace,
            &template_storage,
        )
        .await?;
        connection
            .execute_unprepared(&format!(
                "SET LOCAL search_path={c},pg_catalog,pg_temp; DROP SCHEMA {t} CASCADE"
            ))
            .await?;

        if let Some(family) = &family {
            let q = identifier(&family.schema);
            connection.execute_unprepared(&format!("LOCK TABLE {q}.mst2_metadata_reader_operation,{q}.mst2_metadata_root_anchor IN ACCESS EXCLUSIVE MODE")).await?;
            let rendered = render_family(
                &core,
                core_oid,
                &family.schema,
                family.oid,
                &family.namespace,
                &family.storage,
            );
            let mut functions = String::new();
            for name in [
                "mst2_metadata_dml_barrier",
                "mst2_metadata_gc_enabled",
                "mst2_metadata_root_anchor_guard",
                "mst2_metadata_serving_complete",
                "mst2_metadata_next_reader_issuance",
                "mst2_metadata_reader_issuance_guard",
                "mst2_metadata_reader_guard",
                "mst2_metadata_prune_readers",
                "mst2_metadata_begin_reader",
                "mst2_metadata_finish_reader",
                "mst2_metadata_read_source_entries",
                "mst2_metadata_cleanup_expired",
                "mst2_metadata_gc_owner_cleanup",
            ] {
                functions.push_str(&trusted_function(&rendered, name)?);
                functions.push('\n');
            }
            let upgrade = include_str!("m20261008_000400_reader_retention_upgrade.sql")
                .replace("$Q_SCHEMA$", &q)
                .replace("$READER_FUNCTIONS_SQL$", &functions);
            connection.execute_unprepared(&upgrade).await?;
            if shape(connection, family.oid, &family.namespace, &family.storage).await?
                != expected_shape
            {
                return Err(rejected(
                    "reader migration upgraded Q differs from the trusted complete shape",
                ));
            }
        }

        // Fingerprints include trigger identities/enabled state, but not table
        // values. Preserve trigger OIDs while writing only these controlled stamps.
        let authority = catalog(connection, core_oid, 0, q_oid).await?;
        let full = if q_oid == 0 {
            None
        } else {
            Some(catalog(connection, core_oid, q_oid, 0).await?)
        };
        connection.execute_unprepared(&format!("ALTER TABLE {c}.mst2_qualified_family_policy DISABLE TRIGGER mst2_route_family_policy_immutable")).await?;
        connection.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
            "UPDATE {c}.mst2_qualified_family_policy SET implementation_fingerprint=$1,expected_shape=$2,authority_catalog=$3 WHERE singleton=1"),
            [current.clone().into(),expected_shape.clone().into(),authority.clone().into()])).await?;
        connection.execute_unprepared(&format!("ALTER TABLE {c}.mst2_qualified_family_policy ENABLE TRIGGER mst2_route_family_policy_immutable")).await?;
        if let (Some(family), Some(full)) = (&family, &full) {
            let q = identifier(&family.schema);
            connection.execute_unprepared(&format!(
                "ALTER TABLE {q}.mst2_metadata_family_identity DISABLE TRIGGER mst2_00_family_barrier;
                 ALTER TABLE {q}.mst2_metadata_family_identity DISABLE TRIGGER mst2_metadata_identity_immutable;
                 ALTER TABLE {c}.mst2_metadata_namespace DISABLE TRIGGER mst2_00_route_statement_barrier;
                 ALTER TABLE {c}.mst2_metadata_namespace DISABLE TRIGGER mst2_route_immutable")).await?;
            connection.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
                format!("UPDATE {q}.mst2_metadata_family_identity SET implementation_fingerprint=$1 WHERE singleton=1"),[current.clone().into()])).await?;
            connection.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,format!(
                "UPDATE {c}.mst2_metadata_namespace SET implementation_fingerprint=$1,catalog_fingerprint=$2 WHERE namespace_uuid=$3::uuid"),
                [current.clone().into(),full.clone().into(),family.namespace.clone().into()])).await?;
            connection.execute_unprepared(&format!(
                "ALTER TABLE {q}.mst2_metadata_family_identity ENABLE TRIGGER mst2_00_family_barrier;
                 ALTER TABLE {q}.mst2_metadata_family_identity ENABLE TRIGGER mst2_metadata_identity_immutable;
                 ALTER TABLE {c}.mst2_metadata_namespace ENABLE TRIGGER mst2_00_route_statement_barrier;
                 ALTER TABLE {c}.mst2_metadata_namespace ENABLE TRIGGER mst2_route_immutable")).await?;
        }
        connection
            .execute_unprepared(&format!("SET LOCAL search_path={c},pg_catalog,pg_temp"))
            .await?;
        if catalog(connection, core_oid, 0, q_oid).await? != authority {
            return Err(rejected(
                "reader migration final core authority catalog changed",
            ));
        }
        if let (Some(family), Some(full)) = (&family, &full)
            && (catalog(connection, core_oid, q_oid, 0).await? != *full
                || shape(connection, family.oid, &family.namespace, &family.storage).await?
                    != expected_shape)
        {
            return Err(rejected("reader migration final Q fingerprint changed"));
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Preserve committed issuance fences and the complete reader identity.
        Ok(())
    }

    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }
}
