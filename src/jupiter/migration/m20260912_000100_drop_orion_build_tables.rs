//! Forward-only drop of the retired Orion / Bellatrix build-dispatch schema.
//!
//! Historical CREATE/ALTER migrations stay in the chain so existing databases
//! keep a valid checksum path. This migration removes the leftover tables and
//! the `orion_target_status_enum` type. `down` is empty: recovery is a new
//! forward migration, not a reconstructed Orion control plane.

use sea_orm_migration::{prelude::*, sea_orm::Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Children before parents: target_state_histories → build_events /
        // build_targets; target_build_status / build_events / build_targets →
        // orion_tasks. build_triggers has no FK to orion_tasks.
        for table in [
            "target_state_histories",
            "target_build_status",
            "build_events",
            "build_targets",
            "build_triggers",
            "orion_tasks",
        ] {
            manager
                .drop_table(
                    Table::drop()
                        .table(Alias::new(table))
                        .if_exists()
                        .to_owned(),
                )
                .await?;
        }

        let db_backend = manager.get_database_backend();
        manager
            .get_connection()
            .execute_raw(Statement::from_string(
                db_backend,
                "DROP TYPE IF EXISTS orion_target_status_enum CASCADE;".to_owned(),
            ))
            .await?;

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    #[tokio::test]
    async fn drop_orion_build_tables_removes_schema_after_apply_migrations() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        for table in [
            "target_state_histories",
            "target_build_status",
            "build_events",
            "build_targets",
            "build_triggers",
            "orion_tasks",
        ] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to be dropped"
            );
        }

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT 1 AS present \
             FROM pg_type t \
             JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE t.typname = 'orion_target_status_enum' \
               AND n.nspname = current_schema();"
                .to_owned(),
        );
        let row = db
            .query_one_raw(stmt)
            .await
            .expect("query PostgreSQL type catalog");
        assert!(
            row.is_none(),
            "expected type 'orion_target_status_enum' to be dropped in the current schema"
        );
    }
}
