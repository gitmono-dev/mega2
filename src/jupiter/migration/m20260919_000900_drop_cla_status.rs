//! RM-AD: forward-only drop of `cla_sign_status`.
//!
//! HTTP and the ClaSign merge gate are already gone (RM-AH / RM-AC).
//! Historical CREATE stays in the chain. `down` is a no-op.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(Alias::new("cla_sign_status"))
                    .if_exists()
                    .to_owned(),
            )
            .await
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
    async fn drop_cla_sign_status_after_apply_migrations() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('cla_sign_status')::text AS table_name;".to_owned(),
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
            "expected cla_sign_status to be dropped"
        );
    }
}
