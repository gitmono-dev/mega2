LOCK TABLE mst2_metadata_lifetime,mst2_metadata_current,mst2_metadata_payload,
  mst2_metadata_prepare,mst2_metadata_prepare_page,mst2_retention_node,
  mst2_retention_edge,mst2_retention_root,mst2_retention_gc_op,
  mst2_snapshot_context,mst2_snapshot_lease IN ACCESS EXCLUSIVE MODE;

ALTER TABLE mst2_metadata_lifetime ADD COLUMN graph_domain text NOT NULL DEFAULT 'generic-v1'
  CHECK (graph_domain IN ('generic-v1','qualified-v1'));
ALTER TABLE mst2_metadata_prepare_page ADD UNIQUE(prepare_id,page_id,generation);
ALTER TABLE mst2_metadata_prepare ADD UNIQUE(prepare_id,storage_seal);
CREATE INDEX idx_mst2_snapshot_context_metadata_root ON mst2_snapshot_context(metadata_root,snapshot_id);
CREATE INDEX idx_mst2_metadata_lifetime_qualified_scan ON mst2_metadata_lifetime(page_id,generation)
  WHERE graph_domain='qualified-v1' AND state IN ('RESERVED','LIVE');

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

CREATE FUNCTION mst2_metadata_dml_barrier() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF pg_is_in_recovery() OR current_setting('transaction_isolation')<>'read committed' THEN
    RAISE EXCEPTION 'metadata mutation requires primary READ COMMITTED';
  END IF;
  PERFORM set_config('lock_timeout','5000ms',true);
  PERFORM pg_advisory_xact_lock(1296717362,hashtext(current_schema()));
  RETURN NULL;
END $$;
DO $$ DECLARE t text; BEGIN
  FOREACH t IN ARRAY ARRAY['mst2_metadata_lifetime','mst2_metadata_current','mst2_metadata_payload',
    'mst2_metadata_prepare','mst2_metadata_prepare_page','mst2_metadata_graph_node',
    'mst2_metadata_graph_edge','mst2_metadata_graph_root','mst2_metadata_gc_op',
    'mst2_retention_node','mst2_retention_edge','mst2_retention_root','mst2_retention_gc_op',
    'mst2_snapshot_context','mst2_snapshot_lease'] LOOP
    EXECUTE format('CREATE TRIGGER mst2_metadata_statement_barrier BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_dml_barrier()',t);
  END LOOP;
END $$;

CREATE FUNCTION mst2_metadata_scope_matches(s bytea) RETURNS boolean LANGUAGE plpgsql VOLATILE AS $$
DECLARE actual jsonb;
BEGIN
  IF pg_is_in_recovery() OR current_setting('transaction_isolation')<>'read committed' THEN RETURN false; END IF;
  SELECT jsonb_build_array(x.storage_uuid,current_database(),d.oid::bigint,current_schema(),n.oid::bigint,
      inet_server_addr()::text,inet_server_port()) INTO actual
    FROM mst2_metadata_storage_scope x JOIN pg_database d ON d.datname=current_database()
    JOIN pg_namespace n ON n.nspname=current_schema() WHERE x.singleton=1;
  RETURN actual IS NOT NULL AND convert_from(s,'UTF8')::jsonb=actual;
EXCEPTION WHEN OTHERS THEN RETURN false;
END $$;

CREATE FUNCTION mst2_metadata_assert_uncovered(p bytea,g bigint) RETURNS void LANGUAGE plpgsql VOLATILE AS $$
DECLARE nid text:='page:sha256:'||encode(p,'hex');
BEGIN
  IF EXISTS(SELECT 1 FROM mst2_metadata_graph_root WHERE page_id=p AND generation=g)
    OR EXISTS(SELECT 1 FROM mst2_metadata_graph_edge WHERE child_page=p AND child_generation=g)
    OR EXISTS(SELECT 1 FROM mst2_retention_node WHERE node_id=nid)
    OR EXISTS(SELECT 1 FROM mst2_retention_edge WHERE parent_id=nid OR child_id=nid)
    OR EXISTS(SELECT 1 FROM mst2_retention_root WHERE node_id=nid)
    OR EXISTS(SELECT 1 FROM mst2_retention_gc_op WHERE node_id=nid)
    OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
      WHERE m.page_id=p AND (q.state='PREPARING' OR (q.state='COMMITTED' AND q.coverage_retired_at IS NULL)))
    OR EXISTS(SELECT 1 FROM mst2_snapshot_context WHERE metadata_root=p)
    OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_snapshot_context s USING(prepare_id)
      WHERE m.page_id=p) THEN
    RAISE EXCEPTION 'metadata lifetime has durable or ambiguous coverage';
  END IF;
END $$;

CREATE FUNCTION mst2_metadata_gc_proof(p bytea,g bigint,stage text,graph_present boolean,payload_present boolean)
RETURNS void LANGUAGE plpgsql VOLATILE AS $$
DECLARE l mst2_metadata_lifetime%ROWTYPE; n mst2_metadata_graph_node%ROWTYPE; b mst2_metadata_payload%ROWTYPE;
BEGIN
  SELECT h.* INTO l FROM mst2_metadata_current c JOIN mst2_metadata_lifetime h USING(page_id,generation)
    WHERE c.page_id=p AND c.generation=g FOR UPDATE OF c,h;
  IF NOT FOUND OR l.graph_domain<>'qualified-v1'
    OR (stage='CLAIM' AND l.state NOT IN ('RESERVED','LIVE'))
    OR (stage='PENDING' AND l.state<>'DELETING')
    OR (stage='REMOVED' AND l.state<>'REMOVED')
    OR stage NOT IN ('CLAIM','PENDING','REMOVED') THEN
    RAISE EXCEPTION 'metadata GC is not the exact current qualified lifetime';
  END IF;
  PERFORM mst2_metadata_assert_uncovered(p,g);
  SELECT * INTO n FROM mst2_metadata_graph_node WHERE page_id=p AND generation=g FOR UPDATE;
  IF FOUND IS DISTINCT FROM graph_present THEN RAISE EXCEPTION 'metadata GC graph presence changed'; END IF;
  IF graph_present AND (n.metadata_codec<>l.metadata_codec OR n.bytes<>l.expected_size OR n.incoming_refs<>0
    OR (stage='CLAIM' AND n.state<>'LIVE') OR (stage='PENDING' AND n.state<>'DELETING')) THEN
    RAISE EXCEPTION 'metadata GC graph profile or counter changed';
  END IF;
  IF graph_present AND (SELECT count(*) FROM (SELECT 1 FROM mst2_metadata_graph_edge
    WHERE parent_page=p AND parent_generation=g LIMIT 16385) bounded)>16384 THEN
    RAISE EXCEPTION 'metadata GC outgoing edge limit exceeded';
  END IF;
  SELECT * INTO b FROM mst2_metadata_payload WHERE page_id=p FOR UPDATE;
  IF FOUND IS DISTINCT FROM payload_present THEN RAISE EXCEPTION 'metadata GC payload presence changed'; END IF;
  IF payload_present AND (b.generation IS DISTINCT FROM g OR b.metadata_codec<>l.metadata_codec OR b.byte_size<>l.expected_size) THEN
    RAISE EXCEPTION 'metadata GC payload profile or generation changed';
  END IF;
  IF stage='CLAIM' AND l.state='LIVE' AND NOT (graph_present AND payload_present) THEN
    RAISE EXCEPTION 'LIVE metadata corruption is not collectable';
  END IF;
  IF stage='CLAIM' AND l.state='RESERVED' AND (graph_present OR NOT EXISTS(
      SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
      WHERE m.page_id=p AND m.generation=g AND q.graph_domain='qualified-v1'
        AND q.state='ABORTED' AND q.storage_seal IS NOT NULL AND m.expected_size=l.expected_size)
    OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
      WHERE m.page_id=p AND m.generation=g AND q.state<>'ABORTED')) THEN
    RAISE EXCEPTION 'RESERVED metadata needs an explicitly aborted qualified origin';
  END IF;
END $$;

CREATE FUNCTION mst2_metadata_gc_op_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE l mst2_metadata_lifetime%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'metadata GC evidence cannot be deleted'; END IF;
  IF NOT mst2_metadata_scope_matches(NEW.primary_scope) THEN RAISE EXCEPTION 'metadata GC wrong primary scope'; END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.state<>'PENDING' OR NEW.completed_at IS NOT NULL OR NEW.payload_delete_xid IS NOT NULL THEN RAISE EXCEPTION 'metadata GC must start PENDING'; END IF;
    PERFORM mst2_metadata_gc_proof(NEW.page_id,NEW.generation,'CLAIM',NEW.graph_present,NEW.had_payload);
    SELECT * INTO l FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation=NEW.generation;
    IF NEW.graph_domain<>l.graph_domain OR NEW.metadata_codec<>l.metadata_codec OR NEW.expected_size<>l.expected_size THEN
      RAISE EXCEPTION 'metadata GC immutable profile conflict';
    END IF;
    NEW.created_at:=clock_timestamp();
  ELSE
    IF ROW(NEW.operation_id,NEW.page_id,NEW.generation,NEW.primary_scope,NEW.graph_domain,
      NEW.metadata_codec,NEW.expected_size,NEW.graph_present,NEW.had_payload,NEW.created_at)
      IS DISTINCT FROM ROW(OLD.operation_id,OLD.page_id,OLD.generation,OLD.primary_scope,OLD.graph_domain,
      OLD.metadata_codec,OLD.expected_size,OLD.graph_present,OLD.had_payload,OLD.created_at) THEN
      RAISE EXCEPTION 'metadata GC identity cannot change';
    END IF;
    IF OLD.state='APPLIED' AND (NEW.state<>OLD.state OR NEW.completed_at IS DISTINCT FROM OLD.completed_at
      OR NEW.payload_delete_xid IS DISTINCT FROM OLD.payload_delete_xid) THEN
      RAISE EXCEPTION 'metadata GC receipt cannot change';
    END IF;
    IF OLD.state='PENDING' AND NEW.state='APPLIED' THEN
      IF OLD.had_payload AND OLD.payload_delete_xid IS DISTINCT FROM txid_current() THEN
        RAISE EXCEPTION 'metadata GC has no same-transaction payload delete proof';
      END IF;
      PERFORM mst2_metadata_gc_proof(NEW.page_id,NEW.generation,'REMOVED',false,false);
      NEW.completed_at:=clock_timestamp();
    ELSIF NEW.state IS DISTINCT FROM OLD.state OR NEW.completed_at IS DISTINCT FROM OLD.completed_at THEN
      RAISE EXCEPTION 'metadata GC invalid receipt transition';
    END IF;
    IF NEW.payload_delete_xid IS DISTINCT FROM OLD.payload_delete_xid THEN
      IF OLD.state<>'PENDING' OR NEW.state<>'PENDING' OR NOT OLD.had_payload THEN
        RAISE EXCEPTION 'invalid metadata delete transaction proof';
      END IF;
      PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'PENDING',OLD.graph_present,true);
      NEW.payload_delete_xid:=txid_current();
    END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_gc_op_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_gc_op
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_gc_op_guard();

CREATE OR REPLACE FUNCTION mst2_metadata_lifetime_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'metadata lifetime watermark cannot be deleted'; END IF;
  IF ROW(NEW.page_id,NEW.node_id,NEW.generation,NEW.metadata_codec,NEW.expected_size,NEW.graph_domain)
    IS DISTINCT FROM ROW(OLD.page_id,OLD.node_id,OLD.generation,OLD.metadata_codec,OLD.expected_size,OLD.graph_domain) THEN
    RAISE EXCEPTION 'metadata lifetime identity is immutable';
  END IF;
  IF NEW.state=OLD.state THEN RETURN NEW; END IF;
  IF OLD.graph_domain='generic-v1' THEN
    IF OLD.state='RESERVED' AND NEW.state='LIVE' THEN RETURN NEW; END IF;
    RAISE EXCEPTION 'generic lifetime transition requires original collector';
  END IF;
  IF OLD.state='RESERVED' AND NEW.state='LIVE' THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_node n JOIN mst2_metadata_payload b USING(page_id,generation)
      WHERE n.page_id=OLD.page_id AND n.generation=OLD.generation AND n.state='LIVE'
        AND n.metadata_codec=OLD.metadata_codec AND n.bytes=OLD.expected_size
        AND b.metadata_codec=OLD.metadata_codec AND b.byte_size=OLD.expected_size)
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_root r WHERE r.page_id=OLD.page_id AND r.generation=OLD.generation) THEN
      RAISE EXCEPTION 'qualified LIVE transition needs its graph payload and prepare root';
    END IF;
    RETURN NEW;
  END IF;
  SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='PENDING';
  IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN RAISE EXCEPTION 'metadata transition has no exact GC op'; END IF;
  IF OLD.state IN ('RESERVED','LIVE') AND NEW.state='DELETING' THEN
    PERFORM mst2_metadata_assert_uncovered(OLD.page_id,OLD.generation);
    RETURN NEW;
  END IF;
  IF OLD.state='DELETING' AND NEW.state='REMOVED' THEN
    IF op.had_payload AND op.payload_delete_xid IS DISTINCT FROM txid_current() THEN
      RAISE EXCEPTION 'metadata removal has no same-transaction payload delete proof';
    END IF;
    PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'PENDING',false,false);
    RETURN NEW;
  END IF;
  RAISE EXCEPTION 'metadata lifetime invalid state transition';
END $$;

CREATE FUNCTION mst2_metadata_lifetime_removed() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF OLD.state='DELETING' AND NEW.state='REMOVED' THEN
    UPDATE mst2_metadata_gc_op SET state='APPLIED'
      WHERE page_id=NEW.page_id AND generation=NEW.generation AND state='PENDING';
    IF NOT FOUND THEN RAISE EXCEPTION 'metadata removal lost its GC operation'; END IF;
  END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_lifetime_removed AFTER UPDATE ON mst2_metadata_lifetime
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_lifetime_removed();

CREATE OR REPLACE FUNCTION mst2_metadata_current_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE; l mst2_metadata_lifetime%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'metadata current watermark cannot be deleted'; END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.generation<>1 OR EXISTS(SELECT 1 FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation<>1) THEN
      RAISE EXCEPTION 'initial metadata current cannot reset history';
    END IF;
    SELECT * INTO l FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation=NEW.generation;
    IF l.graph_domain='qualified-v1' THEN
      PERFORM mst2_metadata_assert_uncovered(NEW.page_id,NEW.generation);
      IF EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=NEW.page_id)
        OR EXISTS(SELECT 1 FROM mst2_metadata_graph_node WHERE page_id=NEW.page_id) THEN
        RAISE EXCEPTION 'initial qualified current cannot adopt old bytes or graph';
      END IF;
    END IF;
    RETURN NEW;
  END IF;
  IF NEW.page_id<>OLD.page_id OR OLD.generation=9223372036854775807 OR NEW.generation<>OLD.generation+1 THEN
    RAISE EXCEPTION 'metadata current requires exact next-generation CAS';
  END IF;
  SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='APPLIED';
  IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN RAISE EXCEPTION 'fresh metadata requires exact APPLIED proof'; END IF;
  PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'REMOVED',false,false);
  SELECT * INTO l FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation=NEW.generation;
  IF NOT FOUND OR l.state<>'RESERVED' OR l.graph_domain<>'qualified-v1'
    OR l.metadata_codec<>op.metadata_codec OR l.expected_size<>op.expected_size THEN
    RAISE EXCEPTION 'fresh metadata incarnation profile mismatch';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_current_insert_guard BEFORE INSERT ON mst2_metadata_current
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_current_guard();

CREATE FUNCTION mst2_metadata_current_protected() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF EXISTS(SELECT 1 FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation=NEW.generation AND graph_domain='qualified-v1')
    AND NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
      JOIN mst2_metadata_current c ON c.page_id=m.page_id AND c.generation=m.generation
      WHERE m.page_id=NEW.page_id AND m.generation=NEW.generation AND q.graph_domain='qualified-v1'
        AND q.storage_seal IS NOT NULL AND (q.state='PREPARING' OR (q.state='COMMITTED' AND q.coverage_retired_at IS NULL))) THEN
    RAISE EXCEPTION 'qualified current change must commit with fresh prepare protection';
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_current_protected AFTER INSERT OR UPDATE ON mst2_metadata_current
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_current_protected();

CREATE FUNCTION mst2_metadata_lifetime_insert_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE c mst2_metadata_current%ROWTYPE; old_owner text; op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  SELECT * INTO c FROM mst2_metadata_current WHERE page_id=NEW.page_id;
  IF FOUND THEN
    SELECT graph_domain INTO old_owner FROM mst2_metadata_lifetime WHERE page_id=c.page_id AND generation=c.generation;
    IF old_owner IS DISTINCT FROM NEW.graph_domain THEN RAISE EXCEPTION 'metadata incarnation domain cannot be adopted'; END IF;
    IF NEW.graph_domain='qualified-v1' AND NEW.generation<>c.generation THEN
      SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=c.page_id AND generation=c.generation AND state='APPLIED';
      IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope) OR c.generation=9223372036854775807
        OR NEW.generation<>c.generation+1 OR NEW.state<>'RESERVED'
        OR NEW.metadata_codec<>op.metadata_codec OR NEW.expected_size<>op.expected_size THEN
        RAISE EXCEPTION 'fresh historical incarnation needs exact removal proof';
      END IF;
      PERFORM mst2_metadata_gc_proof(c.page_id,c.generation,'REMOVED',false,false);
    END IF;
  ELSIF NEW.graph_domain='qualified-v1' AND (NEW.generation<>1 OR NEW.state<>'RESERVED'
    OR EXISTS(SELECT 1 FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND graph_domain<>NEW.graph_domain)) THEN
    RAISE EXCEPTION 'initial qualified incarnation cannot adopt old history';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_lifetime_insert_guard BEFORE INSERT ON mst2_metadata_lifetime
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_lifetime_insert_guard();

CREATE FUNCTION mst2_metadata_qualified_mapping_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE owner text; domain text; st text;
BEGIN
  IF TG_OP<>'INSERT' THEN RETURN NEW; END IF;
  SELECT l.graph_domain INTO owner FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l USING(page_id,generation)
    WHERE c.page_id=NEW.page_id;
  SELECT graph_domain INTO domain FROM mst2_metadata_prepare WHERE prepare_id=NEW.prepare_id;
  IF owner='qualified-v1' OR domain='qualified-v1' THEN
    SELECT l.state INTO st FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l USING(page_id,generation)
      WHERE c.page_id=NEW.page_id AND c.generation=NEW.generation AND l.graph_domain='qualified-v1';
    IF owner IS DISTINCT FROM domain OR st IS NULL OR st NOT IN ('RESERVED','LIVE')
      OR EXISTS(SELECT 1 FROM mst2_metadata_gc_op WHERE page_id=NEW.page_id AND generation=NEW.generation)
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare q WHERE q.prepare_id=NEW.prepare_id
        AND q.state='PREPARING' AND q.storage_seal IS NOT NULL AND mst2_metadata_scope_matches(q.primary_scope)) THEN
      RAISE EXCEPTION 'qualified mapping cannot cross domain/current/state fence';
    END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_qualified_mapping_guard BEFORE INSERT ON mst2_metadata_prepare_page
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_qualified_mapping_guard();

CREATE FUNCTION mst2_metadata_generic_domain_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE record jsonb; ident text; key text;
BEGIN
  FOREACH key IN ARRAY ARRAY['node_id','parent_id','child_id'] LOOP
    IF TG_OP<>'INSERT' THEN
      record:=to_jsonb(OLD); ident:=record->>key;
      IF EXISTS(SELECT 1 FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l USING(page_id,generation)
        WHERE l.graph_domain='qualified-v1' AND l.node_id=ident) THEN RAISE EXCEPTION 'generic graph cannot touch qualified incarnation'; END IF;
    END IF;
    IF TG_OP<>'DELETE' THEN
      record:=to_jsonb(NEW); ident:=record->>key;
      IF EXISTS(SELECT 1 FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l USING(page_id,generation)
        WHERE l.graph_domain='qualified-v1' AND l.node_id=ident) THEN RAISE EXCEPTION 'generic graph cannot adopt qualified incarnation'; END IF;
    END IF;
  END LOOP;
  IF TG_OP='DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;
DO $$ DECLARE t text; BEGIN
  FOREACH t IN ARRAY ARRAY['mst2_retention_node','mst2_retention_edge','mst2_retention_root','mst2_retention_gc_op'] LOOP
    EXECUTE format('CREATE TRIGGER mst2_metadata_generic_domain_guard BEFORE INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION mst2_metadata_generic_domain_guard()',t);
  END LOOP;
END $$;

CREATE FUNCTION mst2_metadata_session_domain_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE sid text; pid text;
BEGIN
  IF TG_OP='DELETE' THEN RETURN OLD; END IF;
  IF TG_TABLE_NAME='mst2_snapshot_context' THEN pid:=NEW.prepare_id;
  ELSE sid:=NEW.snapshot_id; SELECT prepare_id INTO pid FROM mst2_snapshot_context WHERE snapshot_id=sid; END IF;
  IF EXISTS(SELECT 1 FROM mst2_metadata_prepare WHERE prepare_id=pid AND graph_domain='qualified-v1') THEN
    RAISE EXCEPTION 'qualified production session adoption is closed';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_session_domain_guard BEFORE INSERT OR UPDATE ON mst2_snapshot_context
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_session_domain_guard();
CREATE TRIGGER mst2_metadata_session_domain_guard BEFORE INSERT OR UPDATE ON mst2_snapshot_lease
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_session_domain_guard();

CREATE FUNCTION mst2_metadata_graph_node_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE l mst2_metadata_lifetime%ROWTYPE; op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN
    SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='PENDING';
    IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope) OR OLD.state<>'DELETING' THEN
      RAISE EXCEPTION 'qualified node deletion needs exact pending operation';
    END IF;
    IF op.had_payload AND op.payload_delete_xid IS DISTINCT FROM txid_current() THEN RAISE EXCEPTION 'node deletion has no payload delete proof'; END IF;
    PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'PENDING',true,false);
    IF EXISTS(SELECT 1 FROM mst2_metadata_graph_edge WHERE parent_page=OLD.page_id AND parent_generation=OLD.generation) THEN
      RAISE EXCEPTION 'qualified node still has outgoing edges';
    END IF;
    RETURN OLD;
  END IF;
  SELECT l0.* INTO l FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l0 USING(page_id,generation)
    WHERE c.page_id=NEW.page_id AND c.generation=NEW.generation;
  IF NOT FOUND OR l.graph_domain<>'qualified-v1' OR l.metadata_codec<>NEW.metadata_codec OR l.expected_size<>NEW.bytes THEN
    RAISE EXCEPTION 'qualified node does not match exact lifetime';
  END IF;
  IF NEW.incoming_refs<>(SELECT count(*) FROM mst2_metadata_graph_edge WHERE child_page=NEW.page_id AND child_generation=NEW.generation) THEN
    RAISE EXCEPTION 'qualified node counter differs from actual unique edges';
  END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.state<>'LIVE' OR l.state NOT IN ('RESERVED','LIVE') OR NOT EXISTS(
      SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
      WHERE m.page_id=NEW.page_id AND m.generation=NEW.generation AND q.graph_domain='qualified-v1' AND q.state='PREPARING') THEN
      RAISE EXCEPTION 'qualified node creation needs active fixed preparation';
    END IF;
  ELSE
    IF ROW(NEW.page_id,NEW.generation,NEW.metadata_codec,NEW.bytes) IS DISTINCT FROM ROW(OLD.page_id,OLD.generation,OLD.metadata_codec,OLD.bytes) THEN
      RAISE EXCEPTION 'qualified node identity is immutable';
    END IF;
    IF NEW.state<>OLD.state AND NOT (OLD.state='LIVE' AND NEW.state='DELETING' AND EXISTS(
      SELECT 1 FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='PENDING')) THEN
      RAISE EXCEPTION 'qualified node invalid state transition';
    END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_graph_node_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_graph_node
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_graph_node_guard();

CREATE FUNCTION mst2_metadata_graph_root_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'qualified roots cannot be retargeted'; END IF;
  IF TG_OP='DELETE' THEN RETURN OLD; END IF;
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

CREATE FUNCTION mst2_metadata_graph_edge_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'qualified edge identity is immutable'; END IF;
  IF TG_OP='DELETE' THEN
    SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.parent_page AND generation=OLD.parent_generation AND state='PENDING';
    IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN RAISE EXCEPTION 'edge deletion has no exact operation'; END IF;
    IF op.had_payload AND op.payload_delete_xid IS DISTINCT FROM txid_current() THEN RAISE EXCEPTION 'edge deletion has no payload delete proof'; END IF;
    IF EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=OLD.parent_page)
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_node WHERE page_id=OLD.parent_page
        AND generation=OLD.parent_generation AND state='DELETING' AND incoming_refs=0) THEN
      RAISE EXCEPTION 'edge deletion is outside its atomic byte-removal phase';
    END IF;
    RETURN OLD;
  END IF;
  IF (SELECT count(*) FROM mst2_metadata_graph_node n JOIN mst2_metadata_current c USING(page_id,generation)
      JOIN mst2_metadata_lifetime l USING(page_id,generation)
      WHERE ((n.page_id=NEW.parent_page AND n.generation=NEW.parent_generation)
        OR (n.page_id=NEW.child_page AND n.generation=NEW.child_generation))
        AND n.state='LIVE' AND l.state IN ('RESERVED','LIVE') AND l.graph_domain='qualified-v1')<>2
    OR NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page a JOIN mst2_metadata_prepare_page b USING(prepare_id)
      JOIN mst2_metadata_prepare q USING(prepare_id) WHERE a.page_id=NEW.parent_page AND a.generation=NEW.parent_generation
        AND b.page_id=NEW.child_page AND b.generation=NEW.child_generation AND q.state='PREPARING' AND q.graph_domain='qualified-v1') THEN
    RAISE EXCEPTION 'edge creation requires active exact qualified endpoints';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_graph_edge_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_graph_edge
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_graph_edge_guard();

CREATE FUNCTION mst2_metadata_edges_added() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE reachable text[]; links jsonb; adjacency jsonb; indegrees bigint[];
  queue integer[]:=ARRAY[]::integer[]; head integer:=1; parent_index integer; child_index integer; i integer;
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
  -- One bounded closure per statement; topological accounting below is local.
  WITH RECURSIVE walk(page_id,generation) AS (
    SELECT page_id,generation FROM (
      SELECT parent_page AS page_id,parent_generation AS generation FROM added_edges
      UNION SELECT child_page,child_generation FROM added_edges
    ) starts UNION
    SELECT x.child_page,x.child_generation FROM walk w JOIN mst2_metadata_graph_edge x
      ON x.parent_page=w.page_id AND x.parent_generation=w.generation
  ) SELECT array_agg(encode(page_id,'hex')||':'||generation ORDER BY page_id,generation)
      INTO reachable FROM (SELECT * FROM walk LIMIT 4097) bounded;
  IF cardinality(reachable)>4096 THEN RAISE EXCEPTION 'qualified graph node audit overflow'; END IF;
  WITH nodes AS (
    SELECT ordinality::integer AS i,decode(split_part(key,':',1),'hex') AS page_id,
      split_part(key,':',2)::bigint AS generation FROM unnest(reachable) WITH ORDINALITY n(key,ordinality)
  ) SELECT coalesce(jsonb_agg(jsonb_build_array(p.i,c.i)),'[]'::jsonb) INTO links FROM (
      SELECT e.* FROM nodes p JOIN mst2_metadata_graph_edge e
        ON e.parent_page=p.page_id AND e.parent_generation=p.generation LIMIT 16385
    ) e JOIN nodes p ON p.page_id=e.parent_page AND p.generation=e.parent_generation
      JOIN nodes c ON c.page_id=e.child_page AND c.generation=e.child_generation;
  IF jsonb_array_length(links)>16384 THEN RAISE EXCEPTION 'qualified graph edge audit overflow'; END IF;
  SELECT coalesce(jsonb_object_agg(parent,children),'{}'::jsonb) INTO adjacency FROM (
    SELECT item->>0 AS parent,jsonb_agg((item->>1)::integer) AS children
      FROM jsonb_array_elements(links) x(item) GROUP BY item->>0
  ) grouped;
  SELECT array_agg(coalesce(counts.refs,0) ORDER BY n.i) INTO indegrees
    FROM generate_series(1,cardinality(reachable)) n(i) LEFT JOIN (
      SELECT (item->>1)::integer AS child,count(*) AS refs FROM jsonb_array_elements(links) x(item) GROUP BY item->>1
    ) counts ON counts.child=n.i;
  FOR i IN 1..cardinality(reachable) LOOP
    IF indegrees[i]=0 THEN queue:=array_append(queue,i); END IF;
  END LOOP;
  WHILE head<=cardinality(queue) LOOP
    parent_index:=queue[head]; head:=head+1;
    FOR child_index IN SELECT value::text::integer FROM jsonb_array_elements(coalesce(adjacency->parent_index::text,'[]'::jsonb)) LOOP
      indegrees[child_index]:=indegrees[child_index]-1;
      IF indegrees[child_index]=0 THEN queue:=array_append(queue,child_index); END IF;
    END LOOP;
  END LOOP;
  IF cardinality(queue)<>cardinality(reachable) THEN RAISE EXCEPTION 'qualified graph contains a cycle'; END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_edges_added AFTER INSERT ON mst2_metadata_graph_edge
  REFERENCING NEW TABLE AS added_edges FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_edges_added();
CREATE FUNCTION mst2_metadata_edges_removed() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  IF EXISTS(SELECT 1 FROM mst2_metadata_graph_node n JOIN (
      SELECT child_page,child_generation,count(*) AS delta FROM removed_edges GROUP BY child_page,child_generation
    ) d ON d.child_page=n.page_id AND d.child_generation=n.generation
    WHERE n.incoming_refs<d.delta OR n.incoming_refs-d.delta<>(SELECT count(*) FROM mst2_metadata_graph_edge x WHERE x.child_page=n.page_id AND x.child_generation=n.generation)) THEN
    RAISE EXCEPTION 'qualified subtraction found counter drift';
  END IF;
  UPDATE mst2_metadata_graph_node n SET incoming_refs=n.incoming_refs-d.delta FROM (
    SELECT child_page,child_generation,count(*) AS delta FROM removed_edges GROUP BY child_page,child_generation
  ) d WHERE n.page_id=d.child_page AND n.generation=d.child_generation;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_edges_removed AFTER DELETE ON mst2_metadata_graph_edge
  REFERENCING OLD TABLE AS removed_edges FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_edges_removed();

CREATE FUNCTION mst2_metadata_gc_claimed() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
BEGIN
  UPDATE mst2_metadata_graph_node SET state='DELETING' WHERE page_id=NEW.page_id AND generation=NEW.generation;
  UPDATE mst2_metadata_lifetime SET state='DELETING' WHERE page_id=NEW.page_id AND generation=NEW.generation;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_gc_claimed AFTER INSERT ON mst2_metadata_gc_op
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_gc_claimed();

CREATE FUNCTION mst2_metadata_gc_finish(id uuid) RETURNS void LANGUAGE plpgsql VOLATILE AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF pg_is_in_recovery() OR current_setting('transaction_isolation')<>'read committed' THEN RAISE EXCEPTION 'GC finish needs primary READ COMMITTED'; END IF;
  PERFORM set_config('lock_timeout','5000ms',true);
  PERFORM pg_advisory_xact_lock(1296717362,hashtext(current_schema()));
  SELECT * INTO op FROM mst2_metadata_gc_op WHERE operation_id=id FOR UPDATE;
  IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN RAISE EXCEPTION 'GC finish wrong exact operation scope'; END IF;
  IF op.state='APPLIED' THEN RETURN; END IF;
  IF op.had_payload AND op.payload_delete_xid IS DISTINCT FROM txid_current() THEN
    RAISE EXCEPTION 'GC finish has no same-transaction payload delete proof';
  END IF;
  PERFORM mst2_metadata_gc_proof(op.page_id,op.generation,'PENDING',op.graph_present,false);
  DELETE FROM mst2_metadata_graph_edge WHERE parent_page=op.page_id AND parent_generation=op.generation;
  DELETE FROM mst2_metadata_graph_node WHERE page_id=op.page_id AND generation=op.generation;
  UPDATE mst2_metadata_lifetime SET state='REMOVED' WHERE page_id=op.page_id AND generation=op.generation AND state='DELETING';
  IF NOT FOUND THEN RAISE EXCEPTION 'GC finish lost exact lifetime'; END IF;
END $$;

CREATE FUNCTION mst2_metadata_payload_fenced() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE l mst2_metadata_lifetime%ROWTYPE; op mst2_metadata_gc_op%ROWTYPE; selector text;
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'MST2 metadata payload UPDATE is forbidden'; END IF;
  IF TG_OP='INSERT' THEN
    SELECT h.* INTO l FROM mst2_metadata_current c JOIN mst2_metadata_lifetime h USING(page_id,generation) WHERE c.page_id=NEW.page_id;
    IF FOUND AND l.graph_domain='qualified-v1' THEN
      IF NEW.generation IS DISTINCT FROM l.generation OR l.state NOT IN ('RESERVED','LIVE')
        OR NEW.metadata_codec<>l.metadata_codec OR NEW.byte_size<>l.expected_size
        OR NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
          WHERE m.page_id=NEW.page_id AND m.generation=NEW.generation AND q.state='PREPARING'
            AND q.graph_domain='qualified-v1' AND q.storage_seal IS NOT NULL AND mst2_metadata_scope_matches(q.primary_scope)) THEN
        RAISE EXCEPTION 'qualified payload INSERT crossed its exact active generation';
      END IF;
    END IF;
    RETURN NEW;
  END IF;
  selector:=current_setting('mega2.metadata_gc_operation',true);
  IF OLD.generation IS NULL OR selector IS NULL OR selector='' THEN RAISE EXCEPTION 'payload DELETE has no qualified operation selector'; END IF;
  SELECT * INTO op FROM mst2_metadata_gc_op WHERE operation_id=selector::uuid AND page_id=OLD.page_id AND generation=OLD.generation FOR UPDATE;
  IF NOT FOUND OR op.state<>'PENDING' OR NOT op.had_payload OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN
    RAISE EXCEPTION 'payload DELETE requires exact persistent pending proof';
  END IF;
  PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'PENDING',op.graph_present,true);
  UPDATE mst2_metadata_gc_op SET payload_delete_xid=txid_current() WHERE operation_id=op.operation_id;
  RETURN OLD;
END $$;
DROP TRIGGER mst2_metadata_payload_immutable ON mst2_metadata_payload;
CREATE TRIGGER mst2_metadata_payload_fenced BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_payload
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_fenced();
CREATE FUNCTION mst2_metadata_payload_removed() RETURNS trigger LANGUAGE plpgsql VOLATILE AS $$
DECLARE op uuid;
BEGIN
  SELECT operation_id INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='PENDING';
  IF NOT FOUND THEN RAISE EXCEPTION 'deleted payload lost exact pending operation'; END IF;
  PERFORM mst2_metadata_gc_finish(op);
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_payload_removed AFTER DELETE ON mst2_metadata_payload
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_removed();

CREATE FUNCTION mst2_metadata_gc_apply(id uuid) RETURNS void LANGUAGE plpgsql VOLATILE AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF pg_is_in_recovery() OR current_setting('transaction_isolation')<>'read committed' THEN RAISE EXCEPTION 'GC apply needs primary READ COMMITTED'; END IF;
  PERFORM set_config('lock_timeout','5000ms',true);
  PERFORM pg_advisory_xact_lock(1296717362,hashtext(current_schema()));
  SELECT * INTO op FROM mst2_metadata_gc_op WHERE operation_id=id FOR UPDATE;
  IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN RAISE EXCEPTION 'GC apply missing exact scope'; END IF;
  IF op.state='APPLIED' THEN RETURN; END IF;
  PERFORM mst2_metadata_gc_proof(op.page_id,op.generation,'PENDING',op.graph_present,op.had_payload);
  IF op.had_payload THEN
    PERFORM set_config('mega2.metadata_gc_operation',op.operation_id::text,true);
    DELETE FROM mst2_metadata_payload WHERE page_id=op.page_id AND generation=op.generation;
    IF NOT FOUND THEN RAISE EXCEPTION 'GC apply lost captured payload'; END IF;
  ELSE
    PERFORM mst2_metadata_gc_finish(id);
  END IF;
END $$;
