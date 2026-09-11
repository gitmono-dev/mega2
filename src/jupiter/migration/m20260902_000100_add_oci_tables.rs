//! DR-02: OCI Distribution metadata tables (ADR-DR-03 / ADR-DR-06).
//!
//! Four new tables back the storage-only `/v2` surface:
//! - `oci_manifest` — repo + digest → media type / size
//! - `oci_tag` — repo + tag → digest
//! - `oci_blob_ref` — repo membership of a CAS blob digest
//! - `oci_upload` — chunked upload session (`offset` + `chunks`)
//!
//! Forward-only (`down` is empty): this checkout has no runtime `migrate down`
//! entry point; recovery is a follow-up DROP migration.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS oci_manifest (
                repo_name text NOT NULL,
                digest text NOT NULL,
                media_type text NOT NULL,
                size bigint NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (repo_name, digest)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS oci_tag (
                repo_name text NOT NULL,
                tag text NOT NULL,
                digest text NOT NULL,
                updated_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (repo_name, tag)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS oci_blob_ref (
                repo_name text NOT NULL,
                digest text NOT NULL,
                size bigint NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (repo_name, digest)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS oci_upload (
                uuid text NOT NULL,
                repo_name text NOT NULL,
                \"offset\" bigint NOT NULL DEFAULT 0,
                chunks bigint NOT NULL DEFAULT 0,
                started_at timestamptz NOT NULL DEFAULT now(),
                updated_at timestamptz NOT NULL DEFAULT now(),
                PRIMARY KEY (uuid)
            )",
        )
        .await
        .map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    use crate::jupiter::{migration::runner::apply_migrations, tests::test_db_connection};

    async fn assert_table_exists(db: &sea_orm::DatabaseConnection, table: &str) {
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
            table_name.is_some(),
            "expected table '{table}' to exist after migrations"
        );
    }

    #[tokio::test]
    async fn oci_manifest_table_exists() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        assert_table_exists(&db, "oci_manifest").await;
    }

    #[tokio::test]
    async fn oci_tag_table_exists() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        assert_table_exists(&db, "oci_tag").await;
    }

    #[tokio::test]
    async fn oci_blob_ref_table_exists() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        assert_table_exists(&db, "oci_blob_ref").await;
    }

    #[tokio::test]
    async fn oci_upload_table_exists() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        assert_table_exists(&db, "oci_upload").await;
    }

    #[tokio::test]
    async fn oci_upload_has_chunks_column() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT column_name FROM information_schema.columns \
             WHERE table_name = 'oci_upload' AND column_name = 'chunks'"
                .to_owned(),
        );
        let row = db
            .query_one_raw(stmt)
            .await
            .expect("query information_schema")
            .expect("chunks column should exist");
        let column_name: String = row.try_get("", "column_name").expect("column_name present");
        assert_eq!(column_name, "chunks");
    }
}
