//! FU-14: `import_repo_cleanups`, the ImportRepo cleanup ledger
//! (plan-20260923 ADR-FU-09 item 6).
//!
//! One row per detach; `id` is the detach `push_queue` row id (`cleanup_id`).
//! `state` only moves `detached` → `swept`. The `(path, state, id)` index
//! serves the resume query `WHERE path = $1 AND state = 'detached' ORDER BY
//! id ASC LIMIT 16` in index order, with no sort step.
//!
//! Forward-only in practice: there is no runtime `migrate down` entry point.
//! `down` drops the table only while it is empty, so a refresh cannot lose a
//! pending cleanup.

use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        conn.execute_unprepared(
            "CREATE TABLE IF NOT EXISTS import_repo_cleanups (
                id bigint NOT NULL,
                path text NOT NULL,
                repo_id bigint NOT NULL,
                state text NOT NULL,
                requester text NOT NULL,
                rows_deleted jsonb,
                created_at timestamptz NOT NULL DEFAULT now(),
                swept_at timestamptz,
                PRIMARY KEY (id),
                CONSTRAINT chk_import_repo_cleanups_state CHECK (state IN ('detached', 'swept'))
            )",
        )
        .await?;

        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_import_repo_cleanups_path_state_id \
             ON import_repo_cleanups (path, state, id)",
        )
        .await
        .map(|_| ())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // One statement, so the lock, the emptiness check and the drop share a
        // transaction: a detach insert in flight either commits first (and is
        // seen) or waits for the lock and fails on the missing table.
        manager
            .get_connection()
            .execute_unprepared(
                "DO $$ BEGIN
                    IF to_regclass('import_repo_cleanups') IS NULL THEN
                        RETURN;
                    END IF;
                    LOCK TABLE import_repo_cleanups IN ACCESS EXCLUSIVE MODE;
                    IF EXISTS (SELECT 1 FROM import_repo_cleanups) THEN
                        RAISE EXCEPTION 'import_repo_cleanups has ledger rows; \
                            dropping it would lose pending cleanups';
                    END IF;
                    DROP TABLE import_repo_cleanups;
                END $$",
            )
            .await
            .map(|_| ())
    }
}
