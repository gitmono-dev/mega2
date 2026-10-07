CREATE FUNCTION mst2_route_qualified_namespace_valid(id uuid) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT mst2_route_scope_valid($CORE_LITERAL$) AND EXISTS(SELECT 1 FROM mst2_metadata_namespace q
    JOIN mst2_metadata_namespace g ON g.singleton=1
    JOIN mst2_qualified_family_policy policy ON policy.singleton=1
    JOIN pg_catalog.pg_namespace physical ON physical.oid=q.metadata_schema_oid AND physical.nspname=q.metadata_schema
    WHERE q.namespace_uuid=id AND q.singleton IS NULL AND q.graph_domain='qualified-v1'
      AND q.family_identity='v3-rooted-qualified-1' AND q.admission_state='ROOTED_Q_ADMITTED' AND q.collector_state='ENABLED'
      AND q.metadata_schema='mst2q_'||replace(q.namespace_uuid::text,'-','')
      AND q.metadata_schema_oid<>q.core_schema_oid
      AND ROW(q.core_schema,q.core_schema_oid,q.database_name,q.database_oid,q.storage_uuid,
          q.server_address,q.server_port,q.mono_lock_key2) IS NOT DISTINCT FROM
        ROW(g.core_schema,g.core_schema_oid,g.database_name,g.database_oid,g.storage_uuid,
          g.server_address,g.server_port,g.mono_lock_key2)
      AND q.implementation_fingerprint=policy.implementation_fingerprint
      AND policy.authority_catalog=mst2_route_family_catalog(q.core_schema_oid,0::oid,q.metadata_schema_oid)
      AND q.catalog_fingerprint=mst2_route_family_catalog(q.core_schema_oid,q.metadata_schema_oid)
      AND policy.expected_shape=mst2_route_family_shape(q.metadata_schema_oid,q.namespace_uuid,q.metadata_storage_uuid))
$$;

CREATE OR REPLACE FUNCTION mst2_route_statement_barrier() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE q_id uuid; caller text:=pg_catalog.current_schema();
BEGIN
  IF TG_TABLE_SCHEMA<>$CORE_LITERAL$ OR NOT EXISTS(SELECT 1 FROM pg_catalog.pg_class
    WHERE oid=TG_RELID AND relnamespace=$CORE_OID$) THEN
    RAISE EXCEPTION 'storage route mutation is outside its captured core schema';
  END IF;
  IF caller IS DISTINCT FROM $CORE_LITERAL$ THEN
    SELECT n.namespace_uuid INTO q_id FROM $CORE_SCHEMA$.mst2_metadata_namespace n
      WHERE n.metadata_schema=caller AND n.graph_domain='qualified-v1';
    IF NOT FOUND OR NOT $CORE_SCHEMA$.mst2_route_qualified_namespace_valid(q_id) THEN
      RAISE EXCEPTION 'storage route mutation caller has no exact captured physical family'; END IF;
  END IF;
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  RETURN NULL;
END $$;

CREATE FUNCTION mst2_route_qualified_snapshot_proof(sid text) RETURNS boolean LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE r mst2_snapshot_storage_route%ROWTYPE; n mst2_metadata_namespace%ROWTYPE; valid boolean;
BEGIN
  SELECT * INTO r FROM mst2_snapshot_storage_route WHERE snapshot_id=sid;
  IF NOT FOUND THEN RETURN false; END IF;
  SELECT * INTO n FROM mst2_metadata_namespace WHERE namespace_uuid=r.namespace_uuid;
  IF NOT FOUND OR NOT mst2_route_qualified_namespace_valid(n.namespace_uuid) THEN RETURN false; END IF;
  EXECUTE pg_catalog.format('SELECT %I.mst2_metadata_snapshot_route_proof($1,$2,$3,$4,$5,$6,$7)',n.metadata_schema)
    INTO valid USING r.snapshot_id,r.canonical_descriptor,r.instance_id,r.commit_oid,r.root_tree_oid,r.metadata_root,r.source_profile;
  RETURN coalesce(valid,false);
END $$;

CREATE FUNCTION mst2_route_qualified_lease_proof(lid text) RETURNS boolean LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE r mst2_lease_storage_route%ROWTYPE; n mst2_metadata_namespace%ROWTYPE; valid boolean;
BEGIN
  SELECT * INTO r FROM mst2_lease_storage_route WHERE lease_id=lid;
  IF NOT FOUND THEN RETURN false; END IF;
  SELECT * INTO n FROM mst2_metadata_namespace WHERE namespace_uuid=r.namespace_uuid;
  IF NOT FOUND OR NOT mst2_route_qualified_namespace_valid(n.namespace_uuid) THEN RETURN false; END IF;
  EXECUTE pg_catalog.format('SELECT %I.mst2_metadata_lease_route_proof($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)',n.metadata_schema)
    INTO valid USING r.lease_id,r.snapshot_id,r.namespace_uuid,r.session_incarnation,r.prepare_id,r.metadata_root,
      r.authorization_epoch,r.publication_sequence,r.writer_epoch,r.certificate_receipt_id;
  RETURN coalesce(valid,false);
END $$;

-- Existing G proof functions remain authoritative for their original family.
CREATE FUNCTION mst2_route_family_for_snapshot(sid text,caller_schema text)
RETURNS TABLE(namespace_uuid uuid,graph_domain text) LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE route mst2_snapshot_storage_route%ROWTYPE; namespace mst2_metadata_namespace%ROWTYPE;
BEGIN
  IF NOT mst2_route_scope_valid(caller_schema) THEN RAISE EXCEPTION 'storage route captured primary scope is unavailable'; END IF;
  SELECT * INTO route FROM mst2_snapshot_storage_route WHERE snapshot_id=sid;
  IF NOT FOUND THEN RETURN; END IF;
  SELECT * INTO namespace FROM mst2_metadata_namespace n WHERE n.namespace_uuid=route.namespace_uuid;
  IF NOT FOUND OR namespace.graph_domain='generic-v1' AND NOT mst2_route_snapshot_proof(sid)
    OR namespace.graph_domain='qualified-v1' AND NOT mst2_route_qualified_snapshot_proof(sid)
    OR namespace.graph_domain NOT IN ('generic-v1','qualified-v1') THEN
    RAISE EXCEPTION 'permanent snapshot route has a corrupt exact family or fixed source binding'; END IF;
  RETURN QUERY SELECT namespace.namespace_uuid,namespace.graph_domain;
END $$;

CREATE FUNCTION mst2_route_family_for_lease(lid text,caller_schema text)
RETURNS TABLE(namespace_uuid uuid,graph_domain text) LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE route mst2_lease_storage_route%ROWTYPE; namespace mst2_metadata_namespace%ROWTYPE;
BEGIN
  IF NOT mst2_route_scope_valid(caller_schema) THEN RAISE EXCEPTION 'storage route captured primary scope is unavailable'; END IF;
  SELECT * INTO route FROM mst2_lease_storage_route WHERE lease_id=lid;
  IF NOT FOUND THEN RETURN; END IF;
  SELECT * INTO namespace FROM mst2_metadata_namespace n WHERE n.namespace_uuid=route.namespace_uuid;
  IF NOT FOUND OR namespace.graph_domain='generic-v1' AND NOT mst2_route_lease_proof(lid)
    OR namespace.graph_domain='qualified-v1' AND NOT mst2_route_qualified_lease_proof(lid)
    OR namespace.graph_domain NOT IN ('generic-v1','qualified-v1') THEN
    RAISE EXCEPTION 'permanent lease route has a corrupt exact family or fixed source binding'; END IF;
  RETURN QUERY SELECT namespace.namespace_uuid,namespace.graph_domain;
END $$;

CREATE OR REPLACE FUNCTION mst2_route_insert_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE namespace mst2_metadata_namespace%ROWTYPE; valid boolean;
BEGIN
  SELECT * INTO namespace FROM mst2_metadata_namespace WHERE namespace_uuid=NEW.namespace_uuid;
  IF NOT FOUND THEN RAISE EXCEPTION 'storage route target namespace is not registered'; END IF;
  IF namespace.graph_domain='qualified-v1' THEN
    IF NOT mst2_route_qualified_namespace_valid(namespace.namespace_uuid) THEN
      RAISE EXCEPTION 'qualified route has no exact complete physical family catalog'; END IF;
    IF TG_TABLE_NAME='mst2_snapshot_storage_route' THEN
      EXECUTE pg_catalog.format('SELECT %I.mst2_metadata_snapshot_candidate($1,$2,$3,$4,$5,$6,$7)',namespace.metadata_schema)
        INTO valid USING NEW.snapshot_id,NEW.canonical_descriptor,NEW.instance_id,NEW.commit_oid,
          NEW.root_tree_oid,NEW.metadata_root,NEW.source_profile;
    ELSIF TG_TABLE_NAME='mst2_lease_storage_route' THEN
      EXECUTE pg_catalog.format('SELECT %I.mst2_metadata_lease_route_proof($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)',namespace.metadata_schema)
        INTO valid USING NEW.lease_id,NEW.snapshot_id,NEW.namespace_uuid,NEW.session_incarnation,NEW.prepare_id,
          NEW.metadata_root,NEW.authorization_epoch,NEW.publication_sequence,NEW.writer_epoch,NEW.certificate_receipt_id;
    ELSE RAISE EXCEPTION 'qualified route cannot use a generic binding relation'; END IF;
    IF NOT coalesce(valid,false) THEN RAISE EXCEPTION 'qualified route is not independently derived from its exact source'; END IF;
    RETURN NEW;
  END IF;
  IF namespace.singleton IS DISTINCT FROM 1 OR namespace.graph_domain<>'generic-v1' THEN
    RAISE EXCEPTION 'storage route target family is unsupported'; END IF;
  IF TG_TABLE_NAME='mst2_snapshot_storage_route' THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_snapshot_context s JOIN mst2_metadata_prepare p ON p.prepare_id=s.prepare_id
      JOIN mst2_metadata_namespace n ON n.namespace_uuid=NEW.namespace_uuid
      WHERE n.singleton=1 AND n.graph_domain='generic-v1' AND s.snapshot_id=NEW.snapshot_id AND NEW.canonical_descriptor=s.canonical_descriptor
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

CREATE FUNCTION mst2_route_permanent_complete() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE domain text; valid boolean;
BEGIN
  SELECT graph_domain INTO domain FROM mst2_metadata_namespace WHERE namespace_uuid=NEW.namespace_uuid;
  IF TG_TABLE_NAME='mst2_snapshot_storage_route' THEN
    valid:=CASE domain WHEN 'generic-v1' THEN mst2_route_snapshot_proof(NEW.snapshot_id)
      WHEN 'qualified-v1' THEN mst2_route_qualified_snapshot_proof(NEW.snapshot_id) ELSE false END;
  ELSE
    valid:=CASE domain WHEN 'generic-v1' THEN mst2_route_lease_proof(NEW.lease_id)
      WHEN 'qualified-v1' THEN mst2_route_qualified_lease_proof(NEW.lease_id) ELSE false END;
  END IF;
  IF NOT coalesce(valid,false) THEN RAISE EXCEPTION 'permanent storage route cannot commit without its exact actual incarnation'; END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_route_permanent_complete AFTER INSERT ON mst2_snapshot_storage_route
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_route_permanent_complete();
CREATE CONSTRAINT TRIGGER mst2_route_permanent_complete AFTER INSERT ON mst2_lease_storage_route
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_route_permanent_complete();
