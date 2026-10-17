//! MTR-03: stream scope, ingest attempt, object membership, and ref journal.
//!
//! Forward-only. `down` is an explicit no-op.

use sea_orm_migration::prelude::*;

const UUID_CHECK: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_stream (
                id text PRIMARY KEY,
                principal_id text NOT NULL REFERENCES trace_principal (id),
                target_id text NOT NULL REFERENCES trace_target (id),
                source_repo_id text NOT NULL,
                epoch_id text NOT NULL,
                hash_kind text NOT NULL,
                tip_oid text NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_stream_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_stream_principal_uuid CHECK (principal_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_stream_target_uuid CHECK (target_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_stream_epoch_uuid CHECK (epoch_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_stream_source_present CHECK (length(source_repo_id) > 0),
                CONSTRAINT trace_stream_hash_kind CHECK (hash_kind IN ('sha1', 'sha256')),
                CONSTRAINT trace_stream_natural_key UNIQUE (
                    principal_id, target_id, source_repo_id, epoch_id
                )
            )"
        ))
        .await?;
        conn.execute_unprepared(
            "CREATE OR REPLACE FUNCTION trace_stream_reject_hash_kind_change()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $$
             BEGIN
               IF NEW.hash_kind IS DISTINCT FROM OLD.hash_kind THEN
                 RAISE EXCEPTION 'trace_stream.hash_kind is immutable';
               END IF;
               RETURN NEW;
             END;
             $$",
        )
        .await?;
        conn.execute_unprepared(
            "DROP TRIGGER IF EXISTS trace_stream_hash_kind_immutable ON trace_stream",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE TRIGGER trace_stream_hash_kind_immutable
             BEFORE UPDATE ON trace_stream
             FOR EACH ROW
             EXECUTE FUNCTION trace_stream_reject_hash_kind_change()",
        )
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_ingest_attempt (
                id text PRIMARY KEY,
                stream_id text NOT NULL REFERENCES trace_stream (id),
                idempotency_key text NOT NULL,
                state text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_ingest_attempt_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_ingest_attempt_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_ingest_attempt_key_present CHECK (length(idempotency_key) > 0),
                CONSTRAINT trace_ingest_attempt_state CHECK (
                    state IN ('received', 'published', 'rejected')
                ),
                CONSTRAINT trace_ingest_attempt_idempotency UNIQUE (stream_id, idempotency_key)
            )"
        ))
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_object (
                id text PRIMARY KEY,
                stream_id text NOT NULL REFERENCES trace_stream (id),
                hash_kind text NOT NULL,
                oid text NOT NULL,
                visible boolean NOT NULL DEFAULT false,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_object_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_object_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_object_hash_kind CHECK (hash_kind IN ('sha1', 'sha256')),
                CONSTRAINT trace_object_oid_present CHECK (length(oid) > 0),
                CONSTRAINT trace_object_stream_oid UNIQUE (stream_id, hash_kind, oid)
            )"
        ))
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_ref_update (
                id text PRIMARY KEY,
                stream_id text NOT NULL REFERENCES trace_stream (id),
                ref_sequence bigint NOT NULL,
                idempotency_key text NOT NULL,
                old_tip text NULL,
                new_tip text NOT NULL,
                state text NOT NULL,
                claim_until timestamptz NULL,
                attempt_count bigint NOT NULL DEFAULT 0,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_ref_update_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_ref_update_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_ref_update_sequence_positive CHECK (ref_sequence > 0),
                CONSTRAINT trace_ref_update_key_present CHECK (length(idempotency_key) > 0),
                CONSTRAINT trace_ref_update_tip_present CHECK (length(new_tip) > 0),
                CONSTRAINT trace_ref_update_state CHECK (
                    state IN ('pending', 'claimed', 'indexed', 'failed')
                ),
                CONSTRAINT trace_ref_update_sequence UNIQUE (stream_id, ref_sequence),
                CONSTRAINT trace_ref_update_idempotency UNIQUE (idempotency_key)
            )"
        ))
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

    async fn count(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
        let row = db
            .query_one_raw(Statement::from_string(DbBackend::Postgres, sql.to_owned()))
            .await
            .expect("count")
            .expect("count row");
        row.try_get("", "n").expect("n")
    }

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        for table in [
            "trace_stream",
            "trace_ingest_attempt",
            "trace_object",
            "trace_ref_update",
            "git_repo",
            "agent_capture_session",
        ] {
            assert!(regclass(&db, table).await, "{table} exists");
        }
        db.execute_unprepared(
            "INSERT INTO git_repo (id, repo_path, repo_name, created_at, updated_at) \
             VALUES (91003, '/code/kept', 'kept', now(), now())",
        )
        .await
        .expect("code row");
        db.execute_unprepared(
            "INSERT INTO agent_capture_session (
                deployment_id, tenant_id, repo_id, producer_id, session_kind,
                client_session_id, completeness, created_at, updated_at
             ) VALUES (
                'dep', 'tenant', 'repo', 'producer', 'external_capture',
                'client-mtr03', 'complete', now(), now()
             )",
        )
        .await
        .expect("capture row");
        Migration
            .up(&SchemaManager::new(&db))
            .await
            .expect("upgrade is additive");
        Migration
            .down(&SchemaManager::new(&db))
            .await
            .expect("down is a no-op");
        assert_eq!(
            count(
                &db,
                "SELECT count(*)::bigint AS n FROM git_repo WHERE id = 91003"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*)::bigint AS n FROM agent_capture_session \
                 WHERE client_session_id = 'client-mtr03'"
            )
            .await,
            1
        );
        let registered = count(
            &db,
            "SELECT count(*)::bigint AS n FROM seaql_migrations \
             WHERE version = 'm20261015_000200_trace_publication'",
        )
        .await;
        assert_eq!(registered, 1);
    }
}
