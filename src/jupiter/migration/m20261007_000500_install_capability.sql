LOCK TABLE mst2_metadata_prepare,mst2_metadata_prepare_page IN ACCESS EXCLUSIVE MODE;

CREATE TABLE mst2_metadata_install_seal (
  prepare_id text PRIMARY KEY REFERENCES mst2_metadata_prepare(prepare_id),
  operation_id text NOT NULL CHECK (octet_length(operation_id) BETWEEN 1 AND 255),
  manifest_digest bytea NOT NULL CHECK (octet_length(manifest_digest)=32),
  members_digest bytea NOT NULL CHECK (octet_length(members_digest)=32),
  primary_scope bytea NOT NULL CHECK (octet_length(primary_scope) BETWEEN 1 AND 16384),
  install_seal bytea NOT NULL CHECK (octet_length(install_seal)=32),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- The actual regulated table, rather than caller search_path, selects the lock.
CREATE FUNCTION mst2_install_capability_barrier() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA
    OR pg_catalog.pg_is_in_recovery()
    OR pg_catalog.current_setting('transaction_isolation')<>'read committed' THEN
    RAISE EXCEPTION 'install capability mutation requires its actual primary schema and READ COMMITTED';
  END IF;
  PERFORM pg_catalog.set_config('search_path',pg_catalog.quote_ident(TG_TABLE_SCHEMA)||',pg_catalog,pg_temp',true);
  PERFORM pg_catalog.set_config('lock_timeout','5000ms',true);
  PERFORM pg_catalog.pg_advisory_xact_lock(1296717362,pg_catalog.hashtext(TG_TABLE_SCHEMA));
  RETURN NULL;
END $$;

CREATE FUNCTION mst2_install_capability_register() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE p record; members bytea; actual jsonb; member_count bigint; member_bytes bigint; has_root boolean;
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'install capability registration is immutable'; END IF;
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN
    RAISE EXCEPTION 'install capability registration is outside its actual schema';
  END IF;
  EXECUTE pg_catalog.format('SELECT * FROM %I.mst2_metadata_prepare WHERE prepare_id=$1',TG_TABLE_SCHEMA)
    INTO p USING NEW.prepare_id;
  IF p.prepare_id IS NULL OR p.operation_id IS DISTINCT FROM NEW.operation_id
    OR p.manifest_digest IS DISTINCT FROM NEW.manifest_digest
    OR pg_catalog.sha256(p.canonical_plan) IS DISTINCT FROM NEW.manifest_digest
    OR p.state NOT IN ('PREPARING','COMMITTED') OR p.coverage_retired_at IS NOT NULL
    OR p.canonical_bindings IS NOT NULL OR p.bindings_digest IS NOT NULL
    OR p.primary_scope IS NOT NULL OR p.storage_seal IS NOT NULL OR p.graph_domain IS NOT NULL THEN
    RAISE EXCEPTION 'install capability requires a fixed unbound legacy preparation';
  END IF;
  EXECUTE pg_catalog.format(
    'SELECT count(*),sum(expected_size),bool_or(page_id=$2),
       sha256(string_agg(page_id||int4send(expected_size)||
         CASE WHEN generation IS NULL THEN decode(''00'',''hex'')
         ELSE decode(''01'',''hex'')||int8send(generation) END,''''::bytea ORDER BY page_id))
     FROM %I.mst2_metadata_prepare_page WHERE prepare_id=$1',TG_TABLE_SCHEMA)
    INTO member_count,member_bytes,has_root,members USING NEW.prepare_id,p.metadata_root;
  IF member_count<>p.node_count OR member_bytes<>p.total_bytes OR has_root IS DISTINCT FROM true
    OR members IS DISTINCT FROM NEW.members_digest THEN
    RAISE EXCEPTION 'install capability complete membership proof disagrees';
  END IF;
  EXECUTE pg_catalog.format(
    'SELECT EXISTS(SELECT 1 FROM %I.mst2_metadata_prepare_page WHERE prepare_id=$1 AND generation IS NOT NULL)',
    TG_TABLE_SCHEMA) INTO has_root USING NEW.prepare_id;
  IF has_root THEN RAISE EXCEPTION 'legacy install capability cannot adopt generation bindings'; END IF;
  EXECUTE pg_catalog.format(
    'SELECT jsonb_build_array(s.storage_uuid,current_database(),d.oid::bigint,$1,n.oid::bigint,
       inet_server_addr()::text,inet_server_port())
     FROM %I.mst2_metadata_storage_scope s
     JOIN pg_catalog.pg_database d ON d.datname=current_database()
     JOIN pg_catalog.pg_namespace n ON n.nspname=$1 WHERE s.singleton=1',TG_TABLE_SCHEMA)
    INTO actual USING TG_TABLE_SCHEMA;
  IF actual IS NULL OR pg_catalog.convert_from(NEW.primary_scope,'UTF8')::jsonb IS DISTINCT FROM actual
    OR NEW.install_seal IS DISTINCT FROM pg_catalog.sha256(
      pg_catalog.convert_to('MST2-LEGACY-INSTALL-CAPABILITY-1','UTF8')||pg_catalog.decode('00','hex')||
      pg_catalog.uuid_send(NEW.prepare_id::uuid)||
      pg_catalog.int4send(pg_catalog.octet_length(NEW.operation_id))||pg_catalog.convert_to(NEW.operation_id,'UTF8')||
      NEW.manifest_digest||NEW.members_digest||
      pg_catalog.int4send(pg_catalog.octet_length(NEW.primary_scope))||NEW.primary_scope) THEN
    RAISE EXCEPTION 'install capability seal or actual primary scope disagrees';
  END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION mst2_install_capability_prepare_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE registered boolean;
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN
    RAISE EXCEPTION 'install capability preparation is outside its actual schema';
  END IF;
  EXECUTE pg_catalog.format('SELECT EXISTS(SELECT 1 FROM %I.mst2_metadata_install_seal WHERE prepare_id=$1)',TG_TABLE_SCHEMA)
    INTO registered USING OLD.prepare_id;
  IF registered AND (TG_OP='DELETE' OR
    (pg_catalog.to_jsonb(NEW)-ARRAY['state','committed_at','aborted_at','coverage_retired_at']) IS DISTINCT FROM
    (pg_catalog.to_jsonb(OLD)-ARRAY['state','committed_at','aborted_at','coverage_retired_at'])) THEN
    RAISE EXCEPTION 'registered metadata preparation identity is immutable';
  END IF;
  IF TG_OP='DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION mst2_install_capability_mapping_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE old_id text; new_id text; registered boolean;
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN
    RAISE EXCEPTION 'install capability membership is outside its actual schema';
  END IF;
  IF TG_OP<>'INSERT' THEN old_id:=OLD.prepare_id; END IF;
  IF TG_OP<>'DELETE' THEN new_id:=NEW.prepare_id; END IF;
  EXECUTE pg_catalog.format(
    'SELECT EXISTS(SELECT 1 FROM %I.mst2_metadata_install_seal WHERE prepare_id=$1 OR prepare_id=$2)',TG_TABLE_SCHEMA)
    INTO registered USING old_id,new_id;
  IF registered THEN RAISE EXCEPTION 'registered metadata membership is immutable'; END IF;
  IF TG_OP='DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION mst2_install_capability_truncate_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE registered boolean;
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA
    OR pg_catalog.pg_is_in_recovery()
    OR pg_catalog.current_setting('transaction_isolation')<>'read committed' THEN
    RAISE EXCEPTION 'install capability truncation requires its actual primary schema and READ COMMITTED';
  END IF;
  PERFORM pg_catalog.set_config('lock_timeout','5000ms',true);
  PERFORM pg_catalog.pg_advisory_xact_lock(1296717362,pg_catalog.hashtext(TG_TABLE_SCHEMA));
  EXECUTE pg_catalog.format('SELECT EXISTS(SELECT 1 FROM %I.mst2_metadata_install_seal)',TG_TABLE_SCHEMA)
    INTO registered;
  IF registered THEN RAISE EXCEPTION 'registered metadata evidence cannot be truncated'; END IF;
  RETURN NULL;
END $$;

DO $$ DECLARE t text; BEGIN
  FOREACH t IN ARRAY ARRAY['mst2_metadata_prepare','mst2_metadata_prepare_page','mst2_metadata_install_seal'] LOOP
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_00_install_capability_barrier BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_install_capability_barrier()',t);
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_install_capability_truncate_guard BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_install_capability_truncate_guard()',t);
  END LOOP;
END $$;
CREATE TRIGGER mst2_install_capability_register BEFORE INSERT OR UPDATE OR DELETE
  ON mst2_metadata_install_seal FOR EACH ROW EXECUTE FUNCTION mst2_install_capability_register();
CREATE TRIGGER mst2_install_capability_prepare_guard BEFORE UPDATE OR DELETE
  ON mst2_metadata_prepare FOR EACH ROW EXECUTE FUNCTION mst2_install_capability_prepare_guard();
CREATE TRIGGER mst2_install_capability_mapping_guard BEFORE INSERT OR UPDATE OR DELETE
  ON mst2_metadata_prepare_page FOR EACH ROW EXECUTE FUNCTION mst2_install_capability_mapping_guard();
