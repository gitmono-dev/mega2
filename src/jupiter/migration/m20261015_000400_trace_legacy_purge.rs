//! MTR-23: legacy Capture purge journal, inventory items and the cross-instance
//! writer fence. The migration is additive: it never drops a legacy table and
//! its `down` is an explicit no-op. While one active fence row exists, the
//! barrier function refuses runtime DML on all 12 legacy Capture tables.

use sea_orm_migration::prelude::*;

const UUID_CHECK: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";

/// The 12 legacy Capture tables owned by the pre-H6 subsystem.
const LEGACY_TABLES: [&str; 12] = [
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

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_legacy_purge (
                id text PRIMARY KEY,
                backend_fingerprint text NOT NULL,
                storage_location text NOT NULL,
                deployment_id text NOT NULL,
                manifest_digest text NOT NULL,
                state text NOT NULL DEFAULT 'frozen',
                cursor text NULL,
                retry_count integer NOT NULL DEFAULT 0,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_legacy_purge_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_legacy_purge_fingerprint_present CHECK (length(backend_fingerprint) > 0),
                CONSTRAINT trace_legacy_purge_location_present CHECK (length(storage_location) > 0),
                CONSTRAINT trace_legacy_purge_deployment_present CHECK (length(deployment_id) > 0),
                CONSTRAINT trace_legacy_purge_manifest_present CHECK (length(manifest_digest) > 0),
                CONSTRAINT trace_legacy_purge_state CHECK (state IN ('frozen', 'deleting', 'rescanning', 'complete'))
            )"
        ))
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_legacy_purge_item (
                id text PRIMARY KEY,
                purge_id text NOT NULL REFERENCES trace_legacy_purge (id) ON DELETE CASCADE,
                scope text NOT NULL,
                object_key text NOT NULL,
                provider_version text NULL,
                provider_generation text NULL,
                deleted_at timestamptz NULL,
                CONSTRAINT trace_legacy_purge_item_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_legacy_purge_item_scope_present CHECK (length(scope) > 0),
                CONSTRAINT trace_legacy_purge_item_key_present CHECK (length(object_key) > 0),
                CONSTRAINT trace_legacy_purge_item_natural_key UNIQUE (purge_id, scope, object_key)
            )"
        ))
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_legacy_writer_fence (
                id text PRIMARY KEY,
                purge_id text NOT NULL UNIQUE REFERENCES trace_legacy_purge (id) ON DELETE RESTRICT,
                deployment_id text NOT NULL,
                active boolean NOT NULL DEFAULT true,
                activated_at timestamptz NOT NULL DEFAULT now(),
                released_at timestamptz NULL,
                CONSTRAINT trace_legacy_writer_fence_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_legacy_writer_fence_deployment_present CHECK (length(deployment_id) > 0),
                CONSTRAINT trace_legacy_writer_fence_released CHECK (active OR released_at IS NOT NULL)
            )"
        ))
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS trace_legacy_writer_fence_active
             ON trace_legacy_writer_fence (active) WHERE active",
        )
        .await?;
        // The barrier is a single DB-level function consulted by a trigger on
        // every legacy table. An inactive fence keeps the pre-purge behavior.
        conn.execute_unprepared(
            "CREATE OR REPLACE FUNCTION trace_legacy_write_fence()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $$
             BEGIN
               IF EXISTS (SELECT 1 FROM trace_legacy_writer_fence WHERE active) THEN
                 RAISE EXCEPTION 'legacy capture writes are fenced for purge';
               END IF;
               IF TG_OP = 'DELETE' THEN
                 RETURN OLD;
               END IF;
               RETURN NEW;
             END;
             $$",
        )
        .await?;
        for table in LEGACY_TABLES {
            conn.execute_unprepared(&format!(
                "DROP TRIGGER IF EXISTS {table}_legacy_write_fence ON {table}"
            ))
            .await?;
            conn.execute_unprepared(&format!(
                "CREATE TRIGGER {table}_legacy_write_fence
                 BEFORE INSERT OR UPDATE OR DELETE ON {table}
                 FOR EACH ROW
                 EXECUTE FUNCTION trace_legacy_write_fence()"
            ))
            .await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: the journal, its items and the fence are the durable
        // record of a destructive purge. Dropping them would silently forget
        // what was deleted.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    #[tokio::test]
    async fn db_contract() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");

        for table in super::LEGACY_TABLES {
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT count(*) AS n FROM pg_trigger
                     WHERE tgname = $1 AND NOT tgisinternal",
                    [format!("{table}_legacy_write_fence").into()],
                ))
                .await
                .expect("trigger lookup")
                .expect("trigger count");
            assert_eq!(
                row.try_get::<i64>("", "n").unwrap(),
                1,
                "{table} must carry the purge barrier trigger"
            );
        }
        // The barrier function reads the persistent fence.
        let function = db
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT count(*) AS n FROM pg_proc WHERE proname = 'trace_legacy_write_fence'",
                [],
            ))
            .await
            .expect("function lookup")
            .expect("function count");
        assert_eq!(function.try_get::<i64>("", "n").unwrap(), 1);

        // AC-8: rolling this migration back runs its explicit no-op down; the
        // durable journal and the barrier survive.
        use sea_orm_migration::MigratorTrait;
        crate::jupiter::migration::Migrator::down(&db, Some(1))
            .await
            .expect("the down migration is a no-op");
        let journal = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT to_regclass('trace_legacy_purge') IS NOT NULL AS present",
            ))
            .await
            .expect("journal lookup")
            .expect("journal row");
        assert!(journal.try_get::<bool>("", "present").unwrap());
        db.close().await.ok();
    }
}
