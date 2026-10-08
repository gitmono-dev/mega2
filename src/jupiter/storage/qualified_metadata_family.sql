-- Dedicated physical v3 Q family with exact generations and owned roots.
SET LOCAL search_path=$Q_SCHEMA$,pg_catalog,pg_temp;
CREATE TABLE mst2_metadata_storage_scope (
  singleton smallint PRIMARY KEY CHECK(singleton=1),storage_uuid text NOT NULL UNIQUE
);
INSERT INTO mst2_metadata_storage_scope VALUES(1,'$STORAGE_UUID$');
CREATE TABLE mst2_metadata_family_identity (
  singleton smallint PRIMARY KEY CHECK(singleton=1),namespace_uuid uuid NOT NULL UNIQUE,
  storage_uuid text NOT NULL UNIQUE,core_schema_oid oid NOT NULL,metadata_schema_oid oid NOT NULL,
  family_identity text NOT NULL CHECK(family_identity='v3-rooted-qualified-1'),
  implementation_fingerprint bytea NOT NULL CHECK(octet_length(implementation_fingerprint)=32)
);
INSERT INTO mst2_metadata_family_identity VALUES(1,'$NAMESPACE_UUID$'::uuid,'$STORAGE_UUID$',
  $CORE_OID$,$Q_OID$,'v3-rooted-qualified-1',decode('$IMPLEMENTATION_SHA$','hex'));
CREATE TABLE mst2_metadata_lifetime (
  page_id bytea NOT NULL CHECK(octet_length(page_id)=32),
  node_id text NOT NULL CHECK(node_id='page:sha256:'||encode(page_id,'hex')),
  generation bigint NOT NULL CHECK(generation>0),
  state text NOT NULL CHECK(state IN ('RESERVED','LIVE','DELETING','REMOVED')),
  metadata_codec smallint NOT NULL CHECK(metadata_codec=1),
  expected_size integer NOT NULL CHECK(expected_size BETWEEN $HEADER_LEN$ AND $PAGE_MAX_BYTES$),
  graph_domain text NOT NULL CHECK(graph_domain='qualified-v1'),PRIMARY KEY(page_id,generation)
);
CREATE INDEX idx_mst2_metadata_lifetime_state_page ON mst2_metadata_lifetime(state,page_id);
CREATE INDEX idx_mst2_metadata_lifetime_node_generation ON mst2_metadata_lifetime(node_id,generation);
CREATE TABLE mst2_metadata_current (
  page_id bytea PRIMARY KEY CHECK(octet_length(page_id)=32),generation bigint NOT NULL CHECK(generation>0),
  FOREIGN KEY(page_id,generation) REFERENCES mst2_metadata_lifetime(page_id,generation)
);
CREATE TABLE mst2_metadata_payload (
  page_id bytea PRIMARY KEY CHECK(octet_length(page_id)=32),generation bigint NOT NULL CHECK(generation>0),
  metadata_codec smallint NOT NULL CHECK(metadata_codec=1),
  byte_size integer NOT NULL CHECK(byte_size BETWEEN $HEADER_LEN$ AND $PAGE_MAX_BYTES$),
  payload bytea NOT NULL CHECK(octet_length(payload)=byte_size),created_at timestamptz NOT NULL DEFAULT now(),
  FOREIGN KEY(page_id,generation) REFERENCES mst2_metadata_lifetime(page_id,generation)
);
CREATE TABLE mst2_metadata_prepare (
  prepare_id text PRIMARY KEY CHECK(prepare_id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'),
  operation_id text NOT NULL UNIQUE CHECK(octet_length(operation_id) BETWEEN 1 AND 255),
  manifest_digest bytea NOT NULL CHECK(octet_length(manifest_digest)=32),
  canonical_plan bytea NOT NULL CHECK(octet_length(canonical_plan)<=2097152),
  source_domain text NOT NULL CHECK(source_domain='native-git'),tagged_root_tree_oid text NOT NULL,
  plan_kind text NOT NULL DEFAULT 'COLD' CHECK(plan_kind IN ('COLD','ROOTED')),
  bindings_revision bigint NOT NULL DEFAULT 0 CHECK(bindings_revision>=0),
  scope text NOT NULL CHECK(octet_length(scope)<=4096),schema_version smallint NOT NULL,
  metadata_codec smallint NOT NULL CHECK(metadata_codec=1),materialization_policy smallint NOT NULL,
  fs_semantics smallint NOT NULL,access_projection smallint NOT NULL,verification_revision integer NOT NULL,
  projection_revision smallint NOT NULL,metadata_root bytea NOT NULL CHECK(octet_length(metadata_root)=32),
  node_count integer NOT NULL CHECK(node_count BETWEEN 0 AND 4096),
  edge_count integer NOT NULL CHECK(edge_count BETWEEN 0 AND 16384),
  total_bytes bigint NOT NULL CHECK(total_bytes BETWEEN 0 AND 67108864),
  state text NOT NULL CHECK(state IN ('PREPARING','COMMITTED','ABORTED')),
  created_at timestamptz NOT NULL DEFAULT now(),committed_at timestamptz,aborted_at timestamptz,
  coverage_retired_at timestamptz,
  canonical_bindings bytea NOT NULL CHECK(octet_length(canonical_bindings) BETWEEN 12 AND 196620),
  bindings_digest bytea NOT NULL CHECK(octet_length(bindings_digest)=32),
  primary_scope bytea NOT NULL CHECK(octet_length(primary_scope) BETWEEN 1 AND 16384),
  storage_seal bytea NOT NULL CHECK(octet_length(storage_seal)=32),
  graph_domain text NOT NULL CHECK(graph_domain='qualified-v1'),UNIQUE(prepare_id,storage_seal),
  CHECK((state='COMMITTED')=(committed_at IS NOT NULL) AND (state='ABORTED')=(aborted_at IS NOT NULL)
    AND (coverage_retired_at IS NULL OR state='COMMITTED'))
);
CREATE TABLE mst2_metadata_prepare_page (
  prepare_id text NOT NULL REFERENCES mst2_metadata_prepare(prepare_id),
  page_id bytea NOT NULL CHECK(octet_length(page_id)=32),generation bigint NOT NULL CHECK(generation>0),
  expected_size integer NOT NULL CHECK(expected_size BETWEEN $HEADER_LEN$ AND $PAGE_MAX_BYTES$),
  PRIMARY KEY(prepare_id,page_id),UNIQUE(prepare_id,page_id,generation),
  FOREIGN KEY(page_id,generation) REFERENCES mst2_metadata_lifetime(page_id,generation)
);
CREATE INDEX idx_mst2_metadata_prepare_page_lifetime ON mst2_metadata_prepare_page(page_id,generation,prepare_id);
CREATE TABLE mst2_metadata_graph_node (
  page_id bytea NOT NULL CHECK (octet_length(page_id)=32),
  generation bigint NOT NULL CHECK (generation>0),
  state text NOT NULL CHECK (state IN ('LIVE','DELETING')),
  metadata_codec smallint NOT NULL CHECK (metadata_codec=1),
  bytes bigint NOT NULL CHECK (bytes BETWEEN $HEADER_LEN$ AND $PAGE_MAX_BYTES$),
  incoming_refs bigint NOT NULL DEFAULT 0 CHECK (incoming_refs>=0),
  PRIMARY KEY(page_id,generation),
  FOREIGN KEY(page_id,generation) REFERENCES mst2_metadata_lifetime(page_id,generation)
);
CREATE INDEX idx_mst2_metadata_graph_node_gc ON mst2_metadata_graph_node(state,incoming_refs,page_id,generation);
CREATE TABLE mst2_metadata_graph_edge (
  parent_page bytea NOT NULL,
  parent_generation bigint NOT NULL,
  child_page bytea NOT NULL,
  child_generation bigint NOT NULL,
  PRIMARY KEY(parent_page,parent_generation,child_page,child_generation),
  FOREIGN KEY(parent_page,parent_generation) REFERENCES mst2_metadata_graph_node(page_id,generation),
  FOREIGN KEY(child_page,child_generation) REFERENCES mst2_metadata_graph_node(page_id,generation),
  CHECK (parent_page<>child_page OR parent_generation<>child_generation)
);
CREATE INDEX idx_mst2_metadata_graph_edge_child ON mst2_metadata_graph_edge(child_page,child_generation,parent_page,parent_generation);
CREATE TABLE mst2_metadata_graph_root (
  prepare_id text NOT NULL,
  storage_seal bytea NOT NULL CHECK (octet_length(storage_seal)=32),
  page_id bytea NOT NULL,
  generation bigint NOT NULL,
  PRIMARY KEY(prepare_id,page_id,generation),
  FOREIGN KEY(prepare_id,storage_seal) REFERENCES mst2_metadata_prepare(prepare_id,storage_seal),
  FOREIGN KEY(prepare_id,page_id,generation) REFERENCES mst2_metadata_prepare_page(prepare_id,page_id,generation),
  FOREIGN KEY(page_id,generation) REFERENCES mst2_metadata_graph_node(page_id,generation)
);
CREATE INDEX idx_mst2_metadata_graph_root_page ON mst2_metadata_graph_root(page_id,generation,prepare_id);
CREATE TABLE mst2_metadata_gc_op (
  operation_id uuid PRIMARY KEY,
  page_id bytea NOT NULL CHECK (octet_length(page_id)=32),
  generation bigint NOT NULL CHECK (generation>0),
  primary_scope bytea NOT NULL CHECK (octet_length(primary_scope) BETWEEN 1 AND 16384),
  graph_domain text NOT NULL CHECK (graph_domain='qualified-v1'),
  metadata_codec smallint NOT NULL CHECK (metadata_codec=1),
  expected_size integer NOT NULL CHECK (expected_size BETWEEN $HEADER_LEN$ AND $PAGE_MAX_BYTES$),
  graph_present boolean NOT NULL,
  had_payload boolean NOT NULL,
  payload_delete_xid bigint,
  state text NOT NULL CHECK (state IN ('PENDING','APPLIED')),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  completed_at timestamptz,
  UNIQUE(page_id,generation),
  FOREIGN KEY(page_id,generation) REFERENCES mst2_metadata_lifetime(page_id,generation),
  CHECK ((state='APPLIED')=(completed_at IS NOT NULL))
);
CREATE INDEX idx_mst2_metadata_gc_op_pending ON mst2_metadata_gc_op(created_at,operation_id) WHERE state='PENDING';

-- Incarnations are historical; SID alone is never a unique physical binding.
CREATE TABLE mst2_qualified_session_incarnation (
  snapshot_id text NOT NULL,session_incarnation uuid NOT NULL,namespace_uuid uuid NOT NULL,
  prepare_id text NOT NULL,storage_seal bytea NOT NULL CHECK(octet_length(storage_seal)=32),
  metadata_root bytea NOT NULL CHECK(octet_length(metadata_root)=32),root_generation bigint NOT NULL CHECK(root_generation>0),
  source_profile bytea NOT NULL,instance_id text NOT NULL,commit_oid text NOT NULL,root_tree_oid text NOT NULL,
  authorization_epoch bigint NOT NULL,publication_sequence bigint NOT NULL,writer_epoch bigint NOT NULL,
  certificate_receipt_id bigint NOT NULL,PRIMARY KEY(snapshot_id,session_incarnation),
  UNIQUE(snapshot_id,session_incarnation,prepare_id,storage_seal,metadata_root,root_generation),
  FOREIGN KEY(snapshot_id,namespace_uuid) REFERENCES $CORE_SCHEMA$.mst2_snapshot_storage_route(snapshot_id,namespace_uuid),
  FOREIGN KEY(prepare_id,storage_seal) REFERENCES mst2_metadata_prepare(prepare_id,storage_seal)
);
CREATE TABLE mst2_qualified_lease_binding (
  lease_id text PRIMARY KEY,snapshot_id text NOT NULL,session_incarnation uuid NOT NULL,
  prepare_id text NOT NULL,storage_seal bytea NOT NULL,metadata_root bytea NOT NULL,root_generation bigint NOT NULL,
  FOREIGN KEY(snapshot_id,session_incarnation,prepare_id,storage_seal,metadata_root,root_generation)
    REFERENCES mst2_qualified_session_incarnation(snapshot_id,session_incarnation,prepare_id,storage_seal,metadata_root,root_generation)
);

$CANONICAL_SQL$
$CERTIFICATES_SQL$
$ANCHORS_SQL$
$SOURCE_READ_SQL$
$ROOTED_SQL$

CREATE FUNCTION mst2_metadata_closed() RETURNS trigger LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN RAISE EXCEPTION 'qualified session serving and collector are closed'; END $$;
CREATE FUNCTION mst2_metadata_gc_apply(id uuid) RETURNS void LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN RAISE EXCEPTION 'qualified collector is closed'; END $$;
CREATE FUNCTION mst2_metadata_gc_finish(id uuid) RETURNS void LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN RAISE EXCEPTION 'qualified collector is closed'; END $$;
CREATE FUNCTION mst2_metadata_immutable() RETURNS trigger LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN RAISE EXCEPTION 'qualified family identity and history are immutable'; END $$;

-- Observe the caller before entering fixed trusted functions. Core fallback
-- never enters the pool search path; all cross-family authority is explicit.
CREATE FUNCTION mst2_metadata_dml_barrier() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM $Q_LITERAL$ OR TG_TABLE_SCHEMA IS DISTINCT FROM $Q_LITERAL$
    OR NOT EXISTS(SELECT 1 FROM pg_catalog.pg_class WHERE oid=TG_RELID AND relnamespace='$Q_OID$'::oid)
    OR NOT EXISTS(SELECT 1 FROM pg_catalog.pg_namespace WHERE oid='$Q_OID$'::oid AND nspname=$Q_LITERAL$)
    OR pg_catalog.pg_is_in_recovery() OR pg_catalog.current_setting('transaction_isolation')<>'read committed' THEN
    RAISE EXCEPTION 'qualified mutation requires its captured primary family and READ COMMITTED';
  END IF;
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  IF NOT EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mst2_metadata_namespace n
    WHERE n.namespace_uuid='$NAMESPACE_UUID$'::uuid AND n.metadata_schema=$Q_LITERAL$
      AND n.metadata_schema_oid='$Q_OID$'::oid AND n.core_schema_oid=$CORE_OID$
      AND n.metadata_storage_uuid='$STORAGE_UUID$' AND n.admission_state='ROOTED_Q_ADMITTED'
      AND n.collector_state='ENABLED' AND n.implementation_fingerprint=pg_catalog.decode('$IMPLEMENTATION_SHA$','hex')
      AND n.catalog_fingerprint=$CORE_SCHEMA$.mst2_route_family_catalog($CORE_OID$,'$Q_OID$'::oid)) THEN
    RAISE EXCEPTION 'qualified physical family catalog fingerprint is unavailable';
  END IF;
  RETURN NULL;
END $$;


CREATE FUNCTION mst2_metadata_scope_matches(s bytea) RETURNS boolean LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE actual jsonb;
BEGIN
  IF pg_is_in_recovery() OR current_setting('transaction_isolation')<>'read committed' THEN RETURN false; END IF;
  SELECT jsonb_build_array(x.storage_uuid,current_database(),d.oid::bigint,$Q_LITERAL$,'$Q_OID$'::bigint,
    inet_server_addr()::text,inet_server_port()) INTO actual FROM mst2_metadata_storage_scope x
    JOIN pg_database d ON d.datname=current_database() WHERE x.singleton=1;
  RETURN actual IS NOT NULL AND convert_from(s,'UTF8')::jsonb=actual;
EXCEPTION WHEN OTHERS THEN RETURN false;
END $$;
CREATE FUNCTION mst2_metadata_has_generic_overlap(p bytea) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$ SELECT false $$;

CREATE FUNCTION mst2_metadata_lifetime_guard() RETURNS trigger LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'metadata lifetime watermark cannot be deleted'; END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.generation<>1 OR NEW.state<>'RESERVED' OR EXISTS(SELECT 1 FROM mst2_metadata_current WHERE page_id=NEW.page_id) THEN
      RAISE EXCEPTION 'qualified initial lifetime cannot adopt old history; collector is closed';
    END IF;
    RETURN NEW;
  END IF;
  IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state') THEN
    RAISE EXCEPTION 'metadata lifetime identity is immutable';
  END IF;
  IF NEW.state=OLD.state THEN RETURN NEW; END IF;
  IF OLD.state='RESERVED' AND NEW.state='LIVE' THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_node n JOIN mst2_metadata_payload b USING(page_id,generation)
      WHERE n.page_id=OLD.page_id AND n.generation=OLD.generation AND n.state='LIVE'
        AND n.metadata_codec=OLD.metadata_codec AND n.bytes=OLD.expected_size
        AND b.metadata_codec=OLD.metadata_codec AND b.byte_size=OLD.expected_size)
      OR NOT (EXISTS(SELECT 1 FROM mst2_metadata_graph_root r WHERE r.page_id=OLD.page_id AND r.generation=OLD.generation)
        OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page member JOIN mst2_metadata_prepare q USING(prepare_id)
          JOIN mst2_metadata_root_anchor anchor ON anchor.prepare_id=q.prepare_id AND anchor.anchor_kind='PREPARE'
            AND anchor.owner_key=q.prepare_id AND anchor.root_page=q.metadata_root
          WHERE member.page_id=OLD.page_id AND member.generation=OLD.generation AND q.plan_kind='ROOTED'
            AND q.state='COMMITTED' AND q.coverage_retired_at IS NULL)) THEN
      RAISE EXCEPTION 'qualified LIVE transition needs its graph payload and prepare root';
    END IF;
    RETURN NEW;
  END IF;
  RAISE EXCEPTION 'qualified lifetime collection is closed';
END $$;
CREATE TRIGGER mst2_metadata_lifetime_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_lifetime
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_lifetime_guard();
CREATE FUNCTION mst2_metadata_current_guard() RETURNS trigger LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'qualified current collection is closed'; END IF;
  IF NEW.generation<>1 OR EXISTS(SELECT 1 FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation<>1)
    OR EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=NEW.page_id)
    OR EXISTS(SELECT 1 FROM mst2_metadata_graph_node WHERE page_id=NEW.page_id) THEN
    RAISE EXCEPTION 'initial qualified current cannot reset or adopt old history';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_current_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_current
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_current_guard();
CREATE FUNCTION mst2_metadata_current_protected() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF EXISTS(SELECT 1 FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation=NEW.generation AND graph_domain='qualified-v1')
    AND NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
      JOIN mst2_metadata_current c ON c.page_id=m.page_id AND c.generation=m.generation
      WHERE m.page_id=NEW.page_id AND m.generation=NEW.generation AND q.graph_domain='qualified-v1'
        AND q.storage_seal IS NOT NULL AND (q.state='PREPARING' OR (q.state='COMMITTED' AND
          (q.coverage_retired_at IS NULL OR mst2_metadata_session_covers_prepare(q.prepare_id))))) THEN
    RAISE EXCEPTION 'qualified current change must commit with fresh prepare protection';
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_current_protected AFTER INSERT OR UPDATE ON mst2_metadata_current
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_current_protected();

CREATE FUNCTION mst2_metadata_prepare_guard() RETURNS trigger LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified preparation history is immutable'; END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.state<>'PREPARING' OR NEW.committed_at IS NOT NULL OR NEW.aborted_at IS NOT NULL
      OR NEW.coverage_retired_at IS NOT NULL OR NEW.bindings_revision<>0 OR NOT mst2_metadata_scope_matches(NEW.primary_scope)
      OR NEW.manifest_digest IS DISTINCT FROM sha256(NEW.canonical_plan)
      OR NEW.bindings_digest IS DISTINCT FROM sha256(NEW.canonical_bindings) THEN
      RAISE EXCEPTION 'qualified prepare needs its actual fixed primary plan and bindings';
    END IF;
    IF NEW.plan_kind='ROOTED' THEN PERFORM mst2_metadata_rooted_manifest(NEW);
    ELSIF NEW.node_count=0 OR octet_length(NEW.canonical_bindings)<60 THEN
      RAISE EXCEPTION 'cold preparation requires its nonempty full closure';
    END IF;
    RETURN NEW;
  END IF;
  IF (to_jsonb(NEW)-ARRAY['state','committed_at','aborted_at','coverage_retired_at','bindings_revision']) IS DISTINCT FROM
    (to_jsonb(OLD)-ARRAY['state','committed_at','aborted_at','coverage_retired_at','bindings_revision']) THEN
    RAISE EXCEPTION 'qualified preparation complete identity is immutable';
  END IF;
  IF NEW.bindings_revision<>OLD.bindings_revision AND (OLD.plan_kind<>'ROOTED' OR OLD.state<>'PREPARING'
      OR NEW.state<>'PREPARING' OR NEW.bindings_revision<>OLD.bindings_revision+1) THEN
    RAISE EXCEPTION 'rooted membership revision must advance its exact active preparation';
  END IF;
  IF (OLD.state IN ('COMMITTED','ABORTED') AND NEW.state IS DISTINCT FROM OLD.state)
    OR (OLD.committed_at IS NOT NULL AND NEW.committed_at IS DISTINCT FROM OLD.committed_at)
    OR (OLD.aborted_at IS NOT NULL AND NEW.aborted_at IS DISTINCT FROM OLD.aborted_at)
    OR (OLD.coverage_retired_at IS NOT NULL AND NEW.coverage_retired_at IS DISTINCT FROM OLD.coverage_retired_at) THEN
    RAISE EXCEPTION 'qualified terminal receipt cannot be revived or rewritten';
  END IF;
  IF OLD.state='PREPARING' AND NEW.state='COMMITTED' THEN
    IF NEW.plan_kind='ROOTED' THEN
      PERFORM mst2_metadata_rooted_finalize_proof(NEW.prepare_id);
    ELSE
    IF (SELECT count(*) FROM mst2_metadata_prepare_page WHERE prepare_id=NEW.prepare_id)<>NEW.node_count
      OR (SELECT coalesce(sum(expected_size),0) FROM mst2_metadata_prepare_page WHERE prepare_id=NEW.prepare_id)<>NEW.total_bytes
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page WHERE prepare_id=NEW.prepare_id AND page_id=NEW.metadata_root)
      OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m
        LEFT JOIN mst2_metadata_current c USING(page_id,generation)
        LEFT JOIN mst2_metadata_lifetime l USING(page_id,generation)
        LEFT JOIN mst2_metadata_payload b USING(page_id,generation)
        LEFT JOIN mst2_metadata_graph_node n USING(page_id,generation)
        LEFT JOIN mst2_metadata_graph_root r ON r.prepare_id=m.prepare_id AND r.page_id=m.page_id AND r.generation=m.generation
        WHERE m.prepare_id=NEW.prepare_id AND (c.page_id IS NULL OR l.page_id IS NULL OR b.page_id IS NULL OR n.page_id IS NULL
          OR l.state NOT IN ('RESERVED','LIVE') OR n.state<>'LIVE' OR l.metadata_codec<>NEW.metadata_codec
          OR b.metadata_codec<>NEW.metadata_codec OR n.metadata_codec<>NEW.metadata_codec
          OR l.expected_size<>m.expected_size OR b.byte_size<>m.expected_size OR n.bytes<>m.expected_size
          OR r.storage_seal IS DISTINCT FROM NEW.storage_seal
          OR n.incoming_refs<>(SELECT count(*) FROM mst2_metadata_graph_edge WHERE child_page=m.page_id AND child_generation=m.generation)))
      OR (SELECT count(*) FROM mst2_metadata_prepare_page m JOIN mst2_metadata_graph_edge e
        ON e.parent_page=m.page_id AND e.parent_generation=m.generation WHERE m.prepare_id=NEW.prepare_id)<>NEW.edge_count THEN
      RAISE EXCEPTION 'qualified COMMITTED transition lacks its complete exact graph payload and root coverage';
    END IF;
    END IF;
  END IF;
  IF NEW.state='ABORTED' THEN
    IF EXISTS(SELECT 1 FROM mst2_metadata_graph_root WHERE prepare_id=NEW.prepare_id)
      OR EXISTS(SELECT 1 FROM mst2_qualified_session_incarnation WHERE prepare_id=NEW.prepare_id) THEN
      RAISE EXCEPTION 'qualified terminal transition still has exact coverage';
    END IF;
  ELSIF NEW.coverage_retired_at IS NOT NULL AND OLD.coverage_retired_at IS NULL THEN
    IF NEW.plan_kind<>'ROOTED' THEN
      IF EXISTS(SELECT 1 FROM mst2_metadata_graph_root WHERE prepare_id=NEW.prepare_id)
        OR EXISTS(SELECT 1 FROM mst2_qualified_session_incarnation WHERE prepare_id=NEW.prepare_id) THEN
        RAISE EXCEPTION 'cold terminal transition still has exact coverage';
      END IF;
    ELSIF NEW.state<>'COMMITTED' OR EXISTS(SELECT 1 FROM mst2_metadata_graph_root WHERE prepare_id=NEW.prepare_id)
      OR NOT (mst2_metadata_session_covers_prepare(NEW.prepare_id)
        OR mst2_metadata_orphan_prepare_eligible(NEW.prepare_id)) THEN
      RAISE EXCEPTION 'rooted coverage retirement needs its definitive independent ready session root';
    END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_prepare_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_prepare
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_prepare_guard();
CREATE FUNCTION mst2_metadata_generation_mapping_guard() RETURNS trigger LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'metadata generation mappings are immutable'; END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare q JOIN mst2_metadata_current c ON c.page_id=NEW.page_id AND c.generation=NEW.generation
    JOIN mst2_metadata_lifetime l USING(page_id,generation) WHERE q.prepare_id=NEW.prepare_id AND q.state='PREPARING'
      AND q.graph_domain='qualified-v1' AND q.storage_seal IS NOT NULL AND mst2_metadata_scope_matches(q.primary_scope)
      AND l.state IN ('RESERVED','LIVE') AND l.expected_size=NEW.expected_size AND l.metadata_codec=q.metadata_codec) THEN
    RAISE EXCEPTION 'qualified mapping cannot cross domain/current/state fence';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_generation_mapping_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_prepare_page
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_generation_mapping_guard();
CREATE FUNCTION mst2_metadata_payload_fenced() RETURNS trigger LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'qualified payload mutation and collector are closed'; END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l USING(page_id,generation)
    JOIN mst2_metadata_prepare_page m USING(page_id,generation) JOIN mst2_metadata_prepare q USING(prepare_id)
    WHERE c.page_id=NEW.page_id AND c.generation=NEW.generation AND l.state IN ('RESERVED','LIVE')
      AND l.metadata_codec=NEW.metadata_codec AND l.expected_size=NEW.byte_size AND q.state='PREPARING'
      AND q.graph_domain='qualified-v1' AND mst2_metadata_scope_matches(q.primary_scope)) THEN
    RAISE EXCEPTION 'qualified payload INSERT crossed its exact active generation';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_payload_fenced BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_payload
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_fenced();
CREATE FUNCTION mst2_metadata_graph_node_guard() RETURNS trigger LANGUAGE plpgsql
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_metadata_lifetime%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified graph collection is closed'; END IF;
  SELECT l0.* INTO l FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l0 USING(page_id,generation)
    WHERE c.page_id=NEW.page_id AND c.generation=NEW.generation;
  IF NOT FOUND OR l.graph_domain<>'qualified-v1' OR l.metadata_codec<>NEW.metadata_codec OR l.expected_size<>NEW.bytes
    OR NEW.state<>'LIVE' OR l.state NOT IN ('RESERVED','LIVE')
    OR NEW.incoming_refs<>(SELECT count(*) FROM mst2_metadata_graph_edge WHERE child_page=NEW.page_id AND child_generation=NEW.generation) THEN
    RAISE EXCEPTION 'qualified node does not match exact lifetime and actual counters';
  END IF;
  IF TG_OP='INSERT' AND NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
    WHERE m.page_id=NEW.page_id AND m.generation=NEW.generation AND q.state='PREPARING') THEN
    RAISE EXCEPTION 'qualified node creation needs active fixed preparation';
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_page_certificate c WHERE c.page_id=NEW.page_id AND c.generation=NEW.generation
    AND c.certificate_digest=NEW.certificate_digest AND c.metadata_codec=NEW.metadata_codec AND c.byte_size=NEW.bytes) THEN
    RAISE EXCEPTION 'qualified graph node lacks its exact independently canonical certificate';
  END IF;
  IF TG_OP='UPDATE' AND (to_jsonb(NEW)-'incoming_refs') IS DISTINCT FROM (to_jsonb(OLD)-'incoming_refs') THEN
    RAISE EXCEPTION 'qualified node identity is immutable';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_graph_node_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_graph_node
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_graph_node_guard();
CREATE FUNCTION mst2_metadata_graph_root_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'qualified roots cannot be retargeted'; END IF;
  IF TG_OP='DELETE' THEN
    IF EXISTS(SELECT 1 FROM mst2_qualified_session_incarnation WHERE prepare_id=OLD.prepare_id) THEN
      RAISE EXCEPTION 'qualified root still has historical incarnation coverage';
    END IF;
    RETURN OLD;
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare q JOIN mst2_metadata_current c ON c.page_id=NEW.page_id AND c.generation=NEW.generation
    JOIN mst2_metadata_lifetime l USING(page_id,generation) JOIN mst2_metadata_graph_node n USING(page_id,generation)
    WHERE q.prepare_id=NEW.prepare_id AND q.storage_seal=NEW.storage_seal AND q.state='PREPARING'
      AND q.graph_domain='qualified-v1' AND l.graph_domain='qualified-v1' AND l.state IN ('RESERVED','LIVE')
      AND n.state='LIVE' AND mst2_metadata_scope_matches(q.primary_scope)) THEN
    RAISE EXCEPTION 'qualified root requires its active exact sealed preparation';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_graph_root_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_graph_root
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_graph_root_guard();

CREATE FUNCTION mst2_metadata_graph_edge_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'qualified edge identity is immutable'; END IF;
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified edge collection is closed'; END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_verified_ref ref
    JOIN mst2_metadata_page_certificate parent ON parent.page_id=ref.parent_page AND parent.generation=ref.parent_generation
    JOIN mst2_metadata_page_certificate child ON child.page_id=ref.child_page AND child.generation=ref.child_generation
    WHERE ref.parent_page=NEW.parent_page AND ref.parent_generation=NEW.parent_generation
      AND ref.child_page=NEW.child_page AND ref.child_generation=NEW.child_generation
      AND ref.child_certificate_digest=child.certificate_digest AND parent.rank>child.rank) THEN
    RAISE EXCEPTION 'qualified edge differs from its exact canonical reference or strict certified rank';
  END IF;
  IF (SELECT count(*) FROM mst2_metadata_graph_node n JOIN mst2_metadata_current c USING(page_id,generation)
      JOIN mst2_metadata_lifetime l USING(page_id,generation)
      WHERE ((n.page_id=NEW.parent_page AND n.generation=NEW.parent_generation)
        OR (n.page_id=NEW.child_page AND n.generation=NEW.child_generation))
        AND n.state='LIVE' AND l.state IN ('RESERVED','LIVE') AND l.graph_domain='qualified-v1')<>2
    OR NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page a JOIN mst2_metadata_prepare q USING(prepare_id)
      WHERE a.page_id=NEW.parent_page AND a.generation=NEW.parent_generation AND q.state='PREPARING'
        AND q.graph_domain='qualified-v1' AND mst2_metadata_scope_matches(q.primary_scope)
        AND (EXISTS(SELECT 1 FROM mst2_metadata_prepare_page b WHERE b.prepare_id=a.prepare_id
            AND b.page_id=NEW.child_page AND b.generation=NEW.child_generation)
          OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_reuse_root r JOIN mst2_metadata_root_anchor anchor
            ON anchor.prepare_id=r.prepare_id AND anchor.anchor_kind='REUSE' AND anchor.owner_key=r.prepare_id
              AND anchor.root_page=r.root_page AND anchor.root_generation=r.root_generation
            WHERE r.prepare_id=a.prepare_id AND r.root_page=NEW.child_page AND r.root_generation=NEW.child_generation))) THEN
    RAISE EXCEPTION 'edge creation requires active exact qualified endpoints';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_graph_edge_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_graph_edge
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_graph_edge_guard();

CREATE FUNCTION mst2_metadata_edges_added() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF (SELECT count(*) FROM added_edges)>16384 THEN RAISE EXCEPTION 'qualified edge batch exceeds limit'; END IF;
  IF NOT EXISTS(SELECT 1 FROM added_edges) THEN RETURN NULL; END IF;
  IF EXISTS(SELECT 1 FROM mst2_metadata_graph_node n JOIN (
      SELECT child_page,child_generation,count(*) AS delta FROM added_edges GROUP BY child_page,child_generation
    ) d ON d.child_page=n.page_id AND d.child_generation=n.generation
    WHERE n.incoming_refs+d.delta<>(SELECT count(*) FROM mst2_metadata_graph_edge x WHERE x.child_page=n.page_id AND x.child_generation=n.generation)) THEN
    RAISE EXCEPTION 'qualified insertion found preexisting counter drift';
  END IF;
  UPDATE mst2_metadata_graph_node n SET incoming_refs=n.incoming_refs+d.delta FROM (
    SELECT child_page,child_generation,count(*) AS delta FROM added_edges GROUP BY child_page,child_generation
  ) d WHERE n.page_id=d.child_page AND n.generation=d.child_generation;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_edges_added AFTER INSERT ON mst2_metadata_graph_edge
  REFERENCING NEW TABLE AS added_edges FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_edges_added();

DO $$ DECLARE t text; BEGIN
  FOREACH t IN ARRAY ARRAY['mst2_metadata_storage_scope','mst2_metadata_family_identity',
    'mst2_metadata_lifetime','mst2_metadata_current','mst2_metadata_payload','mst2_metadata_prepare',
    'mst2_metadata_prepare_page','mst2_metadata_graph_node','mst2_metadata_graph_edge','mst2_metadata_graph_root',
    'mst2_metadata_gc_op','mst2_qualified_session_incarnation','mst2_qualified_lease_binding',
    'mst2_metadata_page_certificate','mst2_metadata_verified_ref','mst2_metadata_source_root_attestation',
    'mst2_metadata_prepare_reuse_root','mst2_metadata_reuse_index','mst2_metadata_reader_operation','mst2_metadata_reader_issuance','mst2_metadata_root_anchor',
    'mst2_metadata_source_entry_reference','mst2_metadata_scope_source_reference'] LOOP
    EXECUTE format('CREATE TRIGGER mst2_00_family_barrier BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_dml_barrier()',t);
    EXECUTE format('CREATE TRIGGER mst2_metadata_truncate_guard BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_immutable()',t);
  END LOOP;
  FOREACH t IN ARRAY ARRAY['mst2_metadata_storage_scope','mst2_metadata_family_identity'] LOOP
    EXECUTE format('CREATE TRIGGER mst2_metadata_identity_immutable BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_immutable()',t);
  END LOOP;
  FOREACH t IN ARRAY ARRAY['mst2_metadata_gc_op','mst2_qualified_session_incarnation','mst2_qualified_lease_binding',
    'mst2_metadata_reader_operation'] LOOP
    EXECUTE format('CREATE TRIGGER mst2_01_family_closed BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_closed()',t);
  END LOOP;
END $$;

$SERVING_SQL$
$GC_SQL$
