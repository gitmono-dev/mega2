//! MTR-02: stable principal, credential-namespace, and target identity.
//!
//! Forward-only. `down` is an explicit no-op so an upgrade cannot drop these
//! tables or the code and Capture tables that already exist.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS trace_principal (
                id text PRIMARY KEY,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_principal_id_uuid CHECK (
                    id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                )
            )",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS trace_credential_binding (
                id text PRIMARY KEY,
                principal_id text NOT NULL REFERENCES trace_principal (id),
                namespace text NOT NULL,
                credential_id text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                retired_at timestamptz NULL,
                CONSTRAINT trace_credential_binding_id_uuid CHECK (
                    id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                ),
                CONSTRAINT trace_credential_binding_principal_uuid CHECK (
                    principal_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                ),
                CONSTRAINT trace_credential_binding_namespace_present CHECK (length(namespace) > 0),
                CONSTRAINT trace_credential_binding_credential_present CHECK (length(credential_id) > 0)
            )",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS trace_credential_binding_active_key
                ON trace_credential_binding (namespace, credential_id)
                WHERE retired_at IS NULL",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS trace_target (
                id text PRIMARY KEY,
                target_kind text NOT NULL,
                project_path text NOT NULL,
                backing_import_repo_id text NULL,
                active boolean NOT NULL DEFAULT true,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_target_id_uuid CHECK (
                    id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                ),
                CONSTRAINT trace_target_kind CHECK (target_kind IN ('native', 'import')),
                CONSTRAINT trace_target_path_present CHECK (length(project_path) > 0)
            )",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS trace_target_one_active_path
                ON trace_target (project_path)
                WHERE active",
        )
        .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    use sea_orm_migration::prelude::*;

    use super::Migration;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn regclass(db: &sea_orm::DatabaseConnection, table: &str) -> bool {
        let row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name"),
            ))
            .await
            .expect("catalog query")
            .expect("catalog row");
        row.try_get::<Option<String>>("", "table_name")
            .expect("table_name")
            .is_some()
    }

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        for table in [
            "trace_principal",
            "trace_credential_binding",
            "trace_target",
            "git_repo",
            "agent_capture_session",
        ] {
            assert!(regclass(&db, table).await, "{table} remains after upgrade");
        }

        let registered = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*)::bigint AS n FROM seaql_migrations \
                 WHERE version = 'm20261015_000100_trace_identity'",
            ))
            .await
            .expect("migration ledger")
            .expect("ledger row");
        let n: i64 = registered.try_get("", "n").expect("count");
        assert_eq!(n, 1, "identity migration is registered");

        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("down is a no-op");
        for table in [
            "trace_principal",
            "trace_credential_binding",
            "trace_target",
            "git_repo",
            "agent_capture_session",
        ] {
            assert!(regclass(&db, table).await, "{table} survives down");
        }
    }
}
