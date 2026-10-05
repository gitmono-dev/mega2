//! Forward-only preparation deadlines; historical rows retain unknown deadlines.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared(
            "ALTER TABLE mst2_metadata_prepare
               ADD COLUMN deadline_managed boolean NOT NULL DEFAULT false,
               DROP CONSTRAINT mst2_metadata_prepare_state_check,
               DROP CONSTRAINT mst2_metadata_prepare_commit_check,
               ADD CONSTRAINT mst2_metadata_prepare_state_check CHECK (state IN ('PREPARING','COMMITTED','CONSUMED','EXPIRED')),
               ADD CONSTRAINT mst2_metadata_prepare_commit_check CHECK (
                 (state='PREPARING' AND committed_at IS NULL) OR
                 (state IN ('COMMITTED','CONSUMED') AND committed_at IS NOT NULL) OR
                 (state='EXPIRED' AND deadline_managed));
             CREATE TABLE mst2_metadata_prepare_deadline (
               prepare_id text PRIMARY KEY REFERENCES mst2_metadata_prepare(prepare_id),
               storage_uuid text NOT NULL REFERENCES mst2_metadata_storage_scope(storage_uuid),
               operation_id text NOT NULL UNIQUE CHECK (octet_length(operation_id) BETWEEN 1 AND 255),
               manifest_digest bytea NOT NULL CHECK (octet_length(manifest_digest)=32),
               request_digest bytea NOT NULL CHECK (octet_length(request_digest)=32),
               grant_digest bytea NOT NULL CHECK (octet_length(grant_digest)=32),
               duration_ms bigint NOT NULL CHECK (duration_ms BETWEEN 1 AND 3600000),
               granted_at_ms bigint NOT NULL CHECK (granted_at_ms>0),
               expires_at_ms bigint NOT NULL CHECK (expires_at_ms=granted_at_ms+duration_ms)
             );
             CREATE INDEX idx_mst2_metadata_prepare_deadline_expiry ON mst2_metadata_prepare_deadline(expires_at_ms);
             CREATE TABLE mst2_metadata_prepare_expiry (
               prepare_id text PRIMARY KEY REFERENCES mst2_metadata_prepare_deadline(prepare_id),
               operation_id text NOT NULL UNIQUE CHECK (octet_length(operation_id) BETWEEN 1 AND 255),
               operation_digest bytea NOT NULL CHECK (octet_length(operation_digest)=32),
               grant_digest bytea NOT NULL CHECK (octet_length(grant_digest)=32),
               previous_state text NOT NULL CHECK (previous_state IN ('PREPARING','COMMITTED')),
               pin_root text NOT NULL CHECK (pin_root='prepare:' || prepare_id),
               expires_at_ms bigint NOT NULL CHECK (expires_at_ms>0),
               expired_at_ms bigint NOT NULL CHECK (expired_at_ms>=expires_at_ms)
             );
             CREATE TRIGGER mst2_metadata_prepare_deadline_immutable BEFORE UPDATE OR DELETE
               ON mst2_metadata_prepare_deadline FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_immutable();
             CREATE TRIGGER mst2_metadata_prepare_expiry_immutable BEFORE UPDATE OR DELETE
               ON mst2_metadata_prepare_expiry FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_immutable();
             CREATE FUNCTION mst2_metadata_prepare_deadline_guard() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NEW.deadline_managed IS DISTINCT FROM OLD.deadline_managed THEN
                 RAISE EXCEPTION 'MST2 preparation deadline management cannot be changed';
               END IF;
               IF OLD.state='EXPIRED' AND NEW.state<>'EXPIRED' THEN
                 RAISE EXCEPTION 'Expired MST2 preparations cannot be revived';
               END IF;
               IF NEW.state='EXPIRED' AND
                  (OLD.state NOT IN ('PREPARING','COMMITTED','EXPIRED') OR
                   NEW.committed_at IS DISTINCT FROM OLD.committed_at) THEN
                 RAISE EXCEPTION 'MST2 preparation expiry must preserve its unconsumed history';
               END IF;
               RETURN NEW;
             END $$;
             CREATE TRIGGER mst2_metadata_prepare_deadline_guard BEFORE UPDATE ON mst2_metadata_prepare
               FOR EACH ROW EXECUTE FUNCTION mst2_metadata_prepare_deadline_guard();
             CREATE FUNCTION mst2_metadata_prepare_deadline_check() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NOT EXISTS (
                 SELECT 1 FROM mst2_metadata_prepare_deadline d JOIN mst2_metadata_storage_scope s ON s.singleton=1
                 WHERE d.prepare_id=NEW.prepare_id AND d.operation_id=NEW.operation_id AND
                       d.manifest_digest=NEW.manifest_digest AND d.storage_uuid=s.storage_uuid
               ) THEN
                 RAISE EXCEPTION 'Managed MST2 preparations require their fixed deadline grant in the same transaction';
               END IF;
               RETURN NEW;
             END $$;
             CREATE CONSTRAINT TRIGGER mst2_metadata_prepare_deadline_check AFTER INSERT OR UPDATE ON mst2_metadata_prepare
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.deadline_managed)
               EXECUTE FUNCTION mst2_metadata_prepare_deadline_check();
             CREATE FUNCTION mst2_metadata_prepare_grant_check() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NOT EXISTS (
                 SELECT 1 FROM mst2_metadata_prepare p JOIN mst2_metadata_storage_scope s ON s.singleton=1
                 WHERE p.prepare_id=NEW.prepare_id AND p.deadline_managed AND p.operation_id=NEW.operation_id AND
                       p.manifest_digest=NEW.manifest_digest AND s.storage_uuid=NEW.storage_uuid
               ) THEN
                 RAISE EXCEPTION 'MST2 preparation grants require their fixed managed preparation';
               END IF;
               RETURN NEW;
             END $$;
             CREATE CONSTRAINT TRIGGER mst2_metadata_prepare_grant_check AFTER INSERT ON mst2_metadata_prepare_deadline
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_prepare_grant_check();
             CREATE FUNCTION mst2_metadata_prepare_expiry_check() RETURNS trigger LANGUAGE plpgsql AS $$
             DECLARE prior text;
             BEGIN
               IF TG_OP='INSERT' THEN
                 RAISE EXCEPTION 'MST2 preparations cannot start in EXPIRED state';
               END IF;
               SELECT previous_state INTO prior FROM mst2_metadata_prepare_expiry WHERE prepare_id=NEW.prepare_id;
               IF prior IS NULL OR (OLD.state<>'EXPIRED' AND prior<>OLD.state) OR
                  ((prior='COMMITTED')<>(NEW.committed_at IS NOT NULL)) OR
                  EXISTS (SELECT 1 FROM mst2_metadata_prepare_consumption WHERE prepare_id=NEW.prepare_id) OR
                  EXISTS (SELECT 1 FROM mst2_retention_root WHERE root_key='prepare:' || NEW.prepare_id) THEN
                 RAISE EXCEPTION 'MST2 preparation expiry requires its immutable event and released Prepare pins';
               END IF;
               RETURN NEW;
             END $$;
             CREATE CONSTRAINT TRIGGER mst2_metadata_prepare_expiry_check AFTER INSERT OR UPDATE ON mst2_metadata_prepare
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.state='EXPIRED')
               EXECUTE FUNCTION mst2_metadata_prepare_expiry_check();
             CREATE FUNCTION mst2_metadata_prepare_expiry_event_check() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN
               IF NOT EXISTS (
                 SELECT 1 FROM mst2_metadata_prepare p JOIN mst2_metadata_prepare_deadline d ON d.prepare_id=p.prepare_id
                 WHERE p.prepare_id=NEW.prepare_id AND p.state='EXPIRED' AND p.deadline_managed AND
                       d.grant_digest=NEW.grant_digest AND d.expires_at_ms=NEW.expires_at_ms AND
                       ((NEW.previous_state='COMMITTED')=(p.committed_at IS NOT NULL))
               ) OR EXISTS (SELECT 1 FROM mst2_metadata_prepare_consumption WHERE prepare_id=NEW.prepare_id) OR
                 EXISTS (SELECT 1 FROM mst2_retention_root WHERE root_key=NEW.pin_root) THEN
                 RAISE EXCEPTION 'MST2 preparation expiry events require their matching terminal grant and released pins';
               END IF;
               RETURN NEW;
             END $$;
             CREATE CONSTRAINT TRIGGER mst2_metadata_prepare_expiry_event_check AFTER INSERT ON mst2_metadata_prepare_expiry
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_prepare_expiry_event_check();"
        ).await.map(|_| ())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
