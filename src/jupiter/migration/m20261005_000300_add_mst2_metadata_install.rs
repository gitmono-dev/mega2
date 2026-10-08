//! Additive, forward-only native metadata installation records.

use mst2_codec::metapage::{HEADER_LEN, PAGE_MAX_BYTES};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(&format!(
            "CREATE TABLE mst2_metadata_storage_scope (
               singleton smallint PRIMARY KEY CHECK (singleton = 1),
               storage_uuid text NOT NULL UNIQUE
             );
             CREATE TABLE mst2_metadata_payload (
               page_id bytea PRIMARY KEY CHECK (octet_length(page_id) = 32),
               metadata_codec smallint NOT NULL CHECK (metadata_codec = 1),
               byte_size integer NOT NULL CHECK (byte_size BETWEEN {HEADER_LEN} AND {PAGE_MAX_BYTES}),
               payload bytea NOT NULL CHECK (octet_length(payload) = byte_size),
               created_at timestamptz NOT NULL DEFAULT now()
             );
             CREATE OR REPLACE FUNCTION mst2_metadata_payload_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN RAISE EXCEPTION 'MST2 metadata payloads cannot be updated or deleted'; END $$;
             CREATE TRIGGER mst2_metadata_payload_immutable BEFORE UPDATE OR DELETE
               ON mst2_metadata_payload FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_immutable();
             CREATE TRIGGER mst2_metadata_scope_immutable BEFORE UPDATE OR DELETE
               ON mst2_metadata_storage_scope FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_immutable();
             CREATE TABLE mst2_metadata_prepare (
               prepare_id text PRIMARY KEY CHECK (prepare_id ~ '^[0-9a-f]{{8}}-[0-9a-f]{{4}}-4[0-9a-f]{{3}}-[89ab][0-9a-f]{{3}}-[0-9a-f]{{12}}$'),
               operation_id text NOT NULL UNIQUE CHECK (octet_length(operation_id) BETWEEN 1 AND 255),
               manifest_digest bytea NOT NULL CHECK (octet_length(manifest_digest) = 32),
               canonical_plan bytea NOT NULL CHECK (octet_length(canonical_plan) <= 2097152),
               source_domain text NOT NULL CHECK (source_domain = 'native-git'),
               tagged_root_tree_oid text NOT NULL,
               scope text NOT NULL CHECK (octet_length(scope) <= 4096),
               schema_version smallint NOT NULL,
               metadata_codec smallint NOT NULL CHECK (metadata_codec = 1),
               materialization_policy smallint NOT NULL,
               fs_semantics smallint NOT NULL,
               access_projection smallint NOT NULL,
               verification_revision integer NOT NULL,
               projection_revision smallint NOT NULL,
               metadata_root bytea NOT NULL CHECK (octet_length(metadata_root) = 32),
               node_count integer NOT NULL CHECK (node_count BETWEEN 1 AND 4096),
               edge_count integer NOT NULL CHECK (edge_count BETWEEN 0 AND 16384),
               total_bytes bigint NOT NULL CHECK (total_bytes BETWEEN 0 AND 67108864),
               state text NOT NULL CHECK (state IN ('PREPARING', 'COMMITTED')),
               created_at timestamptz NOT NULL DEFAULT now(),
               committed_at timestamptz,
               CHECK ((state = 'COMMITTED') = (committed_at IS NOT NULL))
             );
             CREATE TABLE mst2_metadata_prepare_page (
               prepare_id text NOT NULL REFERENCES mst2_metadata_prepare(prepare_id),
               page_id bytea NOT NULL CHECK (octet_length(page_id) = 32),
               expected_size integer NOT NULL CHECK (expected_size BETWEEN {HEADER_LEN} AND {PAGE_MAX_BYTES}),
               PRIMARY KEY (prepare_id, page_id)
             )"
        )).await?;
        manager
            .get_connection()
            .execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "INSERT INTO mst2_metadata_storage_scope(singleton,storage_uuid) VALUES(1,$1)",
                [uuid::Uuid::new_v4().to_string().into()],
            ))
            .await
            .map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Preserve immutable payloads, prepare pins and recovery evidence.
        Ok(())
    }
}
