//! RM-CC: forward-only DELETE of `code_review` check config/result rows.
//!
//! The CodeReview merge gate is unregistered. PG enum value `code_review`
//! stays (ADR-RM-19). `down` is a no-op.

use sea_orm_migration::{prelude::*, sea_orm::Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db_backend = manager.get_database_backend();
        let conn = manager.get_connection();
        conn.execute_raw(Statement::from_string(
            db_backend,
            r#"DELETE FROM path_check_configs WHERE check_type_code = 'code_review';"#,
        ))
        .await?;
        conn.execute_raw(Statement::from_string(
            db_backend,
            r#"DELETE FROM check_result WHERE check_type_code = 'code_review';"#,
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
    use sea_orm_migration::MigrationTrait;

    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn code_review_row_count(db: &sea_orm::DatabaseConnection, table: &str) -> i64 {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT COUNT(*)::bigint AS n FROM {table} WHERE check_type_code = 'code_review';"
            ),
        );
        let row = db
            .query_one_raw(stmt)
            .await
            .expect("count query")
            .expect("count row");
        row.try_get("", "n").expect("n")
    }

    #[tokio::test]
    async fn delete_code_review_check_rows_after_apply_migrations() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        assert_eq!(code_review_row_count(&db, "path_check_configs").await, 0);
        assert_eq!(code_review_row_count(&db, "check_result").await, 0);

        db.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            r#"INSERT INTO path_check_configs (created_at, updated_at, id, path, check_type_code, enabled, required)
                VALUES (CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 9100500001, '/', 'code_review', true, true);"#
                .to_owned(),
        ))
        .await
        .expect("insert leftover path_check_configs code_review row");
        db.execute_raw(Statement::from_string(
            DbBackend::Postgres,
            r#"INSERT INTO check_result (created_at, updated_at, id, path, cl_link, commit_id, check_type_code, status, message)
                VALUES (CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, 9100500002, '/', 'C0000001', 'deadbeef', 'code_review', 'PASSED', 'seed');"#
                .to_owned(),
        ))
        .await
        .expect("insert leftover check_result code_review row");

        assert_eq!(code_review_row_count(&db, "path_check_configs").await, 1);
        assert_eq!(code_review_row_count(&db, "check_result").await, 1);

        {
            use sea_orm_migration::SchemaManager;
            let manager = SchemaManager::new(&db);
            super::Migration
                .up(&manager)
                .await
                .expect("delete code_review rows");
            super::Migration
                .down(&manager)
                .await
                .expect("down should be a no-op Ok(())");
        }

        assert_eq!(code_review_row_count(&db, "path_check_configs").await, 0);
        assert_eq!(code_review_row_count(&db, "check_result").await, 0);
    }
}
