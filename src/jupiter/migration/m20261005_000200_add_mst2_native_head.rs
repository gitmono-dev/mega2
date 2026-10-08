use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute_unprepared(
                r#"
            ALTER TABLE mst2_publication
              ADD COLUMN IF NOT EXISTS native_certificate_version integer;
            ALTER TABLE push_queue
              ADD COLUMN IF NOT EXISTS expected_native_sequence bigint,
              ADD COLUMN IF NOT EXISTS expected_native_epoch bigint,
              ADD COLUMN IF NOT EXISTS expected_native_certificate bigint;
            CREATE TABLE IF NOT EXISTS mst2_native_publication (
              receipt_id bigint PRIMARY KEY REFERENCES mst2_publication(id),
              namespace text NOT NULL CHECK (namespace = '/'),
              instance_id text NOT NULL,
              sequence bigint NOT NULL CHECK (sequence > 0),
              writer_epoch bigint NOT NULL CHECK (writer_epoch > 0),
              old_root_commit text NOT NULL,
              old_root_tree text NOT NULL,
              root_commit text NOT NULL,
              root_tree text NOT NULL,
              origin_path text NOT NULL,
              origin_ref text NOT NULL,
              old_path_commit text,
              old_path_tree text,
              path_commit text NOT NULL,
              path_tree text NOT NULL,
              CHECK ((old_path_commit IS NULL) = (old_path_tree IS NULL)),
              CHECK (old_path_commit IS DISTINCT FROM path_commit),
              UNIQUE (namespace, sequence)
            );
            CREATE TABLE IF NOT EXISTS mst2_native_head (
              namespace text PRIMARY KEY CHECK (namespace = '/'),
              instance_id text NOT NULL,
              sequence bigint NOT NULL CHECK (sequence >= 0),
              writer_epoch bigint NOT NULL CHECK (writer_epoch > 0),
              root_commit text NOT NULL,
              root_tree text NOT NULL,
              state text NOT NULL CHECK (state IN ('INITIALIZING', 'READY')),
              certificate_receipt_id bigint REFERENCES mst2_native_publication(receipt_id),
              CHECK ((state = 'READY') = (certificate_receipt_id IS NOT NULL))
            );
            DO $$ BEGIN
              ALTER TABLE mst2_publication ADD CONSTRAINT mst2_native_certificate_version_valid
                CHECK (native_certificate_version IS NULL OR native_certificate_version > 0);
            EXCEPTION WHEN duplicate_object THEN NULL; END $$;
            DO $$ BEGIN
              ALTER TABLE push_queue ADD CONSTRAINT mst2_expected_native_token_valid CHECK (
                (expected_native_sequence IS NULL AND expected_native_epoch IS NULL
                 AND expected_native_certificate IS NULL) OR
                (expected_native_sequence IS NOT NULL AND expected_native_sequence >= 0
                 AND expected_native_epoch IS NOT NULL AND expected_native_epoch > 0)
              );
            EXCEPTION WHEN duplicate_object THEN NULL; END $$;
        "#,
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Historical native certificates and queue fences are never erased.
        Ok(())
    }
}
