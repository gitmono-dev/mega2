//! Metadata-only context/lease evidence. No HTTP or body authority is enabled.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "ALTER TABLE mst2_metadata_prepare
               DROP CONSTRAINT mst2_metadata_prepare_state_check,
               DROP CONSTRAINT mst2_metadata_prepare_check,
               ADD CONSTRAINT mst2_metadata_prepare_state_check CHECK (state IN ('PREPARING','COMMITTED','CONSUMED')),
               ADD CONSTRAINT mst2_metadata_prepare_commit_check CHECK ((state <> 'PREPARING') = (committed_at IS NOT NULL));
             CREATE TABLE mst2_metadata_catalog (
               snapshot_id bytea PRIMARY KEY CHECK (octet_length(snapshot_id)=32),
               canonical_descriptor bytea NOT NULL CHECK (octet_length(canonical_descriptor) BETWEEN 98 AND 4194),
               instance_uuid text NOT NULL,
               namespace_view_id bytea NOT NULL CHECK (octet_length(namespace_view_id)=32),
               tagged_root_commit_oid text NOT NULL,
               tagged_root_tree_oid text NOT NULL,
               scope text NOT NULL,
               metadata_root bytea NOT NULL CHECK (octet_length(metadata_root)=32),
               plan_digest bytea NOT NULL CHECK (octet_length(plan_digest)=32),
               prepare_id text NOT NULL REFERENCES mst2_metadata_prepare(prepare_id),
               created_at timestamptz NOT NULL DEFAULT clock_timestamp()
             );
             CREATE TABLE mst2_metadata_access_generation (
               subject_id bytea PRIMARY KEY CHECK (octet_length(subject_id)=32),
               generation bigint NOT NULL CHECK (generation>0),
               scope text NOT NULL,
               enabled boolean NOT NULL DEFAULT false
             );
             CREATE TABLE mst2_metadata_lease (
               lease_id text PRIMARY KEY,
               snapshot_id bytea NOT NULL REFERENCES mst2_metadata_catalog(snapshot_id),
               subject_id bytea NOT NULL REFERENCES mst2_metadata_access_generation(subject_id),
               policy_generation bigint NOT NULL CHECK (policy_generation>0),
               publication_binding bytea NOT NULL CHECK (octet_length(publication_binding)=32),
               state text NOT NULL CHECK (state IN ('ACTIVE','RELEASED','EXPIRED')),
               expires_at_ms bigint NOT NULL CHECK (expires_at_ms>0),
               version bigint NOT NULL CHECK (version>0),
               created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
               terminal_at timestamptz,
               CHECK ((state='ACTIVE') = (terminal_at IS NULL))
             );
             CREATE TABLE mst2_metadata_prepare_consumption (
               prepare_id text PRIMARY KEY REFERENCES mst2_metadata_prepare(prepare_id),
               operation_id text NOT NULL UNIQUE CHECK (octet_length(operation_id) BETWEEN 1 AND 255),
               operation_digest bytea NOT NULL CHECK (octet_length(operation_digest)=32),
               install_digest bytea NOT NULL CHECK (octet_length(install_digest)=32),
               snapshot_id bytea NOT NULL REFERENCES mst2_metadata_catalog(snapshot_id),
               lease_id text NOT NULL REFERENCES mst2_metadata_lease(lease_id),
               consumed_at timestamptz NOT NULL DEFAULT clock_timestamp()
             );
             CREATE TABLE mst2_metadata_lease_operation (
               operation_id text PRIMARY KEY CHECK (octet_length(operation_id) BETWEEN 1 AND 255),
               operation_digest bytea NOT NULL CHECK (octet_length(operation_digest)=32),
               phase text NOT NULL CHECK (phase IN ('CREATE','RENEW','RELEASE','READ_BEGIN','READ_END','EXPIRE')),
               lease_id text NOT NULL REFERENCES mst2_metadata_lease(lease_id),
               snapshot_id bytea NOT NULL REFERENCES mst2_metadata_catalog(snapshot_id),
               receipt jsonb NOT NULL CHECK (octet_length(receipt::text)<=32768),
               created_at timestamptz NOT NULL DEFAULT clock_timestamp()
             );
             CREATE TABLE mst2_metadata_read_pin (
               read_id text PRIMARY KEY,
               lease_id text NOT NULL REFERENCES mst2_metadata_lease(lease_id),
               snapshot_id bytea NOT NULL REFERENCES mst2_metadata_catalog(snapshot_id),
               subject_id bytea NOT NULL REFERENCES mst2_metadata_access_generation(subject_id),
               policy_generation bigint NOT NULL CHECK (policy_generation>0),
               state text NOT NULL CHECK (state IN ('ACTIVE','RELEASED','EXPIRED')),
               expires_at_ms bigint NOT NULL CHECK (expires_at_ms>0),
               terminal_at timestamptz,
               CHECK ((state='ACTIVE') = (terminal_at IS NULL))
             );
             CREATE INDEX idx_mst2_metadata_lease_expiry ON mst2_metadata_lease(expires_at_ms) WHERE state='ACTIVE';
             CREATE INDEX idx_mst2_metadata_read_expiry ON mst2_metadata_read_pin(expires_at_ms) WHERE state='ACTIVE';
             CREATE TRIGGER mst2_metadata_catalog_immutable BEFORE UPDATE OR DELETE ON mst2_metadata_catalog
               FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_immutable();
             CREATE TRIGGER mst2_metadata_consumption_immutable BEFORE UPDATE OR DELETE ON mst2_metadata_prepare_consumption
               FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_immutable();
             CREATE TRIGGER mst2_metadata_lease_operation_immutable BEFORE UPDATE OR DELETE ON mst2_metadata_lease_operation
               FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_immutable();"
        ).await.map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
