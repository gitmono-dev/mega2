//! Stable native source identity; no serving or retention authority.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "CREATE TABLE mst2_native_source_identity (
               singleton integer PRIMARY KEY CHECK (singleton=1),
               source_id uuid NOT NULL UNIQUE CHECK (source_id <> '00000000-0000-0000-0000-000000000000'::uuid),
               scope_path text NOT NULL CHECK (scope_path='/'),
               created_at timestamptz NOT NULL DEFAULT clock_timestamp()
             );
             CREATE FUNCTION mst2_native_source_identity_immutable() RETURNS trigger AS $$
             BEGIN RAISE EXCEPTION 'native source identity is immutable'; END;
             $$ LANGUAGE plpgsql;
             CREATE TRIGGER mst2_native_source_identity_immutable
               BEFORE UPDATE OR DELETE ON mst2_native_source_identity
               FOR EACH ROW EXECUTE FUNCTION mst2_native_source_identity_immutable();
             CREATE TRIGGER mst2_native_source_identity_no_truncate
               BEFORE TRUNCATE ON mst2_native_source_identity
               FOR EACH STATEMENT EXECUTE FUNCTION mst2_native_source_identity_immutable();"
        ).await.map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
