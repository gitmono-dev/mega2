//! AC-02: `agent_capture_*` tables (plan-20260911 schema freeze).
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
            "CREATE TABLE IF NOT EXISTS agent_capture_session (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                deployment_id text NOT NULL,
                tenant_id text NOT NULL,
                repo_id text NOT NULL,
                producer_id text NOT NULL,
                session_kind text NOT NULL,
                client_session_id text NOT NULL,
                started_at timestamptz,
                ended_at timestamptz,
                completeness text NOT NULL DEFAULT 'empty',
                partial_reason text,
                libra_repoid text,
                cl_link text,
                created_at timestamptz NOT NULL DEFAULT now(),
                updated_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT agent_capture_session_kind_check
                    CHECK (session_kind IN ('external_capture', 'internal_code')),
                CONSTRAINT agent_capture_session_completeness_check
                    CHECK (completeness IN ('empty', 'incomplete', 'complete', 'truncated')),
                CONSTRAINT agent_capture_session_natural_key
                    UNIQUE (deployment_id, tenant_id, repo_id, producer_id, session_kind, client_session_id)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS agent_capture_session_tenant_idx
             ON agent_capture_session (deployment_id, tenant_id)",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_event (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                capture_id bigint NOT NULL
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                event_uid text NOT NULL,
                event_kind text NOT NULL,
                native_id text,
                lifecycle_seq bigint,
                payload jsonb NOT NULL,
                payload_fingerprint text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT agent_capture_event_uid_key UNIQUE (capture_id, event_uid)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_source_stream (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                capture_id bigint NOT NULL
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                stream_kind text NOT NULL,
                generation bigint NOT NULL,
                byte_offset bigint NOT NULL DEFAULT 0,
                CONSTRAINT agent_capture_source_stream_key
                    UNIQUE (capture_id, stream_kind, generation)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_checkpoint (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                capture_id bigint NOT NULL
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                checkpoint_id text NOT NULL,
                transcript_digest text,
                metadata jsonb,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT agent_capture_checkpoint_id_key UNIQUE (capture_id, checkpoint_id)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_file_op (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                capture_id bigint NOT NULL
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                source_event_uid text NOT NULL,
                op text NOT NULL,
                path text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT agent_capture_file_op_op_check
                    CHECK (op IN ('read', 'write', 'patch', 'delete', 'search')),
                CONSTRAINT agent_capture_file_op_source_event_fk
                    FOREIGN KEY (capture_id, source_event_uid)
                    REFERENCES agent_capture_event (capture_id, event_uid)
                    ON DELETE RESTRICT
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_blob (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                deployment_id text NOT NULL,
                tenant_id text NOT NULL,
                digest text NOT NULL,
                visibility text NOT NULL,
                object_key text NOT NULL,
                size_bytes bigint NOT NULL DEFAULT 0,
                lease_state text NOT NULL,
                lease_generation bigint NOT NULL DEFAULT 0,
                lease_id text,
                lease_expires_at timestamptz,
                capture_id bigint
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                upload_intent text,
                cleanup_intent text,
                created_at timestamptz NOT NULL DEFAULT now(),
                updated_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT agent_capture_blob_visibility_check
                    CHECK (visibility IN ('raw', 'redacted')),
                CONSTRAINT agent_capture_blob_lease_state_check
                    CHECK (lease_state IN (
                        'staging',
                        'finalizing',
                        'committed',
                        'expired',
                        'aborted',
                        'protected'
                    )),
                CONSTRAINT agent_capture_blob_digest_key
                    UNIQUE (deployment_id, tenant_id, digest, visibility)
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS agent_capture_blob_lease_idx
             ON agent_capture_blob (capture_id, lease_id)",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_blob_ref (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                blob_id bigint NOT NULL
                    REFERENCES agent_capture_blob (id) ON DELETE RESTRICT,
                owner_session_id bigint
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                owner_event_id bigint
                    REFERENCES agent_capture_event (id) ON DELETE RESTRICT,
                owner_checkpoint_id bigint
                    REFERENCES agent_capture_checkpoint (id) ON DELETE RESTRICT,
                owner_file_op_id bigint
                    REFERENCES agent_capture_file_op (id) ON DELETE RESTRICT,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT agent_capture_blob_ref_one_owner_check CHECK (
                    (owner_session_id IS NOT NULL)::int
                    + (owner_event_id IS NOT NULL)::int
                    + (owner_checkpoint_id IS NOT NULL)::int
                    + (owner_file_op_id IS NOT NULL)::int = 1
                )
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_ingest_receipt (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                deployment_id text NOT NULL,
                tenant_id text NOT NULL,
                producer_id text NOT NULL,
                capture_id bigint NOT NULL
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                operation text NOT NULL,
                idempotency_key text NOT NULL,
                fingerprint text NOT NULL,
                response jsonb,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT agent_capture_ingest_receipt_key UNIQUE (
                    deployment_id, tenant_id, producer_id, capture_id, operation, idempotency_key
                )
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_stream_blob (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                capture_id bigint NOT NULL
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                stream_kind text NOT NULL,
                generation bigint NOT NULL,
                blob_id bigint NOT NULL
                    REFERENCES agent_capture_blob (id) ON DELETE RESTRICT,
                CONSTRAINT agent_capture_stream_blob_key
                    UNIQUE (capture_id, stream_kind, generation),
                CONSTRAINT agent_capture_stream_blob_stream_fk
                    FOREIGN KEY (capture_id, stream_kind, generation)
                    REFERENCES agent_capture_source_stream (capture_id, stream_kind, generation)
                    ON DELETE RESTRICT
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_access_audit (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                deployment_id text NOT NULL,
                tenant_id text NOT NULL,
                capture_id bigint NOT NULL
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                actor text NOT NULL,
                action text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now()
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_tombstone (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                deployment_id text NOT NULL,
                tenant_id text NOT NULL,
                repo_id text NOT NULL,
                producer_id text NOT NULL,
                session_kind text NOT NULL,
                client_session_id text NOT NULL,
                capture_id bigint
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT agent_capture_tombstone_natural_key UNIQUE (
                    deployment_id, tenant_id, repo_id, producer_id, session_kind, client_session_id
                )
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS agent_capture_tombstone_capture_id_idx
             ON agent_capture_tombstone (capture_id)",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS agent_capture_access_audit_scope_idx
             ON agent_capture_access_audit (deployment_id, tenant_id, capture_id)",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS agent_capture_deletion_ledger (
                id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                deployment_id text NOT NULL,
                tenant_id text NOT NULL,
                blob_id bigint
                    REFERENCES agent_capture_blob (id) ON DELETE RESTRICT,
                capture_id bigint
                    REFERENCES agent_capture_session (id) ON DELETE RESTRICT,
                intent text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now()
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

    const ALL_TABLES: &[&str] = &[
        "agent_capture_session",
        "agent_capture_event",
        "agent_capture_source_stream",
        "agent_capture_checkpoint",
        "agent_capture_file_op",
        "agent_capture_blob",
        "agent_capture_blob_ref",
        "agent_capture_ingest_receipt",
        "agent_capture_stream_blob",
        "agent_capture_access_audit",
        "agent_capture_tombstone",
        "agent_capture_deletion_ledger",
    ];

    async fn apply(db: &sea_orm::DatabaseConnection) {
        apply_migrations(db, true)
            .await
            .expect("migrations should apply");
    }

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

    async fn unique_columns(db: &sea_orm::DatabaseConnection, table: &str) -> Vec<String> {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT kcu.column_name
                 FROM information_schema.table_constraints tc
                 JOIN information_schema.key_column_usage kcu
                   ON tc.constraint_catalog = kcu.constraint_catalog
                  AND tc.constraint_schema = kcu.constraint_schema
                  AND tc.constraint_name = kcu.constraint_name
                  AND tc.table_schema = kcu.table_schema
                  AND tc.table_name = kcu.table_name
                 WHERE tc.table_schema = current_schema()
                   AND tc.table_name = '{table}'
                   AND tc.constraint_type = 'UNIQUE'
                 ORDER BY tc.constraint_name, kcu.ordinal_position"
            ),
        );
        let rows = db.query_all_raw(stmt).await.expect("query unique columns");
        rows.into_iter()
            .map(|row| {
                row.try_get::<String>("", "column_name")
                    .expect("column_name")
            })
            .collect()
    }

    async fn column_exists(db: &sea_orm::DatabaseConnection, table: &str, column: &str) -> bool {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT column_name FROM information_schema.columns
                 WHERE table_schema = current_schema()
                   AND table_name = '{table}'
                   AND column_name = '{column}'"
            ),
        );
        db.query_one_raw(stmt)
            .await
            .expect("query information_schema")
            .is_some()
    }

    #[tokio::test]
    async fn migration_creates_session_table() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply(&db).await;
        assert_table_exists(&db, "agent_capture_session").await;
    }

    #[tokio::test]
    async fn migration_session_has_no_user_id() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply(&db).await;
        assert!(
            !column_exists(&db, "agent_capture_session", "user_id").await,
            "agent_capture_session must not have user_id"
        );
    }

    #[tokio::test]
    async fn migration_session_unique_tuple() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply(&db).await;
        let cols = unique_columns(&db, "agent_capture_session").await;
        assert_eq!(
            cols,
            [
                "deployment_id",
                "tenant_id",
                "repo_id",
                "producer_id",
                "session_kind",
                "client_session_id",
            ]
        );
    }

    #[tokio::test]
    async fn migration_event_unique_uid() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply(&db).await;
        let cols = unique_columns(&db, "agent_capture_event").await;
        assert_eq!(cols, ["capture_id", "event_uid"]);
    }

    #[tokio::test]
    async fn migration_blob_unique_digest() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply(&db).await;
        let cols = unique_columns(&db, "agent_capture_blob").await;
        assert_eq!(cols, ["deployment_id", "tenant_id", "digest", "visibility"]);
    }

    #[tokio::test]
    async fn migration_constraints_and_scope() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply(&db).await;
        for table in ALL_TABLES {
            assert_table_exists(&db, table).await;
        }

        let fk_stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT rc.delete_rule
             FROM information_schema.referential_constraints rc
             JOIN information_schema.table_constraints tc
               ON rc.constraint_name = tc.constraint_name
              AND rc.constraint_schema = tc.table_schema
             WHERE tc.table_schema = current_schema()
               AND tc.table_name = 'agent_capture_event'
               AND tc.constraint_type = 'FOREIGN KEY'"
                .to_owned(),
        );
        let row = db
            .query_one_raw(fk_stmt)
            .await
            .expect("query FK")
            .expect("event FK exists");
        let delete_rule: String = row.try_get("", "delete_rule").expect("delete_rule");
        assert_eq!(delete_rule, "RESTRICT");

        let check_stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT pg_get_constraintdef(oid) AS def
             FROM pg_constraint
             WHERE conrelid = 'agent_capture_blob_ref'::regclass
               AND conname = 'agent_capture_blob_ref_one_owner_check'"
                .to_owned(),
        );
        let check = db
            .query_one_raw(check_stmt)
            .await
            .expect("query blob_ref check")
            .expect("one-owner check exists");
        let def: String = check.try_get("", "def").expect("def");
        assert!(
            def.contains("owner_session_id")
                && def.contains("owner_event_id")
                && def.contains("owner_checkpoint_id")
                && def.contains("owner_file_op_id"),
            "blob_ref must CHECK exactly one typed owner: {def}"
        );
    }
}
