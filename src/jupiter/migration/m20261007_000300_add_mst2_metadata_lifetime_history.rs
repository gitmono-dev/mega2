//! Preserve historical incarnations and explicit preparation terminal states.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "ALTER TABLE mst2_metadata_lifetime
               DROP CONSTRAINT mst2_metadata_lifetime_pkey,
               DROP CONSTRAINT mst2_metadata_lifetime_node_id_key,
               ADD PRIMARY KEY(page_id,generation);
             CREATE TABLE mst2_metadata_current (
               page_id bytea PRIMARY KEY CHECK (octet_length(page_id)=32),
               generation bigint NOT NULL CHECK (generation>0),
               FOREIGN KEY(page_id,generation) REFERENCES mst2_metadata_lifetime(page_id,generation)
             );
             INSERT INTO mst2_metadata_current(page_id,generation)
               SELECT page_id,generation FROM mst2_metadata_lifetime;
             CREATE FUNCTION mst2_metadata_current_guard() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN RAISE EXCEPTION 'metadata current watermark requires generation-fenced collection'; END $$;
             CREATE TRIGGER mst2_metadata_current_guard BEFORE UPDATE OR DELETE
               ON mst2_metadata_current FOR EACH ROW EXECUTE FUNCTION mst2_metadata_current_guard();
             ALTER TABLE mst2_metadata_prepare
               ADD COLUMN graph_domain text CHECK (graph_domain IN ('generic-v1','qualified-v1')),
               ADD COLUMN aborted_at timestamptz,
               ADD COLUMN coverage_retired_at timestamptz,
               DROP CONSTRAINT mst2_metadata_prepare_state_check,
               DROP CONSTRAINT mst2_metadata_prepare_check,
               ADD CONSTRAINT mst2_metadata_prepare_state_check CHECK (state IN ('PREPARING','COMMITTED','ABORTED')),
               ADD CONSTRAINT mst2_metadata_prepare_terminal_check CHECK (
                 (state='COMMITTED')=(committed_at IS NOT NULL)
                 AND (state='ABORTED')=(aborted_at IS NOT NULL)
                 AND (coverage_retired_at IS NULL OR state='COMMITTED')
                 AND (state<>'ABORTED' OR storage_seal IS NOT NULL)
                 AND (graph_domain IS NULL OR storage_seal IS NOT NULL)
               );
             CREATE OR REPLACE FUNCTION mst2_metadata_generation_seal_guard() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NEW.canonical_bindings IS DISTINCT FROM OLD.canonical_bindings
                 OR NEW.bindings_digest IS DISTINCT FROM OLD.bindings_digest
                 OR NEW.primary_scope IS DISTINCT FROM OLD.primary_scope
                 OR NEW.storage_seal IS DISTINCT FROM OLD.storage_seal
                 OR NEW.graph_domain IS DISTINCT FROM OLD.graph_domain THEN
                 RAISE EXCEPTION 'metadata generation seal cannot be rebound';
               END IF;
               IF OLD.state IN ('COMMITTED','ABORTED') AND NEW.state IS DISTINCT FROM OLD.state THEN
                 RAISE EXCEPTION 'metadata preparation terminal state cannot be revived';
               END IF;
               IF OLD.committed_at IS NOT NULL AND NEW.committed_at IS DISTINCT FROM OLD.committed_at
                 OR OLD.aborted_at IS NOT NULL AND NEW.aborted_at IS DISTINCT FROM OLD.aborted_at
                 OR OLD.coverage_retired_at IS NOT NULL AND NEW.coverage_retired_at IS DISTINCT FROM OLD.coverage_retired_at THEN
                 RAISE EXCEPTION 'metadata preparation terminal receipt cannot be rewritten';
               END IF;
               RETURN NEW;
             END $$;
             CREATE INDEX idx_mst2_metadata_lifetime_node_generation ON mst2_metadata_lifetime(node_id,generation)"
        ).await.map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
