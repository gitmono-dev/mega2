SELECT mst2_route_enter(current_schema());
SET LOCAL search_path=$CORE_SCHEMA$,pg_catalog,pg_temp;
LOCK TABLE mst2_metadata_namespace IN ACCESS EXCLUSIVE MODE;

ALTER TABLE mst2_metadata_namespace
  DROP CONSTRAINT mst2_metadata_namespace_pkey,
  ALTER COLUMN singleton DROP NOT NULL,
  DROP CONSTRAINT mst2_metadata_namespace_family_identity_check,
  DROP CONSTRAINT mst2_metadata_namespace_graph_domain_check,
  DROP CONSTRAINT mst2_metadata_namespace_admission_state_check,
  DROP CONSTRAINT mst2_metadata_namespace_collector_state_check,
  DROP CONSTRAINT mst2_metadata_namespace_check,
  ADD COLUMN metadata_storage_uuid text,
  ADD COLUMN implementation_fingerprint bytea,
  ADD COLUMN catalog_fingerprint bytea,
  ADD CONSTRAINT mst2_namespace_family_pair CHECK ((
    (singleton=1 AND family_identity='v3-generic-session-1' AND graph_domain='generic-v1'
      AND admission_state='G_ADMITTED_Q_CLOSED' AND collector_state='CLOSED' AND core_schema=metadata_schema
      AND core_schema_oid=metadata_schema_oid AND metadata_storage_uuid IS NULL
      AND implementation_fingerprint IS NULL AND catalog_fingerprint IS NULL)
    OR (singleton IS NULL AND family_identity='v3-rooted-qualified-1' AND graph_domain='qualified-v1'
      AND admission_state='ROOTED_Q_ADMITTED' AND collector_state='ENABLED' AND core_schema<>metadata_schema
      AND core_schema_oid<>metadata_schema_oid AND metadata_storage_uuid IS NOT NULL
      AND octet_length(implementation_fingerprint)=32 AND octet_length(catalog_fingerprint)=32)
  ) IS TRUE);
CREATE UNIQUE INDEX idx_mst2_namespace_generic_slot ON mst2_metadata_namespace(singleton)
  WHERE singleton IS NOT NULL;
CREATE UNIQUE INDEX idx_mst2_namespace_domain ON mst2_metadata_namespace(graph_domain);
CREATE UNIQUE INDEX idx_mst2_namespace_metadata_schema ON mst2_metadata_namespace(metadata_schema_oid);

CREATE OR REPLACE FUNCTION mst2_route_enter(caller_schema text) RETURNS uuid LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE n mst2_metadata_namespace%ROWTYPE; g mst2_metadata_namespace%ROWTYPE; mono_held boolean; route_held boolean;
BEGIN
  IF NOT mst2_route_scope_valid(caller_schema) OR (SELECT count(*) FROM mst2_metadata_namespace)>2 THEN
    RAISE EXCEPTION 'storage route captured primary scope is unavailable';
  END IF;
  SELECT * INTO STRICT g FROM mst2_metadata_namespace WHERE singleton=1;
  mono_held:=mst2_route_lock_held(1297043024,g.mono_lock_key2);
  route_held:=mst2_route_lock_held(1296718001,pg_catalog.hashtext(g.core_schema));
  IF mst2_route_lock_held(1296718001,pg_catalog.hashtext(g.core_schema),false) AND NOT mono_held THEN
    RAISE EXCEPTION 'storage route lock was acquired before core mono';
  END IF;
  IF EXISTS(SELECT 1 FROM mst2_metadata_namespace x
    WHERE mst2_route_lock_held(1296717362,pg_catalog.hashtext(x.metadata_schema),false))
    AND NOT (mono_held AND route_held) THEN
    RAISE EXCEPTION 'storage route cannot acquire core locks after retention';
  END IF;
  PERFORM pg_catalog.set_config('lock_timeout','5000ms',true);
  PERFORM pg_catalog.pg_advisory_xact_lock(1297043024,g.mono_lock_key2);
  PERFORM pg_catalog.pg_advisory_xact_lock(1296718001,pg_catalog.hashtext(g.core_schema));
  FOR n IN SELECT * FROM mst2_metadata_namespace ORDER BY namespace_uuid LOOP
    PERFORM pg_catalog.pg_advisory_xact_lock(1296717362,pg_catalog.hashtext(n.metadata_schema));
  END LOOP;
  RETURN g.namespace_uuid;
END $$;

-- The new namespace is included before acquiring any retention lock.
CREATE FUNCTION mst2_route_family_candidate_enter(caller_schema text,candidate uuid,candidate_schema text)
RETURNS void LANGUAGE plpgsql VOLATILE SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE g mst2_metadata_namespace%ROWTYPE; r record;
BEGIN
  IF NOT mst2_route_scope_valid(caller_schema) OR candidate IS NULL
    OR substr(candidate::text,15,1)<>'4' OR substr(candidate::text,20,1) NOT IN ('8','9','a','b')
    OR candidate_schema IS DISTINCT FROM 'mst2q_'||replace(candidate::text,'-','') THEN
    RAISE EXCEPTION 'qualified provisioning candidate is not a fresh server namespace';
  END IF;
  IF EXISTS(SELECT 1 FROM pg_catalog.pg_locks WHERE locktype='advisory' AND pid=pg_catalog.pg_backend_pid()
    AND database=(SELECT oid FROM pg_catalog.pg_database WHERE datname=pg_catalog.current_database())
    AND classid=1296717362::oid AND objsubid=2 AND granted) THEN
    RAISE EXCEPTION 'qualified provisioning cannot extend a previously locked retention set';
  END IF;
  SELECT * INTO STRICT g FROM mst2_metadata_namespace WHERE singleton=1;
  IF mst2_route_lock_held(1296718001,pg_catalog.hashtext(g.core_schema),false)
    AND NOT mst2_route_lock_held(1297043024,g.mono_lock_key2) THEN
    RAISE EXCEPTION 'storage route lock was acquired before core mono';
  END IF;
  PERFORM pg_catalog.set_config('lock_timeout','5000ms',true);
  PERFORM pg_catalog.pg_advisory_xact_lock(1297043024,g.mono_lock_key2);
  PERFORM pg_catalog.pg_advisory_xact_lock(1296718001,pg_catalog.hashtext(g.core_schema));
  -- A racing successful bootstrap can register while this caller waits.
  IF EXISTS(SELECT 1 FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1') THEN
    RAISE EXCEPTION 'qualified provisioning candidate lost bootstrap serialization';
  END IF;
  FOR r IN SELECT namespace_uuid,metadata_schema FROM mst2_metadata_namespace
    UNION ALL SELECT candidate,candidate_schema ORDER BY namespace_uuid LOOP
    PERFORM pg_catalog.pg_advisory_xact_lock(1296717362,pg_catalog.hashtext(r.metadata_schema));
  END LOOP;
END $$;

CREATE FUNCTION mst2_route_family_catalog(c_oid oid,q_oid oid,exempt_q_oid oid DEFAULT 0::oid) RETURNS bytea LANGUAGE sql VOLATILE
SET search_path=pg_catalog,pg_temp AS $catalog$ $CATALOG_SQL$ $catalog$;
CREATE FUNCTION mst2_route_family_shape(q_oid oid,n_uuid uuid,s_uuid text) RETURNS bytea LANGUAGE sql VOLATILE
SET search_path=pg_catalog,pg_temp AS $shape$ $SHAPE_SQL$ $shape$;
CREATE TABLE mst2_qualified_family_policy (
  singleton smallint PRIMARY KEY CHECK(singleton=1),implementation_fingerprint bytea NOT NULL CHECK(octet_length(implementation_fingerprint)=32),
  expected_shape bytea NOT NULL CHECK(octet_length(expected_shape)=32),
  authority_catalog bytea NOT NULL CHECK(octet_length(authority_catalog)=32)
);
CREATE TRIGGER mst2_route_family_policy_immutable BEFORE UPDATE OR DELETE ON mst2_qualified_family_policy
  FOR EACH ROW EXECUTE FUNCTION mst2_route_immutable();
CREATE TRIGGER mst2_route_family_policy_truncate_guard BEFORE TRUNCATE ON mst2_qualified_family_policy
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_route_immutable();

CREATE FUNCTION mst2_route_family_registration_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE g mst2_metadata_namespace%ROWTYPE; stamp record;
BEGIN
  SELECT * INTO STRICT g FROM mst2_metadata_namespace WHERE singleton=1;
  IF (SELECT count(*) FROM mst2_metadata_namespace)<>1 OR NEW.singleton IS NOT NULL
    OR substr(NEW.namespace_uuid::text,15,1)<>'4' OR substr(NEW.namespace_uuid::text,20,1) NOT IN ('8','9','a','b')
    OR NEW.graph_domain<>'qualified-v1' OR NEW.family_identity<>'v3-rooted-qualified-1'
    OR NEW.admission_state<>'ROOTED_Q_ADMITTED' OR NEW.collector_state<>'ENABLED'
    OR ROW(NEW.core_schema,NEW.core_schema_oid,NEW.database_name,NEW.database_oid,NEW.storage_uuid,
      NEW.server_address,NEW.server_port,NEW.mono_lock_key2) IS DISTINCT FROM
      ROW(g.core_schema,g.core_schema_oid,g.database_name,g.database_oid,g.storage_uuid,
      g.server_address,g.server_port,g.mono_lock_key2)
    OR NEW.metadata_schema IS DISTINCT FROM 'mst2q_'||replace(NEW.namespace_uuid::text,'-','')
    OR NEW.metadata_storage_uuid IS NOT DISTINCT FROM g.storage_uuid
    OR NEW.metadata_storage_uuid IS DISTINCT FROM (NEW.metadata_storage_uuid::uuid)::text
    OR substr(NEW.metadata_storage_uuid,15,1)<>'4' OR substr(NEW.metadata_storage_uuid,20,1) NOT IN ('8','9','a','b')
    OR NOT EXISTS(SELECT 1 FROM pg_catalog.pg_namespace n
      WHERE n.oid=NEW.metadata_schema_oid AND n.nspname=NEW.metadata_schema)
    OR NEW.implementation_fingerprint IS DISTINCT FROM decode('$IMPLEMENTATION_SHA$','hex')
    OR NOT mst2_route_lock_held(1297043024,g.mono_lock_key2)
    OR NOT mst2_route_lock_held(1296718001,pg_catalog.hashtext(g.core_schema))
    OR NOT mst2_route_lock_held(1296717362,pg_catalog.hashtext(NEW.metadata_schema))
    OR NOT mst2_route_lock_held(1296717362,pg_catalog.hashtext(g.metadata_schema)) THEN
    RAISE EXCEPTION 'qualified namespace registration has no exact admitted rooted family scope and lock set';
  END IF;
  EXECUTE pg_catalog.format('SELECT * FROM %I.mst2_metadata_family_identity WHERE singleton=1',NEW.metadata_schema)
    INTO STRICT stamp;
  IF ROW(stamp.namespace_uuid,stamp.storage_uuid,stamp.core_schema_oid,stamp.metadata_schema_oid,
    stamp.family_identity,stamp.implementation_fingerprint) IS DISTINCT FROM
    ROW(NEW.namespace_uuid,NEW.metadata_storage_uuid,NEW.core_schema_oid,NEW.metadata_schema_oid,
    NEW.family_identity,NEW.implementation_fingerprint)
    OR NEW.catalog_fingerprint IS DISTINCT FROM mst2_route_family_catalog(NEW.core_schema_oid,NEW.metadata_schema_oid) THEN
    RAISE EXCEPTION 'qualified namespace registration fingerprint disagrees with its physical family';
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_qualified_family_policy p WHERE p.singleton=1
    AND p.implementation_fingerprint=NEW.implementation_fingerprint
    AND p.authority_catalog=mst2_route_family_catalog(NEW.core_schema_oid,0::oid,NEW.metadata_schema_oid)
    AND p.expected_shape=mst2_route_family_shape(NEW.metadata_schema_oid,NEW.namespace_uuid,NEW.metadata_storage_uuid)) THEN
    RAISE EXCEPTION 'qualified namespace does not have the trusted complete physical family shape';
  END IF;
  RETURN NEW;
END $$;
DROP TRIGGER mst2_route_namespace_registration_closed ON mst2_metadata_namespace;
CREATE TRIGGER mst2_route_namespace_registration_guard BEFORE INSERT ON mst2_metadata_namespace
  FOR EACH ROW EXECUTE FUNCTION mst2_route_family_registration_guard();

CREATE FUNCTION mst2_metadata_has_generic_overlap(p bytea) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM mst2_retention_node WHERE node_id='page:sha256:'||encode(p,'hex'))
    OR EXISTS(SELECT 1 FROM mst2_retention_gc_op WHERE node_id='page:sha256:'||encode(p,'hex'))
$$;

-- Existing generic route derivation must never select the newly registered Q row.
$GENERIC_INSERT_GUARD$
$SOURCE_REVISION_SQL$
$ROOTED_ROUTES_SQL$
