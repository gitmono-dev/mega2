//! Forward-only migration for the CL <-> Orion2 CI build pipeline (`CLB-10`):
//! - Adds monotonic `mega_cl.head_revision` (`BIGINT NOT NULL DEFAULT 1`), separate from TP-07 `revision`.
//! - Adds CI tracking columns to `check_result` (`head_revision`, `ci_generation`, `ci_task_id`,
//!   `ci_attempt_id`, `details_url`).
//! - Creates `ci_callback_nonces` for persistent V2 callback replay protection.
//! - Creates `cl_webhook_outbox` with `lease_epoch` fencing and `(state, next_retry_at)` index.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        conn.execute_unprepared(
            "ALTER TABLE mega_cl \
             ADD COLUMN IF NOT EXISTS head_revision BIGINT NOT NULL DEFAULT 1",
        )
        .await?;

        conn.execute_unprepared(
            "ALTER TABLE check_result \
             ADD COLUMN IF NOT EXISTS head_revision BIGINT NOT NULL DEFAULT 0, \
             ADD COLUMN IF NOT EXISTS ci_generation BIGINT NOT NULL DEFAULT 0, \
             ADD COLUMN IF NOT EXISTS ci_task_id TEXT, \
             ADD COLUMN IF NOT EXISTS ci_attempt_id TEXT, \
             ADD COLUMN IF NOT EXISTS details_url TEXT",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS ci_callback_nonces (
                nonce TEXT PRIMARY KEY,
                cl_link TEXT NOT NULL,
                body_sha256 TEXT NOT NULL,
                expires_at TIMESTAMP NOT NULL
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_ci_callback_nonces_expires_at \
             ON ci_callback_nonces (expires_at)",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS cl_webhook_outbox (
                id BIGINT PRIMARY KEY,
                event_id TEXT NOT NULL UNIQUE,
                event_type TEXT NOT NULL,
                cl_link TEXT NOT NULL,
                cl_path TEXT NOT NULL,
                head_revision BIGINT NOT NULL,
                payload_json TEXT NOT NULL,
                state TEXT NOT NULL CHECK (state IN ('pending', 'leased', 'delivered', 'dead_letter')),
                lease_owner TEXT,
                lease_epoch BIGINT NOT NULL DEFAULT 0 CHECK (lease_epoch >= 0),
                lease_expires_at TIMESTAMP,
                attempt_count INT NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
                next_retry_at TIMESTAMP NOT NULL,
                last_error TEXT,
                created_at TIMESTAMP NOT NULL,
                updated_at TIMESTAMP NOT NULL
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_cl_webhook_outbox_state_next_retry \
             ON cl_webhook_outbox (state, next_retry_at)",
        )
        .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only migration: never drop CI tracking columns, replay nonces, or webhook outbox rows.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, ConnectionTrait, DbBackend, EntityTrait, Set, Statement};

    use super::*;
    use crate::{
        callisto::{
            check_result, ci_callback_nonces, cl_webhook_outbox, mega_cl,
            sea_orm_active_enums::CheckTypeEnum,
        },
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    #[tokio::test]
    async fn cl_ci_check_columns_and_tables_migrate_forward_only_and_idempotently() {
        let root = tempfile::tempdir().unwrap();
        let db = test_db_connection(root.path()).await;
        apply_migrations(&db, true).await.unwrap();

        // Idempotent re-application and forward-only down() no-op.
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await.unwrap();
        Migration.down(&manager).await.unwrap();

        // 1. mega_cl.head_revision defaults to 1 and leaves TP-07 revision untouched.
        let now = chrono::Utc::now().naive_utc();
        db.execute_unprepared(
            "INSERT INTO mega_cl (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, username, base_branch, revision)
             VALUES (1001, 'CLMIG001', 'migration test', 'open', '/project', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', now(), now(), 'alice', 'main', 7)",
        )
        .await
        .unwrap();
        let cl_row = mega_cl::Entity::find_by_id(1001)
            .one(&db)
            .await
            .unwrap()
            .expect("inserted mega_cl row");
        assert_eq!(cl_row.head_revision, 1);
        assert_eq!(cl_row.revision, 7);

        // 2. check_result CI columns default cleanly and accept nullable metadata.
        let check_model = check_result::Model::new(
            "/project",
            "CLMIG001",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            CheckTypeEnum::CiStatus,
            "PENDING",
            "queued",
        );
        let check_id = check_model.id;
        let active_check: check_result::ActiveModel = check_model.into();
        active_check.insert(&db).await.unwrap();
        let loaded_check = check_result::Entity::find_by_id(check_id)
            .one(&db)
            .await
            .unwrap()
            .expect("inserted check_result row");
        assert_eq!(loaded_check.head_revision, 0);
        assert_eq!(loaded_check.ci_generation, 0);
        assert_eq!(loaded_check.ci_task_id, None);
        assert_eq!(loaded_check.ci_attempt_id, None);
        assert_eq!(loaded_check.details_url, None);

        // 3. ci_callback_nonces enforces primary key on nonce.
        let nonce_active = ci_callback_nonces::ActiveModel {
            nonce: Set("nonce-clb10-1".to_owned()),
            cl_link: Set("CLMIG001".to_owned()),
            body_sha256: Set("0".repeat(64)),
            expires_at: Set(now),
        };
        nonce_active.insert(&db).await.unwrap();
        let dup_nonce = ci_callback_nonces::ActiveModel {
            nonce: Set("nonce-clb10-1".to_owned()),
            cl_link: Set("CLMIG001".to_owned()),
            body_sha256: Set("1".repeat(64)),
            expires_at: Set(now),
        };
        assert!(
            dup_nonce.insert(&db).await.is_err(),
            "duplicate ci_callback_nonces.nonce must fail PK constraint"
        );

        // 4. cl_webhook_outbox enforces event_id uniqueness, state check, and (state, next_retry_at) index.
        let outbox_active = cl_webhook_outbox::ActiveModel {
            id: Set(2001),
            event_id: Set("evt-clb10-1".to_owned()),
            event_type: Set("cl.created".to_owned()),
            cl_link: Set("CLMIG001".to_owned()),
            cl_path: Set("/project".to_owned()),
            head_revision: Set(1),
            payload_json: Set("{\"event\":\"cl.created\"}".to_owned()),
            state: Set("pending".to_owned()),
            lease_owner: Set(None),
            lease_epoch: Set(0),
            lease_expires_at: Set(None),
            attempt_count: Set(0),
            next_retry_at: Set(now),
            last_error: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        };
        outbox_active.insert(&db).await.unwrap();
        let loaded_outbox = cl_webhook_outbox::Entity::find_by_id(2001)
            .one(&db)
            .await
            .unwrap()
            .expect("inserted cl_webhook_outbox row");
        assert_eq!(loaded_outbox.lease_epoch, 0);
        assert_eq!(loaded_outbox.event_id, "evt-clb10-1");

        let dup_event = cl_webhook_outbox::ActiveModel {
            id: Set(2002),
            event_id: Set("evt-clb10-1".to_owned()),
            event_type: Set("cl.created".to_owned()),
            cl_link: Set("CLMIG001".to_owned()),
            cl_path: Set("/project".to_owned()),
            head_revision: Set(1),
            payload_json: Set("{}".to_owned()),
            state: Set("pending".to_owned()),
            lease_owner: Set(None),
            lease_epoch: Set(0),
            lease_expires_at: Set(None),
            attempt_count: Set(0),
            next_retry_at: Set(now),
            last_error: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
        };
        assert!(
            dup_event.insert(&db).await.is_err(),
            "duplicate cl_webhook_outbox.event_id must fail UNIQUE constraint"
        );

        let idx_row = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT indexname FROM pg_indexes
                 WHERE schemaname = current_schema()
                   AND tablename = 'cl_webhook_outbox'
                   AND indexname = 'idx_cl_webhook_outbox_state_next_retry'",
            ))
            .await
            .unwrap();
        assert!(
            idx_row.is_some(),
            "idx_cl_webhook_outbox_state_next_retry index must exist"
        );

        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn upgrade_from_prior_schema_preserves_existing_rows_with_db_defaults() {
        use sea_orm_migration::MigratorTrait;

        let root = tempfile::tempdir().unwrap();
        let db = test_db_connection(root.path()).await;

        // Apply every historical migration strictly prior to this migration via Migrator::up.
        let all_migrations = crate::jupiter::migration::Migrator::migrations();
        let prior_count = all_migrations.len() - 1;
        assert_eq!(
            all_migrations[prior_count].name(),
            "m20261124_000100_add_cl_ci_check_columns"
        );
        crate::jupiter::migration::Migrator::up(&db, Some(prior_count as u32))
            .await
            .unwrap();

        // Seed pre-upgrade mega_cl and check_result rows using only pre-migration columns.
        db.execute_unprepared(
            "INSERT INTO mega_cl (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, username, base_branch, revision)
             VALUES (3001, 'CLPRE001', 'pre-upgrade cl', 'open', '/project', '1111111111111111111111111111111111111111', '2222222222222222222222222222222222222222', now(), now(), 'bob', 'main', 9)",
        )
        .await
        .unwrap();
        db.execute_unprepared(
            "INSERT INTO check_result (id, created_at, updated_at, path, cl_link, commit_id, check_type_code, status, message)
             VALUES (4001, now(), now(), '/project', 'CLPRE001', '2222222222222222222222222222222222222222', 'ci_status', 'PASSED', 'legacy ok')",
        )
        .await
        .unwrap();

        // Now apply the pending forward-only migration on top of pre-existing data.
        crate::jupiter::migration::Migrator::up(&db, None)
            .await
            .unwrap();

        let cl_row = mega_cl::Entity::find_by_id(3001)
            .one(&db)
            .await
            .unwrap()
            .expect("pre-existing mega_cl row must survive upgrade");
        assert_eq!(cl_row.revision, 9, "TP-07 revision must remain untouched");
        assert_eq!(
            cl_row.head_revision, 1,
            "existing mega_cl row must receive DB DEFAULT 1 for head_revision"
        );

        let check_row = check_result::Entity::find_by_id(4001)
            .one(&db)
            .await
            .unwrap()
            .expect("pre-existing check_result row must survive upgrade");
        assert_eq!(
            check_row.head_revision, 0,
            "existing check_result row must receive DB DEFAULT 0 for head_revision"
        );
        assert_eq!(
            check_row.ci_generation, 0,
            "existing check_result row must receive DB DEFAULT 0 for ci_generation"
        );
        assert_eq!(check_row.ci_task_id, None);
        assert_eq!(check_row.ci_attempt_id, None);
        assert_eq!(check_row.details_url, None);

        db.close().await.unwrap();
    }
}
