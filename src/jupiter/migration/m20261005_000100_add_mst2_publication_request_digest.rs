use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"ALTER TABLE mst2_publication
                   ADD COLUMN IF NOT EXISTS request_digest text,
                   ADD COLUMN IF NOT EXISTS request_digest_version integer;
                   DO $$ BEGIN
                     ALTER TABLE mst2_publication
                       ADD CONSTRAINT mst2_publication_request_digest_valid CHECK (
                         (request_digest IS NULL AND request_digest_version IS NULL) OR
                         (request_digest IS NOT NULL AND request_digest_version IS NOT NULL
                          AND request_digest_version > 0
                          AND request_digest ~ '^sha256:[0-9a-f]{64}$')
                       );
                   EXCEPTION WHEN duplicate_object THEN NULL;
                   END $$;
                   CREATE TABLE IF NOT EXISTS mst2_queue_noop_receipt (
                     id bigserial PRIMARY KEY,
                     operation_id text NOT NULL UNIQUE,
                     namespace text NOT NULL,
                     request_digest text NOT NULL CHECK (request_digest ~ '^sha256:[0-9a-f]{64}$'),
                     request_digest_version integer NOT NULL CHECK (request_digest_version > 0),
                     writer_epoch bigint NOT NULL,
                     writer_kind text NOT NULL,
                     observed_sequence bigint NOT NULL CHECK (observed_sequence >= 0),
                     observed_root_commit text NOT NULL,
                     observed_root_tree text NOT NULL,
                     landed_commit_id text NOT NULL,
                     created_at timestamptz NOT NULL
                   );"#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Receipts remain immutable, including the absence of legacy digests.
        Ok(())
    }
}
