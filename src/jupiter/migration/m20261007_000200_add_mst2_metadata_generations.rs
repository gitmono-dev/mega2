//! Fixed metadata lifetimes. Legacy rows remain explicitly unbound.

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(&format!(
            "CREATE TABLE mst2_metadata_lifetime (
               page_id bytea PRIMARY KEY CHECK (octet_length(page_id)=32),
               node_id text NOT NULL UNIQUE CHECK (node_id='page:sha256:'||encode(page_id,'hex')),
               generation bigint NOT NULL CHECK (generation>0),
               state text NOT NULL CHECK (state IN ('RESERVED','LIVE','DELETING','REMOVED')),
               metadata_codec smallint NOT NULL CHECK (metadata_codec=1),
               expected_size integer NOT NULL CHECK (expected_size BETWEEN {HEADER_LEN} AND {PAGE_MAX_BYTES}),
               UNIQUE(page_id,generation)
             );
             CREATE INDEX idx_mst2_metadata_lifetime_state_page ON mst2_metadata_lifetime(state,page_id);
             CREATE FUNCTION mst2_metadata_lifetime_guard() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF TG_OP='DELETE' THEN RAISE EXCEPTION 'metadata lifetime watermark cannot be deleted'; END IF;
               IF NEW.page_id IS DISTINCT FROM OLD.page_id OR NEW.node_id IS DISTINCT FROM OLD.node_id
                 OR NEW.generation IS DISTINCT FROM OLD.generation
                 OR NEW.metadata_codec IS DISTINCT FROM OLD.metadata_codec
                 OR NEW.expected_size IS DISTINCT FROM OLD.expected_size THEN
                 RAISE EXCEPTION 'metadata lifetime identity is immutable';
               END IF;
               IF NEW.state IS DISTINCT FROM OLD.state AND NOT (OLD.state='RESERVED' AND NEW.state='LIVE') THEN
                 RAISE EXCEPTION 'metadata lifetime transition requires generation collector';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER mst2_metadata_lifetime_guard BEFORE UPDATE OR DELETE
               ON mst2_metadata_lifetime FOR EACH ROW EXECUTE FUNCTION mst2_metadata_lifetime_guard();
             ALTER TABLE mst2_metadata_payload ADD COLUMN generation bigint CHECK (generation>0),
               ADD CONSTRAINT mst2_metadata_payload_lifetime_fk FOREIGN KEY(page_id,generation)
                 REFERENCES mst2_metadata_lifetime(page_id,generation);
             ALTER TABLE mst2_metadata_prepare_page ADD COLUMN generation bigint CHECK (generation>0),
               ADD CONSTRAINT mst2_metadata_prepare_page_lifetime_fk FOREIGN KEY(page_id,generation)
                 REFERENCES mst2_metadata_lifetime(page_id,generation);
             CREATE INDEX idx_mst2_metadata_prepare_page_lifetime
               ON mst2_metadata_prepare_page(page_id,generation,prepare_id);
             ALTER TABLE mst2_metadata_prepare
               ADD COLUMN canonical_bindings bytea,
               ADD COLUMN bindings_digest bytea CHECK (octet_length(bindings_digest)=32),
               ADD COLUMN primary_scope bytea CHECK (octet_length(primary_scope) BETWEEN 1 AND 16384),
               ADD COLUMN storage_seal bytea CHECK (octet_length(storage_seal)=32),
               ADD CONSTRAINT mst2_metadata_prepare_generation_seal_check CHECK (
                 (canonical_bindings IS NULL AND bindings_digest IS NULL AND primary_scope IS NULL AND storage_seal IS NULL)
                 OR (canonical_bindings IS NOT NULL AND octet_length(canonical_bindings) BETWEEN 60 AND 196620
                   AND bindings_digest IS NOT NULL AND primary_scope IS NOT NULL AND storage_seal IS NOT NULL)
               );
             CREATE FUNCTION mst2_metadata_generation_seal_guard() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NEW.canonical_bindings IS DISTINCT FROM OLD.canonical_bindings
                 OR NEW.bindings_digest IS DISTINCT FROM OLD.bindings_digest
                 OR NEW.primary_scope IS DISTINCT FROM OLD.primary_scope
                 OR NEW.storage_seal IS DISTINCT FROM OLD.storage_seal THEN
                 RAISE EXCEPTION 'metadata generation seal cannot be rebound';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER mst2_metadata_generation_seal_guard BEFORE UPDATE ON mst2_metadata_prepare
               FOR EACH ROW EXECUTE FUNCTION mst2_metadata_generation_seal_guard();
             CREATE FUNCTION mst2_metadata_generation_mapping_guard() RETURNS trigger LANGUAGE plpgsql AS $$
             DECLARE fixed boolean; preparing boolean;
             BEGIN
               SELECT storage_seal IS NOT NULL,state='PREPARING' INTO fixed,preparing
                 FROM mst2_metadata_prepare WHERE prepare_id=CASE WHEN TG_OP='DELETE' THEN OLD.prepare_id ELSE NEW.prepare_id END FOR UPDATE;
               IF fixed AND (TG_OP<>'INSERT' OR NOT preparing) THEN
                 RAISE EXCEPTION 'metadata generation mappings are immutable';
               END IF;
               IF TG_OP='UPDATE' AND OLD.prepare_id IS DISTINCT FROM NEW.prepare_id AND EXISTS (
                 SELECT 1 FROM mst2_metadata_prepare WHERE prepare_id=OLD.prepare_id AND storage_seal IS NOT NULL
               ) THEN RAISE EXCEPTION 'metadata generation mapping cannot leave its prepare'; END IF;
               IF TG_OP='DELETE' THEN RETURN OLD; END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER mst2_metadata_generation_mapping_guard BEFORE INSERT OR UPDATE OR DELETE
               ON mst2_metadata_prepare_page FOR EACH ROW EXECUTE FUNCTION mst2_metadata_generation_mapping_guard()"
        )).await.map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
