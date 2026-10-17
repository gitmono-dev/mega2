//! MTR-04: rebuildable read-model tables. No extension and no trigram GIN.

use sea_orm_migration::prelude::*;

const UUID_CHECK: &str = "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_checkpoint (
                id text PRIMARY KEY,
                stream_id text NOT NULL REFERENCES trace_stream (id),
                checkpoint_key text NOT NULL,
                original_tree text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_checkpoint_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_checkpoint_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_checkpoint_key_present CHECK (length(checkpoint_key) > 0),
                CONSTRAINT trace_checkpoint_tree_present CHECK (length(original_tree) > 0),
                CONSTRAINT trace_checkpoint_stream_key UNIQUE (stream_id, checkpoint_key),
                CONSTRAINT trace_checkpoint_stream_row UNIQUE (stream_id, id)
            )"
        ))
        .await?;
        conn.execute_unprepared(
            "CREATE OR REPLACE FUNCTION trace_checkpoint_reject_tree_change()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $$
             BEGIN
               IF NEW.original_tree IS DISTINCT FROM OLD.original_tree THEN
                 RAISE EXCEPTION 'trace_checkpoint.original_tree is immutable';
               END IF;
               RETURN NEW;
             END;
             $$",
        )
        .await?;
        conn.execute_unprepared(
            "DROP TRIGGER IF EXISTS trace_checkpoint_tree_immutable ON trace_checkpoint",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE TRIGGER trace_checkpoint_tree_immutable
             BEFORE UPDATE ON trace_checkpoint
             FOR EACH ROW
             EXECUTE FUNCTION trace_checkpoint_reject_tree_change()",
        )
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_session (
                id text PRIMARY KEY,
                stream_id text NOT NULL REFERENCES trace_stream (id),
                session_key text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_session_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_session_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_session_key_present CHECK (length(session_key) > 0),
                CONSTRAINT trace_session_stream_key UNIQUE (stream_id, session_key)
            )"
        ))
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_resource (
                id text PRIMARY KEY,
                stream_id text NOT NULL REFERENCES trace_stream (id),
                checkpoint_id text NOT NULL REFERENCES trace_checkpoint (id),
                ordinal bigint NOT NULL,
                byte_start bigint NOT NULL,
                byte_end bigint NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_resource_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_resource_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_resource_checkpoint_uuid CHECK (checkpoint_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_resource_ordinal_nonnegative CHECK (ordinal >= 0),
                CONSTRAINT trace_resource_range_ordered CHECK (
                    byte_start >= 0 AND byte_end >= byte_start
                ),
                CONSTRAINT trace_resource_checkpoint_ordinal UNIQUE (checkpoint_id, ordinal),
                CONSTRAINT trace_resource_stream_row UNIQUE (stream_id, id),
                CONSTRAINT trace_resource_checkpoint_row UNIQUE (checkpoint_id, id),
                CONSTRAINT trace_resource_checkpoint_stream FOREIGN KEY (stream_id, checkpoint_id)
                    REFERENCES trace_checkpoint (stream_id, id)
            )"
        ))
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS trace_resource_stream_checkpoint
             ON trace_resource (stream_id, checkpoint_id)",
        )
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_code_link (
                id text PRIMARY KEY,
                stream_id text NOT NULL REFERENCES trace_stream (id),
                checkpoint_id text NOT NULL REFERENCES trace_checkpoint (id),
                code_oid text NULL,
                link_state text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_code_link_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_code_link_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_code_link_checkpoint_uuid CHECK (checkpoint_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_code_link_state CHECK (link_state IN ('unresolved', 'resolved')),
                CONSTRAINT trace_code_link_resolved_oid CHECK (
                    link_state <> 'resolved'
                    OR (code_oid IS NOT NULL AND length(code_oid) > 0)
                ),
                CONSTRAINT trace_code_link_checkpoint_stream FOREIGN KEY (stream_id, checkpoint_id)
                    REFERENCES trace_checkpoint (stream_id, id)
            )"
        ))
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_fragment (
                id text PRIMARY KEY,
                stream_id text NOT NULL REFERENCES trace_stream (id),
                content_digest text NOT NULL,
                index_version bigint NOT NULL,
                body text NOT NULL,
                ascii_fold text NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_fragment_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_fragment_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_fragment_digest_present CHECK (length(content_digest) > 0),
                CONSTRAINT trace_fragment_index_version_positive CHECK (index_version > 0),
                CONSTRAINT trace_fragment_stream_digest_version UNIQUE (
                    stream_id, content_digest, index_version
                ),
                CONSTRAINT trace_fragment_stream_row UNIQUE (stream_id, id)
            )"
        ))
        .await?;
        conn.execute_unprepared(
            "CREATE OR REPLACE FUNCTION trace_fragment_reject_identity_change()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $$
             BEGIN
               IF NEW.content_digest IS DISTINCT FROM OLD.content_digest
                  OR NEW.index_version IS DISTINCT FROM OLD.index_version
                  OR NEW.stream_id IS DISTINCT FROM OLD.stream_id THEN
                 RAISE EXCEPTION 'trace_fragment identity is immutable';
               END IF;
               RETURN NEW;
             END;
             $$",
        )
        .await?;
        conn.execute_unprepared(
            "DROP TRIGGER IF EXISTS trace_fragment_identity_immutable ON trace_fragment",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE TRIGGER trace_fragment_identity_immutable
             BEFORE UPDATE ON trace_fragment
             FOR EACH ROW
             EXECUTE FUNCTION trace_fragment_reject_identity_change()",
        )
        .await?;
        conn.execute_unprepared(&format!(
            "CREATE TABLE IF NOT EXISTS trace_fragment_membership (
                id text PRIMARY KEY,
                fragment_id text NOT NULL REFERENCES trace_fragment (id),
                stream_id text NOT NULL REFERENCES trace_stream (id),
                checkpoint_id text NOT NULL REFERENCES trace_checkpoint (id),
                resource_id text NOT NULL REFERENCES trace_resource (id),
                created_at timestamptz NOT NULL DEFAULT now(),
                CONSTRAINT trace_fragment_membership_id_uuid CHECK (id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_fragment_membership_fragment_uuid CHECK (
                    fragment_id ~* '{UUID_CHECK}'
                ),
                CONSTRAINT trace_fragment_membership_stream_uuid CHECK (stream_id ~* '{UUID_CHECK}'),
                CONSTRAINT trace_fragment_membership_checkpoint_uuid CHECK (
                    checkpoint_id ~* '{UUID_CHECK}'
                ),
                CONSTRAINT trace_fragment_membership_resource_uuid CHECK (
                    resource_id ~* '{UUID_CHECK}'
                ),
                CONSTRAINT trace_fragment_membership_once UNIQUE (
                    fragment_id, checkpoint_id, resource_id
                ),
                CONSTRAINT trace_fragment_membership_fragment_stream FOREIGN KEY (
                    stream_id, fragment_id
                ) REFERENCES trace_fragment (stream_id, id),
                CONSTRAINT trace_fragment_membership_checkpoint_stream FOREIGN KEY (
                    stream_id, checkpoint_id
                ) REFERENCES trace_checkpoint (stream_id, id),
                CONSTRAINT trace_fragment_membership_resource_stream FOREIGN KEY (
                    stream_id, resource_id
                ) REFERENCES trace_resource (stream_id, id),
                CONSTRAINT trace_fragment_membership_resource_checkpoint FOREIGN KEY (
                    checkpoint_id, resource_id
                ) REFERENCES trace_resource (checkpoint_id, id)
            )"
        ))
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS trace_fragment_membership_stream_checkpoint
             ON trace_fragment_membership (stream_id, checkpoint_id)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE OR REPLACE FUNCTION trace_fragment_membership_reject_provenance_change()
             RETURNS trigger
             LANGUAGE plpgsql
             AS $$
             BEGIN
               IF NEW.fragment_id IS DISTINCT FROM OLD.fragment_id
                  OR NEW.stream_id IS DISTINCT FROM OLD.stream_id
                  OR NEW.checkpoint_id IS DISTINCT FROM OLD.checkpoint_id
                  OR NEW.resource_id IS DISTINCT FROM OLD.resource_id THEN
                 RAISE EXCEPTION 'trace_fragment_membership provenance is immutable';
               END IF;
               RETURN NEW;
             END;
             $$",
        )
        .await?;
        conn.execute_unprepared(
            "DROP TRIGGER IF EXISTS trace_fragment_membership_provenance_immutable
             ON trace_fragment_membership",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE TRIGGER trace_fragment_membership_provenance_immutable
             BEFORE UPDATE ON trace_fragment_membership
             FOR EACH ROW
             EXECUTE FUNCTION trace_fragment_membership_reject_provenance_change()",
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
    use crate::jupiter::{
        migration::apply_migrations,
        tests::{test_database_without_extensions, test_db_connection},
    };

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
            "trace_checkpoint",
            "trace_session",
            "trace_resource",
            "trace_code_link",
            "trace_fragment",
            "trace_fragment_membership",
        ] {
            assert!(regclass(&db, table).await, "{table} exists");
        }
        assert_eq!(
            count(
                &db,
                "SELECT count(*)::bigint AS n FROM pg_indexes \
                 WHERE tablename IN (
                    'trace_checkpoint', 'trace_session', 'trace_resource',
                    'trace_code_link', 'trace_fragment', 'trace_fragment_membership'
                 ) AND indexdef ILIKE '%gin%'"
            )
            .await,
            0,
            "projection indexes are not trigram GIN"
        );
        db.execute_unprepared(
            "INSERT INTO trace_principal (id, created_at) \
             VALUES ('00000000-0000-4000-8000-000000000101', now())",
        )
        .await
        .expect("principal");
        db.execute_unprepared(
            "INSERT INTO trace_target (
                id, target_kind, project_path, active, created_at
             ) VALUES (
                '00000000-0000-4000-8000-000000000102', 'native', '/proj', true, now()
             )",
        )
        .await
        .expect("target");
        db.execute_unprepared(
            "INSERT INTO trace_stream (
                id, principal_id, target_id, source_repo_id, epoch_id, hash_kind, created_at
             ) VALUES (
                '00000000-0000-4000-8000-000000000103',
                '00000000-0000-4000-8000-000000000101',
                '00000000-0000-4000-8000-000000000102',
                'source', '00000000-0000-4000-8000-000000000104', 'sha1', now()
             )",
        )
        .await
        .expect("stream");
        db.execute_unprepared(
            "INSERT INTO trace_checkpoint (id, stream_id, checkpoint_key, original_tree, created_at) \
             VALUES (
                '00000000-0000-4000-8000-000000000105',
                '00000000-0000-4000-8000-000000000103',
                'ckpt-1', 'tree-aaa', now()
             )",
        )
        .await
        .expect("checkpoint");
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
                "SELECT count(*)::bigint AS n FROM trace_checkpoint \
                 WHERE checkpoint_key = 'ckpt-1'"
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*)::bigint AS n FROM seaql_migrations \
                 WHERE version = 'm20261015_000300_trace_projection'"
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn ordinary_schema_without_extensions_() {
        let (db, _schema) = test_database_without_extensions()
            .await
            .expect("template0 database");
        let before = count(
            &db,
            "SELECT count(*)::bigint AS n FROM pg_catalog.pg_extension WHERE extname = 'pg_trgm'",
        )
        .await;
        assert_eq!(before, 0, "template0 starts without pg_trgm");
        apply_migrations(&db, true)
            .await
            .expect("ordinary schema upgrade does not need the extension");
        let after = count(
            &db,
            "SELECT count(*)::bigint AS n FROM pg_catalog.pg_extension WHERE extname = 'pg_trgm'",
        )
        .await;
        assert_eq!(after, 0, "upgrade does not install or drop pg_trgm");
        assert!(regclass(&db, "trace_fragment").await);
        assert_eq!(
            count(
                &db,
                "SELECT count(*)::bigint AS n FROM pg_indexes \
                 WHERE tablename = 'trace_fragment' AND indexdef ILIKE '%gin%'"
            )
            .await,
            0
        );
    }
}
