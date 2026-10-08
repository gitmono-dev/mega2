use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "CREATE TABLE mst2_snapshot_context (
               snapshot_id text PRIMARY KEY,
               canonical_descriptor bytea NOT NULL,
               instance_id text NOT NULL,
               commit_oid text NOT NULL,
               root_tree_oid text NOT NULL,
               metadata_root bytea NOT NULL CHECK (octet_length(metadata_root)=32),
               prepare_id text NOT NULL UNIQUE REFERENCES mst2_metadata_prepare(prepare_id),
               publication_sequence bigint NOT NULL CHECK (publication_sequence>=0),
               writer_epoch bigint NOT NULL CHECK (writer_epoch>0),
               certificate_receipt_id bigint REFERENCES mst2_native_publication(receipt_id),
               authorization_epoch bigint NOT NULL DEFAULT 1 CHECK (authorization_epoch>0),
               state text NOT NULL CHECK (state IN ('READY','DISABLED')),
               created_at timestamptz NOT NULL DEFAULT clock_timestamp()
             );
             CREATE TABLE mst2_snapshot_lease (
               lease_id text PRIMARY KEY,
               snapshot_id text NOT NULL REFERENCES mst2_snapshot_context(snapshot_id),
               authorization_epoch bigint NOT NULL CHECK (authorization_epoch>0),
               publication_sequence bigint NOT NULL CHECK (publication_sequence>0),
               writer_epoch bigint NOT NULL CHECK (writer_epoch>0),
               certificate_receipt_id bigint NOT NULL REFERENCES mst2_native_publication(receipt_id),
               expires_at_unix bigint NOT NULL CHECK (expires_at_unix>=0),
               state text NOT NULL CHECK (state IN ('ACTIVE','RELEASED','EXPIRED')),
               created_at timestamptz NOT NULL DEFAULT clock_timestamp()
             );
             CREATE FUNCTION mst2_snapshot_lease_identity_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NEW.lease_id<>OLD.lease_id OR NEW.snapshot_id<>OLD.snapshot_id
                 OR NEW.authorization_epoch<>OLD.authorization_epoch OR NEW.publication_sequence<>OLD.publication_sequence
                 OR NEW.writer_epoch<>OLD.writer_epoch OR NEW.certificate_receipt_id<>OLD.certificate_receipt_id
                 OR (OLD.state<>'ACTIVE' AND NEW.state<>OLD.state) THEN
                 RAISE EXCEPTION 'MST2 lease source cannot change or be revived';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER mst2_snapshot_lease_identity_immutable BEFORE UPDATE ON mst2_snapshot_lease
               FOR EACH ROW EXECUTE FUNCTION mst2_snapshot_lease_identity_immutable();
             CREATE INDEX mst2_snapshot_lease_active ON mst2_snapshot_lease(snapshot_id,expires_at_unix)
               WHERE state='ACTIVE';
             CREATE INDEX mst2_snapshot_lease_expiry ON mst2_snapshot_lease(expires_at_unix)
               WHERE state='ACTIVE';
             CREATE FUNCTION mst2_snapshot_identity_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NEW.snapshot_id<>OLD.snapshot_id OR NEW.canonical_descriptor<>OLD.canonical_descriptor
                 OR NEW.instance_id<>OLD.instance_id OR NEW.commit_oid<>OLD.commit_oid
                 OR NEW.root_tree_oid<>OLD.root_tree_oid OR NEW.metadata_root<>OLD.metadata_root
                 OR NEW.prepare_id<>OLD.prepare_id OR NEW.publication_sequence<>OLD.publication_sequence
                 OR NEW.writer_epoch<>OLD.writer_epoch
                 OR NEW.certificate_receipt_id IS DISTINCT FROM OLD.certificate_receipt_id THEN
                 RAISE EXCEPTION 'MST2 fixed snapshot identity cannot be changed';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER mst2_snapshot_identity_immutable BEFORE UPDATE ON mst2_snapshot_context
               FOR EACH ROW EXECUTE FUNCTION mst2_snapshot_identity_immutable();"
        ).await.map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
