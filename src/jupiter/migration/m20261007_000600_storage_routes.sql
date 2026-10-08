DO $$ BEGIN
  IF pg_catalog.pg_is_in_recovery() OR pg_catalog.current_setting('transaction_isolation')<>'read committed'
    OR pg_catalog.current_schema()<>$CORE_LITERAL$
    OR NOT EXISTS(SELECT 1 FROM pg_catalog.pg_namespace WHERE oid=$CORE_OID$ AND nspname=$CORE_LITERAL$) THEN
    RAISE EXCEPTION 'storage route migration requires its captured primary core schema';
  END IF;
  IF EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE locktype='advisory' AND pid=pg_catalog.pg_backend_pid()
      AND classid=1296718001::oid AND objid=pg_catalog.hashtext($CORE_LITERAL$)::oid AND objsubid=2 AND granted)
    AND NOT EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE locktype='advisory' AND pid=pg_catalog.pg_backend_pid()
      AND classid=1297043024::oid AND objid=($MONO_KEY2$)::oid AND objsubid=2 AND granted AND mode='ExclusiveLock') THEN
    RAISE EXCEPTION 'storage route migration cannot acquire mono after route';
  END IF;
  IF EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE locktype='advisory' AND pid=pg_catalog.pg_backend_pid()
      AND database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=pg_catalog.current_database())
      AND classid=1296717362::oid AND objid=pg_catalog.hashtext($CORE_LITERAL$)::oid AND objsubid=2 AND granted)
    AND NOT (EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE locktype='advisory' AND pid=pg_catalog.pg_backend_pid()
      AND classid=1297043024::oid AND objid=($MONO_KEY2$)::oid AND objsubid=2 AND granted AND mode='ExclusiveLock')
    AND EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE locktype='advisory' AND pid=pg_catalog.pg_backend_pid()
      AND classid=1296718001::oid AND objid=pg_catalog.hashtext($CORE_LITERAL$)::oid
      AND objsubid=2 AND granted AND mode='ExclusiveLock')) THEN
    RAISE EXCEPTION 'storage route migration cannot acquire core locks after retention';
  END IF;
END $$;
SELECT pg_catalog.set_config('lock_timeout','5000ms',true);
SELECT pg_catalog.pg_advisory_xact_lock(1297043024,$MONO_KEY2$);
SELECT pg_catalog.pg_advisory_xact_lock(1296718001,pg_catalog.hashtext($CORE_LITERAL$));
SELECT pg_catalog.pg_advisory_xact_lock(1296717362,pg_catalog.hashtext($CORE_LITERAL$));
SET LOCAL search_path=$CORE_SCHEMA$,pg_catalog,pg_temp;
LOCK TABLE mst2_snapshot_context,mst2_snapshot_lease,mst2_metadata_prepare,
  mst2_metadata_storage_scope IN ACCESS EXCLUSIVE MODE;

CREATE TABLE mst2_metadata_namespace (
  singleton integer PRIMARY KEY CHECK (singleton=1),
  namespace_uuid uuid NOT NULL UNIQUE,
  core_schema text NOT NULL,
  core_schema_oid oid NOT NULL,
  database_name text NOT NULL,
  database_oid oid NOT NULL,
  storage_uuid text NOT NULL,
  server_address text,
  server_port integer,
  mono_lock_key2 integer NOT NULL,
  metadata_schema text NOT NULL,
  metadata_schema_oid oid NOT NULL,
  family_identity text NOT NULL CHECK (family_identity='v3-generic-session-1'),
  graph_domain text NOT NULL CHECK (graph_domain='generic-v1'),
  admission_state text NOT NULL CHECK (admission_state='G_ADMITTED_Q_CLOSED'),
  collector_state text NOT NULL CHECK (collector_state='CLOSED'),
  CHECK (core_schema=metadata_schema AND core_schema_oid=metadata_schema_oid)
);
INSERT INTO mst2_metadata_namespace
SELECT 1,'$NAMESPACE_UUID$'::uuid,$CORE_LITERAL$,$CORE_OID$,pg_catalog.current_database(),d.oid,s.storage_uuid,
  pg_catalog.inet_server_addr()::text,pg_catalog.inet_server_port(),$MONO_KEY2$,$CORE_LITERAL$,$CORE_OID$,
  'v3-generic-session-1','generic-v1','G_ADMITTED_Q_CLOSED','CLOSED'
FROM mst2_metadata_storage_scope s JOIN pg_catalog.pg_database d ON d.datname=pg_catalog.current_database()
WHERE s.singleton=1;

CREATE TABLE mst2_snapshot_storage_route (
  snapshot_id text PRIMARY KEY,
  namespace_uuid uuid NOT NULL REFERENCES mst2_metadata_namespace(namespace_uuid),
  canonical_descriptor bytea NOT NULL,
  instance_id text NOT NULL,
  commit_oid text NOT NULL,
  root_tree_oid text NOT NULL,
  metadata_root bytea NOT NULL CHECK (octet_length(metadata_root)=32),
  source_profile jsonb NOT NULL,
  UNIQUE(snapshot_id,namespace_uuid),
  CHECK (snapshot_id='sha256:'||pg_catalog.encode(pg_catalog.sha256(
    pg_catalog.convert_to('mega.mst2.descriptor','UTF8')||pg_catalog.decode('00','hex')||canonical_descriptor),'hex'))
);
ALTER TABLE mst2_snapshot_context ADD UNIQUE(snapshot_id,prepare_id,metadata_root);
CREATE TABLE mst2_generic_session_storage_binding (
  session_incarnation uuid PRIMARY KEY,
  snapshot_id text NOT NULL UNIQUE,
  namespace_uuid uuid NOT NULL,
  prepare_id text NOT NULL,
  metadata_root bytea NOT NULL,
  FOREIGN KEY(snapshot_id,namespace_uuid) REFERENCES mst2_snapshot_storage_route(snapshot_id,namespace_uuid),
  FOREIGN KEY(snapshot_id,prepare_id,metadata_root) REFERENCES mst2_snapshot_context(snapshot_id,prepare_id,metadata_root),
  UNIQUE(snapshot_id,namespace_uuid,session_incarnation,prepare_id,metadata_root)
);
CREATE TABLE mst2_lease_storage_route (
  lease_id text PRIMARY KEY,
  snapshot_id text NOT NULL,
  namespace_uuid uuid NOT NULL,
  session_incarnation uuid NOT NULL,
  prepare_id text NOT NULL,
  metadata_root bytea NOT NULL CHECK (octet_length(metadata_root)=32),
  authorization_epoch bigint NOT NULL,
  publication_sequence bigint NOT NULL,
  writer_epoch bigint NOT NULL,
  certificate_receipt_id bigint NOT NULL,
  FOREIGN KEY(snapshot_id,namespace_uuid) REFERENCES mst2_snapshot_storage_route(snapshot_id,namespace_uuid)
);
CREATE INDEX mst2_lease_storage_route_incarnation ON mst2_lease_storage_route(namespace_uuid,session_incarnation,lease_id);

CREATE FUNCTION mst2_route_scope_valid(caller_schema text) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT NOT pg_catalog.pg_is_in_recovery() AND pg_catalog.current_setting('transaction_isolation')='read committed'
    AND EXISTS(SELECT 1 FROM mst2_metadata_namespace n JOIN mst2_metadata_storage_scope s ON s.singleton=1
      JOIN pg_catalog.pg_database d ON d.datname=pg_catalog.current_database()
      JOIN pg_catalog.pg_namespace c ON c.oid=n.core_schema_oid AND c.nspname=n.core_schema
      WHERE n.singleton=1 AND caller_schema=n.core_schema AND n.core_schema=$CORE_LITERAL$ AND n.core_schema_oid=$CORE_OID$
        AND n.database_name=d.datname AND n.database_oid=d.oid AND n.storage_uuid=s.storage_uuid
        AND n.server_address IS NOT DISTINCT FROM pg_catalog.inet_server_addr()::text
        AND n.server_port IS NOT DISTINCT FROM pg_catalog.inet_server_port()
        AND n.metadata_schema=n.core_schema AND n.metadata_schema_oid=n.core_schema_oid
        AND n.graph_domain='generic-v1' AND n.family_identity='v3-generic-session-1'
        AND n.admission_state='G_ADMITTED_Q_CLOSED' AND n.collector_state='CLOSED')
$$;

CREATE FUNCTION mst2_route_lock_held(k1 integer,k2 integer,exclusive_only boolean DEFAULT true) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE locktype='advisory' AND pid=pg_catalog.pg_backend_pid()
    AND database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=pg_catalog.current_database())
    AND classid=k1::oid AND objid=k2::oid AND objsubid=2 AND granted AND (NOT exclusive_only OR mode='ExclusiveLock'))
$$;

CREATE FUNCTION mst2_route_enter(caller_schema text) RETURNS uuid LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE n mst2_metadata_namespace%ROWTYPE; mono_held boolean; route_held boolean;
BEGIN
  IF NOT mst2_route_scope_valid(caller_schema) THEN RAISE EXCEPTION 'storage route captured primary scope is unavailable'; END IF;
  SELECT * INTO STRICT n FROM mst2_metadata_namespace WHERE singleton=1;
  mono_held:=mst2_route_lock_held(1297043024,n.mono_lock_key2);
  route_held:=mst2_route_lock_held(1296718001,pg_catalog.hashtext(n.core_schema));
  IF mst2_route_lock_held(1296718001,pg_catalog.hashtext(n.core_schema),false) AND NOT mono_held THEN
    RAISE EXCEPTION 'storage route lock was acquired before core mono';
  END IF;
  IF EXISTS(SELECT 1 FROM mst2_metadata_namespace x
    WHERE mst2_route_lock_held(1296717362,pg_catalog.hashtext(x.metadata_schema),false))
    AND NOT (mono_held AND route_held) THEN
    RAISE EXCEPTION 'storage route cannot acquire core locks after retention';
  END IF;
  PERFORM pg_catalog.set_config('lock_timeout','5000ms',true);
  PERFORM pg_catalog.pg_advisory_xact_lock(1297043024,n.mono_lock_key2);
  PERFORM pg_catalog.pg_advisory_xact_lock(1296718001,pg_catalog.hashtext(n.core_schema));
  FOR n IN SELECT * FROM mst2_metadata_namespace ORDER BY namespace_uuid LOOP
    PERFORM pg_catalog.pg_advisory_xact_lock(1296717362,pg_catalog.hashtext(n.metadata_schema));
  END LOOP;
  RETURN n.namespace_uuid;
END $$;

-- This wrapper observes the caller before entering a fixed trusted path. It
-- uses only pg_catalog and the explicitly captured core function/relation.
CREATE FUNCTION mst2_route_statement_barrier() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF TG_TABLE_SCHEMA<>$CORE_LITERAL$ OR NOT EXISTS(SELECT 1 FROM pg_catalog.pg_class
    WHERE oid=TG_RELID AND relnamespace=$CORE_OID$) THEN
    RAISE EXCEPTION 'storage route mutation is outside its captured core schema';
  END IF;
  PERFORM $CORE_SCHEMA$.mst2_route_enter(pg_catalog.current_schema());
  RETURN NULL;
END $$;

CREATE FUNCTION mst2_route_profile(source_domain text,tagged_root_tree_oid text,scope text,schema_version smallint,
  metadata_codec smallint,materialization_policy smallint,fs_semantics smallint,access_projection smallint,
  verification_revision integer,projection_revision smallint) RETURNS jsonb LANGUAGE sql IMMUTABLE STRICT
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT pg_catalog.jsonb_build_object('source_domain',source_domain,'tagged_root_tree_oid',tagged_root_tree_oid,
    'scope',scope,'schema_version',schema_version,'metadata_codec',metadata_codec,
    'materialization_policy',materialization_policy,'fs_semantics',fs_semantics,
    'access_projection',access_projection,'verification_revision',verification_revision,
    'projection_revision',projection_revision)
$$;

CREATE FUNCTION mst2_route_snapshot_proof(sid text) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM mst2_snapshot_storage_route r JOIN mst2_snapshot_context s USING(snapshot_id)
    JOIN mst2_generic_session_storage_binding b USING(snapshot_id,namespace_uuid)
    JOIN mst2_metadata_namespace n USING(namespace_uuid)
    JOIN mst2_metadata_prepare p ON p.prepare_id=s.prepare_id
    WHERE r.snapshot_id=sid AND r.canonical_descriptor=s.canonical_descriptor
      AND r.instance_id=s.instance_id AND r.commit_oid=s.commit_oid AND r.root_tree_oid=s.root_tree_oid
      AND r.metadata_root=s.metadata_root AND r.source_profile=mst2_route_profile(p.source_domain,p.tagged_root_tree_oid,
        p.scope,p.schema_version,p.metadata_codec,p.materialization_policy,p.fs_semantics,p.access_projection,
        p.verification_revision,p.projection_revision)
      AND b.prepare_id=s.prepare_id AND b.metadata_root=s.metadata_root
      AND p.state='COMMITTED' AND p.metadata_root=s.metadata_root AND p.source_domain='native-git'
      AND p.tagged_root_tree_oid IN ('sha1:'||s.root_tree_oid,'sha256:'||s.root_tree_oid)
      AND n.graph_domain='generic-v1')
$$;

CREATE FUNCTION mst2_route_lease_proof(lid text) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM mst2_lease_storage_route r JOIN mst2_snapshot_lease l USING(lease_id)
    JOIN mst2_generic_session_storage_binding b ON b.snapshot_id=r.snapshot_id AND b.namespace_uuid=r.namespace_uuid
      AND b.session_incarnation=r.session_incarnation AND b.prepare_id=r.prepare_id AND b.metadata_root=r.metadata_root
    WHERE r.lease_id=lid AND r.snapshot_id=l.snapshot_id AND r.authorization_epoch=l.authorization_epoch
      AND r.publication_sequence=l.publication_sequence AND r.writer_epoch=l.writer_epoch
      AND r.certificate_receipt_id=l.certificate_receipt_id AND mst2_route_snapshot_proof(r.snapshot_id))
$$;

CREATE FUNCTION mst2_route_select_snapshot(sid text,caller_schema text)
RETURNS TABLE(context_present boolean,route_present boolean,valid boolean) LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF NOT mst2_route_scope_valid(caller_schema) THEN RAISE EXCEPTION 'storage route captured primary scope is unavailable'; END IF;
  RETURN QUERY SELECT EXISTS(SELECT 1 FROM mst2_snapshot_context WHERE snapshot_id=sid),
    EXISTS(SELECT 1 FROM mst2_snapshot_storage_route WHERE snapshot_id=sid),mst2_route_snapshot_proof(sid);
END $$;

CREATE FUNCTION mst2_route_select_lease(lid text,caller_schema text)
RETURNS TABLE(actual_sid text,route_present boolean,valid boolean) LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF NOT mst2_route_scope_valid(caller_schema) THEN RAISE EXCEPTION 'storage route captured primary scope is unavailable'; END IF;
  RETURN QUERY SELECT (SELECT snapshot_id FROM mst2_snapshot_lease WHERE lease_id=lid),
    EXISTS(SELECT 1 FROM mst2_lease_storage_route WHERE lease_id=lid),mst2_route_lease_proof(lid);
END $$;

CREATE FUNCTION mst2_route_immutable() RETURNS trigger LANGUAGE plpgsql
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN RAISE EXCEPTION 'storage route identity and historical ledger are immutable'; END $$;

CREATE FUNCTION mst2_route_insert_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_TABLE_NAME='mst2_snapshot_storage_route' THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_snapshot_context s JOIN mst2_metadata_prepare p ON p.prepare_id=s.prepare_id
      JOIN mst2_metadata_namespace n ON n.namespace_uuid=NEW.namespace_uuid
      WHERE s.snapshot_id=NEW.snapshot_id AND NEW.canonical_descriptor=s.canonical_descriptor
        AND NEW.instance_id=s.instance_id AND NEW.commit_oid=s.commit_oid AND NEW.root_tree_oid=s.root_tree_oid
        AND NEW.metadata_root=s.metadata_root AND NEW.source_profile=mst2_route_profile(p.source_domain,p.tagged_root_tree_oid,
          p.scope,p.schema_version,p.metadata_codec,p.materialization_policy,p.fs_semantics,p.access_projection,
          p.verification_revision,p.projection_revision)
        AND p.state='COMMITTED' AND p.metadata_root=s.metadata_root AND p.source_domain='native-git'
        AND (p.graph_domain IS NULL OR p.graph_domain='generic-v1')
        AND p.tagged_root_tree_oid IN ('sha1:'||s.root_tree_oid,'sha256:'||s.root_tree_oid)) THEN
      RAISE EXCEPTION 'storage route is not derived from its actual generic context';
    END IF;
  ELSIF TG_TABLE_NAME='mst2_generic_session_storage_binding' THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_snapshot_context s JOIN mst2_snapshot_storage_route r USING(snapshot_id)
      WHERE s.snapshot_id=NEW.snapshot_id AND r.namespace_uuid=NEW.namespace_uuid
        AND s.prepare_id=NEW.prepare_id AND s.metadata_root=NEW.metadata_root) THEN
      RAISE EXCEPTION 'storage route generic incarnation is not its actual context';
    END IF;
  ELSIF TG_TABLE_NAME='mst2_lease_storage_route' THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_snapshot_lease l JOIN mst2_generic_session_storage_binding b USING(snapshot_id)
      WHERE l.lease_id=NEW.lease_id AND l.snapshot_id=NEW.snapshot_id AND b.namespace_uuid=NEW.namespace_uuid
        AND b.session_incarnation=NEW.session_incarnation AND b.prepare_id=NEW.prepare_id AND b.metadata_root=NEW.metadata_root
        AND l.authorization_epoch=NEW.authorization_epoch AND l.publication_sequence=NEW.publication_sequence
        AND l.writer_epoch=NEW.writer_epoch AND l.certificate_receipt_id=NEW.certificate_receipt_id
        AND mst2_route_snapshot_proof(l.snapshot_id)) THEN
      RAISE EXCEPTION 'storage route lease is not its exact generic incarnation and source';
    END IF;
  ELSE RAISE EXCEPTION 'storage route insertion target is not registered'; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION mst2_route_context_insert() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  INSERT INTO mst2_snapshot_storage_route
    SELECT s.snapshot_id,n.namespace_uuid,s.canonical_descriptor,s.instance_id,s.commit_oid,s.root_tree_oid,
      s.metadata_root,mst2_route_profile(p.source_domain,p.tagged_root_tree_oid,p.scope,p.schema_version,p.metadata_codec,
        p.materialization_policy,p.fs_semantics,p.access_projection,p.verification_revision,p.projection_revision)
    FROM mst2_snapshot_context s JOIN mst2_metadata_prepare p ON p.prepare_id=s.prepare_id
    CROSS JOIN mst2_metadata_namespace n WHERE s.snapshot_id=NEW.snapshot_id AND n.singleton=1
    ON CONFLICT(snapshot_id) DO NOTHING;
  INSERT INTO mst2_generic_session_storage_binding
    SELECT pg_catalog.gen_random_uuid(),s.snapshot_id,r.namespace_uuid,s.prepare_id,s.metadata_root
    FROM mst2_snapshot_context s JOIN mst2_snapshot_storage_route r USING(snapshot_id) WHERE s.snapshot_id=NEW.snapshot_id
    ON CONFLICT(snapshot_id) DO NOTHING;
  RETURN NULL;
END $$;
CREATE FUNCTION mst2_route_lease_insert() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  INSERT INTO mst2_lease_storage_route
    SELECT l.lease_id,l.snapshot_id,b.namespace_uuid,b.session_incarnation,b.prepare_id,b.metadata_root,
      l.authorization_epoch,l.publication_sequence,l.writer_epoch,l.certificate_receipt_id
    FROM mst2_snapshot_lease l JOIN mst2_generic_session_storage_binding b USING(snapshot_id) WHERE l.lease_id=NEW.lease_id
    ON CONFLICT(lease_id) DO NOTHING;
  RETURN NULL;
END $$;
CREATE FUNCTION mst2_route_complete() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_TABLE_NAME='mst2_snapshot_context' THEN
    IF NOT mst2_route_snapshot_proof(NEW.snapshot_id) THEN RAISE EXCEPTION 'storage route context committed without exact routing'; END IF;
  ELSIF NOT mst2_route_lease_proof(NEW.lease_id) THEN RAISE EXCEPTION 'storage route lease committed without exact routing'; END IF;
  RETURN NULL;
END $$;

DO $$ DECLARE t text; BEGIN
  FOREACH t IN ARRAY ARRAY['mst2_metadata_namespace','mst2_snapshot_storage_route',
    'mst2_generic_session_storage_binding','mst2_lease_storage_route'] LOOP
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_00_route_statement_barrier BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_route_statement_barrier()',t);
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_route_immutable BEFORE UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION mst2_route_immutable()',t);
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_route_truncate_guard BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_route_immutable()',t);
    IF t<>'mst2_metadata_namespace' THEN
      EXECUTE pg_catalog.format('CREATE TRIGGER mst2_route_insert_guard BEFORE INSERT ON %I FOR EACH ROW EXECUTE FUNCTION mst2_route_insert_guard()',t);
    END IF;
  END LOOP;
  FOREACH t IN ARRAY ARRAY['mst2_snapshot_context','mst2_snapshot_lease'] LOOP
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_00_route_statement_barrier BEFORE INSERT ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_route_statement_barrier()',t);
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_route_source_delete_guard BEFORE DELETE ON %I FOR EACH ROW EXECUTE FUNCTION mst2_route_immutable()',t);
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_route_source_truncate_guard BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_route_immutable()',t);
    EXECUTE pg_catalog.format('CREATE CONSTRAINT TRIGGER mst2_route_complete AFTER INSERT ON %I DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_route_complete()',t);
  END LOOP;
END $$;
CREATE TRIGGER mst2_route_namespace_registration_closed BEFORE INSERT ON mst2_metadata_namespace
  FOR EACH ROW EXECUTE FUNCTION mst2_route_immutable();
CREATE TRIGGER mst2_route_context_insert AFTER INSERT ON mst2_snapshot_context
  FOR EACH ROW EXECUTE FUNCTION mst2_route_context_insert();
CREATE TRIGGER mst2_route_lease_insert AFTER INSERT ON mst2_snapshot_lease
  FOR EACH ROW EXECUTE FUNCTION mst2_route_lease_insert();

INSERT INTO mst2_snapshot_storage_route
SELECT s.snapshot_id,n.namespace_uuid,s.canonical_descriptor,s.instance_id,s.commit_oid,s.root_tree_oid,
  s.metadata_root,mst2_route_profile(p.source_domain,p.tagged_root_tree_oid,p.scope,p.schema_version,p.metadata_codec,
    p.materialization_policy,p.fs_semantics,p.access_projection,p.verification_revision,p.projection_revision)
FROM mst2_snapshot_context s JOIN mst2_metadata_prepare p ON p.prepare_id=s.prepare_id
CROSS JOIN mst2_metadata_namespace n;
INSERT INTO mst2_generic_session_storage_binding
SELECT pg_catalog.gen_random_uuid(),s.snapshot_id,r.namespace_uuid,s.prepare_id,s.metadata_root
FROM mst2_snapshot_context s JOIN mst2_snapshot_storage_route r USING(snapshot_id);
INSERT INTO mst2_lease_storage_route
SELECT l.lease_id,l.snapshot_id,b.namespace_uuid,b.session_incarnation,b.prepare_id,b.metadata_root,
  l.authorization_epoch,l.publication_sequence,l.writer_epoch,l.certificate_receipt_id
FROM mst2_snapshot_lease l JOIN mst2_generic_session_storage_binding b USING(snapshot_id);
DO $$ BEGIN
  IF NOT mst2_route_scope_valid($CORE_LITERAL$)
    OR EXISTS(SELECT 1 FROM mst2_snapshot_context s WHERE NOT mst2_route_snapshot_proof(s.snapshot_id))
    OR EXISTS(SELECT 1 FROM mst2_snapshot_lease l WHERE NOT mst2_route_lease_proof(l.lease_id)) THEN
    RAISE EXCEPTION 'storage route backfill did not cover the complete durable inventory';
  END IF;
END $$;
