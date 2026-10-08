//! A bounded namespace registry; physical Q provisioning occurs at bootstrap.

use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        let row = connection.query_one_raw(Statement::from_string(DbBackend::Postgres,
            "SELECT current_schema() AS schema,n.oid::bigint AS oid FROM pg_catalog.pg_namespace n WHERE n.nspname=current_schema()"))
            .await?.ok_or_else(|| DbErr::Custom("qualified family core schema is missing".into()))?;
        let schema: String = row.try_get("", "schema")?;
        let oid: i64 = row.try_get("", "oid")?;
        let quoted = format!("\"{}\"", schema.replace('"', "\"\""));
        let literal = format!("'{}'", schema.replace('\'', "''"));
        let old = include_str!("m20261007_000600_storage_routes.sql");
        let start = old
            .find("CREATE FUNCTION mst2_route_insert_guard()")
            .ok_or_else(|| {
                DbErr::Custom("generic route insertion guard source is missing".into())
            })?;
        let end = old[start..]
            .find("CREATE FUNCTION mst2_route_context_insert()")
            .ok_or_else(|| {
                DbErr::Custom("generic route insertion guard boundary is missing".into())
            })?
            + start;
        let guard = old[start..end].replace("CREATE FUNCTION", "CREATE OR REPLACE FUNCTION")
            .replace("WHERE s.snapshot_id=NEW.snapshot_id AND NEW.canonical_descriptor=s.canonical_descriptor",
                "WHERE n.singleton=1 AND n.graph_domain='generic-v1' AND s.snapshot_id=NEW.snapshot_id AND NEW.canonical_descriptor=s.canonical_descriptor");
        let catalog = include_str!("../storage/qualified_family_catalog.sql")
            .replace("$CORE_OID$", "c_oid")
            .replace("$Q_OID$", "q_oid")
            .replace("$EXEMPT_Q_OID$", "exempt_q_oid");
        let shape = include_str!("../storage/qualified_family_shape.sql");
        let implementation = hex::encode(
            super::super::storage::qualified_metadata_family::implementation_fingerprint(),
        );
        let sql = include_str!("m20261008_000200_rooted_qualified_family.sql")
            .replace("$CATALOG_SQL$", &catalog)
            .replace("$SHAPE_SQL$", shape)
            .replace("$GENERIC_INSERT_GUARD$", &guard)
            .replace(
                "$SOURCE_REVISION_SQL$",
                include_str!("../storage/qualified_source_revision.sql"),
            )
            .replace(
                "$ROOTED_ROUTES_SQL$",
                include_str!("m20261008_000200_rooted_routes.sql"),
            )
            .replace("$CORE_SCHEMA$", &quoted)
            .replace("$CORE_LITERAL$", &literal)
            .replace("$IMPLEMENTATION_SHA$", &implementation)
            .replace("$CORE_OID$", &oid.to_string());
        connection.execute_unprepared(&sql).await?;
        // Build the expected physical shape from trusted source exactly once,
        // under this forward migration, without registering a namespace or
        // preserving any template relations/history after the transaction.
        let template_uuid = uuid::Uuid::new_v4().to_string();
        let template_schema = format!("mst2q_{}", template_uuid.replace('-', ""));
        let template_quoted = format!("\"{template_schema}\"");
        connection
            .execute_unprepared(&format!("CREATE SCHEMA {template_quoted}"))
            .await?;
        let template_oid: i64 = connection
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT oid::bigint AS oid FROM pg_catalog.pg_namespace WHERE nspname=$1",
                [template_schema.clone().into()],
            ))
            .await?
            .ok_or_else(|| DbErr::Custom("qualified family template schema is missing".into()))?
            .try_get("", "oid")?;
        let template_storage = uuid::Uuid::new_v4().to_string();
        connection
            .execute_unprepared(
                &super::super::storage::qualified_metadata_family::render_family(
                    &schema,
                    oid,
                    &template_schema,
                    template_oid,
                    &template_uuid,
                    &template_storage,
                ),
            )
            .await?;
        let expected_shape: Vec<u8> = connection.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
            format!("SELECT {quoted}.mst2_route_family_shape($1::bigint::oid,$2::uuid,$3) AS fingerprint"),
            [template_oid.into(),template_uuid.into(),template_storage.into()])).await?
            .ok_or_else(||DbErr::Custom("qualified family template shape is missing".into()))?.try_get("","fingerprint")?;
        connection.execute_unprepared(&format!("SET LOCAL search_path={quoted},pg_catalog,pg_temp; DROP SCHEMA {template_quoted} CASCADE")).await?;
        let authority_catalog: Vec<u8> = connection
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT {quoted}.mst2_route_family_catalog({oid},0::oid) AS fingerprint"),
            ))
            .await?
            .ok_or_else(|| {
                DbErr::Custom("qualified family core authority catalog is missing".into())
            })?
            .try_get("", "fingerprint")?;
        connection
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!("INSERT INTO {quoted}.mst2_qualified_family_policy VALUES(1,$1,$2,$3)"),
                [
                    hex::decode(implementation)
                        .map_err(|e| DbErr::Custom(e.to_string()))?
                        .into(),
                    expected_shape.into(),
                    authority_catalog.into(),
                ],
            ))
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
