//! RM-CF: forward-only drop of the four `mega_code_review_*` tables.
//!
//! Jupiter impl and callisto bindings are already gone (RM-CD / RM-CE).
//! Historical CREATE stays in the chain. PG enum values stay. `down` is a no-op.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for table in [
            "mega_code_review_position",
            "mega_code_review_comment",
            "mega_code_review_anchor",
            "mega_code_review_thread",
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
    async fn drop_mega_code_review_tables_after_apply_migrations() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        for table in [
            "mega_code_review_position",
            "mega_code_review_comment",
            "mega_code_review_anchor",
            "mega_code_review_thread",
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
            assert!(table_name.is_none(), "expected {table} to be dropped");
        }
    }
}
