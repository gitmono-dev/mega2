//! Permanent semantic routes, with a separate existing generic session ledger.

use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let connection = manager.get_connection();
        let row = connection
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT current_schema() AS schema,n.oid::bigint AS oid
                 FROM pg_catalog.pg_namespace n WHERE n.nspname=current_schema()",
            ))
            .await?
            .ok_or_else(|| DbErr::Custom("storage route core schema is missing".into()))?;
        let schema: String = row.try_get("", "schema")?;
        let oid: i64 = row.try_get("", "oid")?;
        let quoted = format!("\"{}\"", schema.replace('"', "\"\""));
        let literal = format!("'{}'", schema.replace('\'', "''"));
        #[cfg(test)]
        let mono_key2 = format!("pg_catalog.hashtext({literal})");
        #[cfg(not(test))]
        let mono_key2 = super::super::storage::push_queue_storage::MONO_WRITE_LOCK_KEY2.to_string();
        let sql = include_str!("m20261007_000600_storage_routes.sql")
            .replace("$CORE_SCHEMA$", &quoted)
            .replace("$CORE_LITERAL$", &literal)
            .replace("$CORE_OID$", &oid.to_string())
            .replace("$MONO_KEY2$", &mono_key2)
            .replace("$NAMESPACE_UUID$", &uuid::Uuid::new_v4().to_string());
        connection.execute_unprepared(&sql).await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
