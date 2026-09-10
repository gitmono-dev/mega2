//! MW-05: forward-only drop of the retired `merge_queue` table.
//!
//! CL merge already writes `push_queue` only (MW-01..MW-04). This migration
//! removes the leftover table and the Postgres enums that existed solely for it.
//! `down` is empty: recovery is a new forward migration, not a reconstructed
//! table.

use sea_orm_migration::{prelude::*, sea_orm::Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("merge_queue"))
                    .if_exists()
                    .to_owned(),
            )
            .await?;

        let db_backend = manager.get_database_backend();
        let conn = manager.get_connection();
        for sql in [
            "DROP TYPE IF EXISTS queue_status_enum;",
            "DROP TYPE IF EXISTS queue_failure_type_enum;",
        ] {
            conn.execute_raw(Statement::from_string(db_backend, sql.to_owned()))
                .await?;
        }

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: dropped merge_queue rows cannot be reconstructed safely.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    use crate::jupiter::{migration::runner::apply_migrations, tests::test_db_connection};

    #[tokio::test]
    async fn drop_merge_queue_removes_table_after_apply_migrations() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('merge_queue')::text AS table_name;".to_owned(),
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
            "expected table 'merge_queue' to be dropped"
        );
    }
}
