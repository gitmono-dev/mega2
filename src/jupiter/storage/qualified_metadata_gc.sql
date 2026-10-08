-- Historical certificates and source/session records do not own live graph rows.
ALTER TABLE mst2_metadata_gc_op ADD COLUMN certificate_digest bytea,
  ADD CONSTRAINT mst2_gc_certificate_binding CHECK(
    (graph_present AND octet_length(certificate_digest)=32 OR NOT graph_present AND certificate_digest IS NULL) IS TRUE);
ALTER TABLE mst2_metadata_prepare ADD COLUMN orphan_expires_at timestamptz NOT NULL;
CREATE INDEX mst2_metadata_prepare_orphan_scan ON mst2_metadata_prepare(orphan_expires_at,prepare_id)
  WHERE state='PREPARING' OR state='COMMITTED' AND coverage_retired_at IS NULL;
CREATE INDEX mst2_metadata_current_active_scan ON mst2_metadata_lifetime(page_id,generation)
  WHERE state IN ('RESERVED','LIVE');

CREATE FUNCTION mst2_metadata_prepare_expiry_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP='INSERT' THEN NEW.created_at:=clock_timestamp(); NEW.orphan_expires_at:=NEW.created_at+interval '3600 seconds';
  ELSIF NEW.created_at IS DISTINCT FROM OLD.created_at OR NEW.orphan_expires_at IS DISTINCT FROM OLD.orphan_expires_at THEN
    RAISE EXCEPTION 'qualified preparation expiry is immutable database evidence'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_prepare_00_expiry_guard BEFORE INSERT OR UPDATE ON mst2_metadata_prepare
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_prepare_expiry_guard();

CREATE FUNCTION mst2_metadata_orphan_prepare_eligible(pid text) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM mst2_metadata_prepare q WHERE q.prepare_id=$1 AND q.state='COMMITTED'
    AND q.plan_kind='ROOTED' AND q.graph_domain='qualified-v1' AND mst2_metadata_scope_matches(q.primary_scope)
    AND q.orphan_expires_at=q.created_at+interval '3600 seconds' AND q.orphan_expires_at<=clock_timestamp()
    AND NOT EXISTS(SELECT 1 FROM mst2_qualified_session_incarnation s WHERE s.state='READY'
      AND (s.prepare_id=q.prepare_id OR s.namespace_uuid='$NAMESPACE_UUID$'::uuid AND s.metadata_root=q.metadata_root
        AND s.source_profile=convert_to($CORE_SCHEMA$.mst2_route_profile(q.source_domain,q.tagged_root_tree_oid,q.scope,
          q.schema_version,q.metadata_codec,q.materialization_policy,q.fs_semantics,q.access_projection,
          q.verification_revision,q.projection_revision)::text,'UTF8')
        AND s.root_generation IN (SELECT generation FROM mst2_metadata_prepare_page m
          WHERE m.prepare_id=q.prepare_id AND m.page_id=q.metadata_root UNION ALL
          SELECT root_generation FROM mst2_metadata_prepare_reuse_root r WHERE r.prepare_id=q.prepare_id AND r.root_page=q.metadata_root)))
    AND NOT EXISTS(SELECT 1 FROM mst2_qualified_lease_binding l WHERE l.prepare_id=q.prepare_id AND l.state='ACTIVE')
    AND NOT EXISTS(SELECT 1 FROM mst2_metadata_reader_operation r JOIN mst2_qualified_lease_binding l USING(lease_id)
      WHERE l.prepare_id=q.prepare_id AND r.state='ACTIVE'))
$$;
CREATE FUNCTION mst2_metadata_orphan_prepare_retired(pid text) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT mst2_metadata_orphan_prepare_eligible($1) AND EXISTS(SELECT 1 FROM mst2_metadata_prepare q
    WHERE q.prepare_id=$1 AND q.coverage_retired_at>=q.orphan_expires_at)
    AND NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor WHERE prepare_id=$1 AND anchor_kind IN ('PREPARE','REUSE'))
$$;

CREATE FUNCTION mst2_metadata_gc_enabled() RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT NOT pg_is_in_recovery() AND current_setting('transaction_isolation')='read committed'
    AND EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mst2_metadata_namespace n WHERE n.namespace_uuid='$NAMESPACE_UUID$'::uuid
      AND n.graph_domain='qualified-v1' AND n.metadata_schema=$Q_LITERAL$ AND n.metadata_schema_oid='$Q_OID$'::oid
      AND n.core_schema_oid=$CORE_OID$ AND n.metadata_storage_uuid='$STORAGE_UUID$'
      AND n.admission_state='ROOTED_Q_ADMITTED' AND n.collector_state='ENABLED'
      AND n.implementation_fingerprint=decode('$IMPLEMENTATION_SHA$','hex')
      AND n.catalog_fingerprint=$CORE_SCHEMA$.mst2_route_family_catalog($CORE_OID$,'$Q_OID$'::oid))
$$;
CREATE FUNCTION mst2_metadata_gc_enter() RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  IF NOT mst2_metadata_gc_enabled() THEN RAISE EXCEPTION 'qualified collector is not independently admitted'; END IF;
END $$;

CREATE FUNCTION mst2_metadata_assert_uncovered(p bytea,g bigint) RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF EXISTS(SELECT 1 FROM mst2_metadata_root_anchor WHERE root_page=p AND root_generation=g)
    OR EXISTS(SELECT 1 FROM mst2_metadata_graph_root WHERE page_id=p AND generation=g)
    OR EXISTS(SELECT 1 FROM mst2_metadata_graph_edge WHERE child_page=p AND child_generation=g)
    OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
      WHERE m.page_id=p AND m.generation=g AND (q.state='PREPARING' OR q.state='COMMITTED' AND q.coverage_retired_at IS NULL))
    OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_reuse_root r JOIN mst2_metadata_prepare q USING(prepare_id)
      WHERE r.root_page=p AND r.root_generation=g AND (q.state='PREPARING' OR q.state='COMMITTED' AND q.coverage_retired_at IS NULL)) THEN
    RAISE EXCEPTION 'qualified current lifetime still has exact owned coverage or incoming edges'; END IF;
END $$;

CREATE FUNCTION mst2_metadata_gc_proof(p bytea,g bigint,stage text,graph_present boolean,payload_present boolean,c bytea)
RETURNS void LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE life mst2_metadata_lifetime%ROWTYPE; node mst2_metadata_graph_node%ROWTYPE;
  body mst2_metadata_payload%ROWTYPE; certificate mst2_metadata_page_certificate%ROWTYPE; actual boolean;
BEGIN
  IF p IS NULL OR octet_length(p)<>32 OR g IS NULL OR g<=0 OR stage IS NULL
    OR graph_present IS NULL OR payload_present IS NULL OR graph_present AND c IS NULL THEN
    RAISE EXCEPTION 'qualified GC proof has an incomplete exact identity'; END IF;
  SELECT * INTO life FROM mst2_metadata_lifetime WHERE page_id=p AND generation=g FOR UPDATE;
  IF NOT FOUND OR life.graph_domain<>'qualified-v1' OR life.metadata_codec<>1 OR stage NOT IN ('CLAIM','PENDING','APPLIED')
    OR stage='CLAIM' AND life.state NOT IN ('RESERVED','LIVE') OR stage='PENDING' AND life.state<>'DELETING'
    OR stage='APPLIED' AND life.state<>'REMOVED' THEN RAISE EXCEPTION 'qualified GC stage is not its exact historical lifetime'; END IF;
  IF stage<>'APPLIED' AND NOT EXISTS(SELECT 1 FROM mst2_metadata_current WHERE page_id=p AND generation=g) THEN
    RAISE EXCEPTION 'qualified GC does not name its exact current generation'; END IF;
  PERFORM mst2_metadata_assert_uncovered(p,g);
  SELECT * INTO node FROM mst2_metadata_graph_node WHERE page_id=p AND generation=g FOR UPDATE;
  actual:=FOUND;
  IF actual IS DISTINCT FROM graph_present OR actual AND (node.incoming_refs<>0 OR node.metadata_codec<>life.metadata_codec
    OR node.bytes<>life.expected_size OR node.certificate_digest IS DISTINCT FROM c
    OR stage='CLAIM' AND node.state<>'LIVE' OR stage='PENDING' AND node.state<>'DELETING') THEN
    RAISE EXCEPTION 'qualified GC graph identity or actual counter changed'; END IF;
  SELECT * INTO body FROM mst2_metadata_payload WHERE page_id=p AND generation=g FOR UPDATE;
  actual:=FOUND;
  IF actual IS DISTINCT FROM payload_present OR actual AND (body.metadata_codec<>life.metadata_codec
    OR body.byte_size<>life.expected_size OR octet_length(body.payload)<>body.byte_size
    OR sha256(convert_to('mega.mst2.metapage','UTF8')||decode('00','hex')||body.payload)<>p) THEN
    RAISE EXCEPTION 'qualified GC durable bytes differ from their exact lifetime and page digest'; END IF;
  IF payload_present THEN PERFORM mst2_metadata_decode_local(body.payload); END IF;
  IF c IS NOT NULL THEN
    SELECT * INTO certificate FROM mst2_metadata_page_certificate WHERE page_id=p AND generation=g AND certificate_digest=c;
    IF NOT FOUND OR certificate.namespace_uuid<>'$NAMESPACE_UUID$'::uuid OR certificate.metadata_codec<>life.metadata_codec
      OR certificate.byte_size<>life.expected_size OR certificate.proof_revision<>1
      OR (SELECT count(*) FROM (SELECT 1 FROM mst2_metadata_verified_ref WHERE parent_page=p AND parent_generation=g LIMIT 258) refs)
        <>jsonb_array_length(certificate.canonical_proof->'references') THEN
      RAISE EXCEPTION 'qualified GC lost its immutable canonical certificate and typed reference evidence'; END IF;
  ELSIF graph_present OR EXISTS(SELECT 1 FROM mst2_metadata_page_certificate WHERE page_id=p AND generation=g) THEN
    RAISE EXCEPTION 'qualified GC cannot omit a certified graph identity'; END IF;
  IF graph_present THEN
    IF (SELECT count(*) FROM (SELECT 1 FROM mst2_metadata_graph_edge WHERE parent_page=p AND parent_generation=g LIMIT 258) edges)>257 THEN
      RAISE EXCEPTION 'qualified canonical node has too many physical outgoing edges'; END IF;
    IF payload_present AND (EXISTS((SELECT child_page,child_generation FROM mst2_metadata_verified_ref WHERE parent_page=p AND parent_generation=g)
        EXCEPT (SELECT child_page,child_generation FROM mst2_metadata_graph_edge WHERE parent_page=p AND parent_generation=g))
      OR EXISTS((SELECT child_page,child_generation FROM mst2_metadata_graph_edge WHERE parent_page=p AND parent_generation=g)
        EXCEPT (SELECT child_page,child_generation FROM mst2_metadata_verified_ref WHERE parent_page=p AND parent_generation=g))) THEN
      RAISE EXCEPTION 'qualified GC physical outgoing edges differ from exact typed occurrences'; END IF;
  END IF;
  IF stage='CLAIM' AND life.state='LIVE' AND NOT (graph_present AND payload_present AND c IS NOT NULL) THEN
    RAISE EXCEPTION 'LIVE metadata corruption cannot be collected'; END IF;
  IF stage='CLAIM' AND graph_present AND NOT payload_present THEN
    RAISE EXCEPTION 'certified RESERVED graph corruption cannot be collected without its durable bytes'; END IF;
  IF stage='CLAIM' AND life.state='RESERVED' AND (NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m
      JOIN mst2_metadata_prepare q USING(prepare_id) WHERE m.page_id=p AND m.generation=g AND m.expected_size=life.expected_size
        AND q.graph_domain='qualified-v1' AND q.state='ABORTED' AND q.storage_seal IS NOT NULL)
    OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
      WHERE m.page_id=p AND m.generation=g AND q.state<>'ABORTED')) THEN
    RAISE EXCEPTION 'RESERVED metadata requires an explicitly aborted exact origin'; END IF;
  IF stage='APPLIED' AND (EXISTS(SELECT 1 FROM mst2_metadata_graph_edge WHERE parent_page=p AND parent_generation=g)
    OR EXISTS(SELECT 1 FROM mst2_metadata_reuse_index WHERE root_page=p AND root_generation=g)) THEN
    RAISE EXCEPTION 'APPLIED qualified receipt still has old physical edges or reuse hints'; END IF;
END $$;

CREATE FUNCTION mst2_metadata_gc_op_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE life mst2_metadata_lifetime%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified GC operation history is immutable'; END IF;
  PERFORM mst2_metadata_gc_enter();
  IF NOT mst2_metadata_scope_matches(NEW.primary_scope) THEN RAISE EXCEPTION 'qualified GC left its captured primary scope'; END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.state<>'PENDING' OR NEW.completed_at IS NOT NULL OR NEW.payload_delete_xid IS NOT NULL
      OR substr(NEW.operation_id::text,15,1)<>'4' OR substr(NEW.operation_id::text,20,1) NOT IN ('8','9','a','b') THEN
      RAISE EXCEPTION 'qualified GC must begin with a fresh UUID and unmodified PENDING proof'; END IF;
    PERFORM mst2_metadata_gc_proof(NEW.page_id,NEW.generation,'CLAIM',NEW.graph_present,NEW.had_payload,NEW.certificate_digest);
    SELECT * INTO STRICT life FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation=NEW.generation;
    IF ROW(NEW.graph_domain,NEW.metadata_codec,NEW.expected_size) IS DISTINCT FROM
      ROW(life.graph_domain,life.metadata_codec,life.expected_size) THEN RAISE EXCEPTION 'qualified GC copied profile is incorrect'; END IF;
    NEW.created_at:=clock_timestamp();
  ELSE
    IF (to_jsonb(NEW)-ARRAY['state','completed_at','payload_delete_xid']) IS DISTINCT FROM
      (to_jsonb(OLD)-ARRAY['state','completed_at','payload_delete_xid']) THEN RAISE EXCEPTION 'qualified GC operation cannot retarget immutable evidence'; END IF;
    IF OLD.state='APPLIED' AND NEW IS DISTINCT FROM OLD THEN RAISE EXCEPTION 'qualified GC receipt cannot revive or change'; END IF;
    IF NEW.payload_delete_xid IS DISTINCT FROM OLD.payload_delete_xid THEN
      IF OLD.state<>'PENDING' OR NEW.state<>'PENDING' OR NOT OLD.had_payload OR OLD.payload_delete_xid IS NOT NULL THEN
        RAISE EXCEPTION 'qualified payload deletion has no fresh same-transaction phase'; END IF;
      PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'PENDING',OLD.graph_present,true,OLD.certificate_digest);
      NEW.payload_delete_xid:=txid_current();
    END IF;
    IF OLD.state='PENDING' AND NEW.state='APPLIED' THEN
      IF OLD.had_payload AND OLD.payload_delete_xid IS DISTINCT FROM txid_current() THEN
        RAISE EXCEPTION 'qualified GC completion has no same-transaction payload removal'; END IF;
      PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'APPLIED',false,false,OLD.certificate_digest);
      NEW.completed_at:=clock_timestamp();
    ELSIF NEW.state IS DISTINCT FROM OLD.state OR NEW.completed_at IS DISTINCT FROM OLD.completed_at THEN
      RAISE EXCEPTION 'qualified GC state transition is invalid'; END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_gc_op_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_gc_op
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_gc_op_guard();

CREATE FUNCTION mst2_metadata_gc_complete() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  SELECT * INTO STRICT op FROM mst2_metadata_gc_op WHERE operation_id=NEW.operation_id;
  IF op.state='PENDING' THEN
    IF op.payload_delete_xid IS NOT NULL THEN RAISE EXCEPTION 'qualified payload deletion phase cannot escape its atomic apply transaction'; END IF;
    PERFORM mst2_metadata_gc_proof(op.page_id,op.generation,'PENDING',op.graph_present,op.had_payload,op.certificate_digest);
  ELSE PERFORM mst2_metadata_gc_proof(op.page_id,op.generation,'APPLIED',false,false,op.certificate_digest); END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_gc_complete AFTER INSERT OR UPDATE ON mst2_metadata_gc_op
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_gc_complete();

CREATE OR REPLACE FUNCTION mst2_metadata_lifetime_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE current_root mst2_metadata_current%ROWTYPE; op mst2_metadata_gc_op%ROWTYPE; previous mst2_metadata_lifetime%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified lifetime watermarks are immutable'; END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.state<>'RESERVED' THEN RAISE EXCEPTION 'qualified lifetime must begin RESERVED'; END IF;
    SELECT * INTO current_root FROM mst2_metadata_current WHERE page_id=NEW.page_id;
    IF FOUND THEN
      SELECT * INTO previous FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation=current_root.generation;
      SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=NEW.page_id AND generation=current_root.generation AND state='APPLIED';
      IF NOT FOUND OR previous.state<>'REMOVED' OR current_root.generation=9223372036854775807
        OR NEW.generation<>current_root.generation+1 OR NEW.metadata_codec<>op.metadata_codec OR NEW.expected_size<>op.expected_size
        OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN RAISE EXCEPTION 'qualified reservation needs its exact next-generation APPLIED predecessor'; END IF;
      PERFORM mst2_metadata_gc_proof(NEW.page_id,current_root.generation,'APPLIED',false,false,op.certificate_digest);
    ELSIF NEW.generation<>1 OR EXISTS(SELECT 1 FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id)
      OR EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=NEW.page_id)
      OR EXISTS(SELECT 1 FROM mst2_metadata_graph_node WHERE page_id=NEW.page_id) THEN
      RAISE EXCEPTION 'qualified initial reservation cannot adopt history'; END IF;
    RETURN NEW;
  END IF;
  IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state') THEN RAISE EXCEPTION 'qualified lifetime identity is immutable'; END IF;
  IF NEW.state=OLD.state THEN RETURN NEW; END IF;
  IF OLD.state='RESERVED' AND NEW.state='LIVE' THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_node node JOIN mst2_metadata_payload body USING(page_id,generation)
      JOIN mst2_metadata_page_certificate certificate USING(page_id,generation)
      WHERE node.page_id=OLD.page_id AND node.generation=OLD.generation AND node.state='LIVE'
        AND node.certificate_digest=certificate.certificate_digest AND node.metadata_codec=OLD.metadata_codec
        AND body.metadata_codec=OLD.metadata_codec AND body.byte_size=OLD.expected_size AND node.bytes=OLD.expected_size)
      OR NOT (EXISTS(SELECT 1 FROM mst2_metadata_graph_root root JOIN mst2_metadata_prepare q USING(prepare_id)
        WHERE root.page_id=OLD.page_id AND root.generation=OLD.generation AND q.plan_kind='COLD'
          AND q.state='COMMITTED' AND root.storage_seal=q.storage_seal AND q.coverage_retired_at IS NULL)
        OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
        JOIN mst2_metadata_root_anchor anchor ON anchor.prepare_id=q.prepare_id AND anchor.anchor_kind='PREPARE'
          AND anchor.root_page=q.metadata_root AND anchor.owner_key=q.prepare_id
        WHERE m.page_id=OLD.page_id AND m.generation=OLD.generation AND q.plan_kind='ROOTED'
          AND q.state='COMMITTED' AND q.coverage_retired_at IS NULL)) THEN
      RAISE EXCEPTION 'qualified LIVE transition lacks its certified graph and exact owned preparation'; END IF;
    RETURN NEW;
  END IF;
  PERFORM mst2_metadata_gc_enter();
  SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='PENDING';
  IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN RAISE EXCEPTION 'qualified lifecycle transition lacks its exact pending operation'; END IF;
  IF OLD.state IN ('RESERVED','LIVE') AND NEW.state='DELETING' THEN
    PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'CLAIM',op.graph_present,op.had_payload,op.certificate_digest);
  ELSIF OLD.state='DELETING' AND NEW.state='REMOVED' THEN
    IF op.had_payload AND op.payload_delete_xid IS DISTINCT FROM txid_current() THEN RAISE EXCEPTION 'qualified removal has no atomic payload deletion'; END IF;
    PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'PENDING',false,false,op.certificate_digest);
    IF EXISTS(SELECT 1 FROM mst2_metadata_graph_edge WHERE parent_page=OLD.page_id AND parent_generation=OLD.generation)
      OR EXISTS(SELECT 1 FROM mst2_metadata_reuse_index WHERE root_page=OLD.page_id AND root_generation=OLD.generation) THEN
      RAISE EXCEPTION 'qualified removal still has old physical references'; END IF;
  ELSE RAISE EXCEPTION 'qualified lifecycle transition is invalid'; END IF;
  RETURN NEW;
END $$;

CREATE OR REPLACE FUNCTION mst2_metadata_current_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE; life mst2_metadata_lifetime%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified current watermark cannot be deleted'; END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.generation<>1 OR EXISTS(SELECT 1 FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation<>1)
      OR EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=NEW.page_id)
      OR EXISTS(SELECT 1 FROM mst2_metadata_graph_node WHERE page_id=NEW.page_id) THEN
      RAISE EXCEPTION 'qualified initial current cannot adopt history'; END IF;
    RETURN NEW;
  END IF;
  IF NEW.page_id<>OLD.page_id OR OLD.generation=9223372036854775807 OR NEW.generation<>OLD.generation+1 THEN
    RAISE EXCEPTION 'qualified current requires exact next-generation CAS'; END IF;
  SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='APPLIED';
  IF NOT FOUND OR NOT mst2_metadata_scope_matches(op.primary_scope)
    OR EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=OLD.page_id)
    OR EXISTS(SELECT 1 FROM mst2_metadata_graph_node WHERE page_id=OLD.page_id AND generation=OLD.generation) THEN
    RAISE EXCEPTION 'qualified current has no definitive old-generation removal'; END IF;
  PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'APPLIED',false,false,op.certificate_digest);
  SELECT * INTO life FROM mst2_metadata_lifetime WHERE page_id=NEW.page_id AND generation=NEW.generation;
  IF NOT FOUND OR life.state<>'RESERVED' OR life.graph_domain<>'qualified-v1'
    OR life.metadata_codec<>op.metadata_codec OR life.expected_size<>op.expected_size THEN
    RAISE EXCEPTION 'qualified fresh current differs from its immutable page profile'; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION mst2_metadata_capacity() RETURNS TABLE(resident_pages bigint,resident_bytes bigint) LANGUAGE sql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT count(*)::bigint,coalesce(sum(byte_size),0)::bigint FROM (SELECT byte_size FROM mst2_metadata_payload LIMIT 16385) actual
$$;
CREATE FUNCTION mst2_metadata_capacity_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE pages bigint; bytes bigint; actual bigint;
BEGIN
  IF TG_TABLE_NAME='mst2_metadata_payload' THEN
    SELECT resident_pages,resident_bytes INTO pages,bytes FROM mst2_metadata_capacity();
    IF pages>16384 OR bytes>268435456 THEN RAISE EXCEPTION 'qualified resident payload capacity is exceeded'; END IF;
  ELSIF TG_TABLE_NAME='mst2_metadata_source_entry_reference' THEN
    SELECT count(*) INTO actual FROM (SELECT 1 FROM mst2_metadata_source_entry_reference LIMIT 262145) bounded;
    IF actual>262144 THEN RAISE EXCEPTION 'qualified retained source dictionary capacity is exceeded'; END IF;
    RETURN NULL;
  ELSIF TG_TABLE_NAME='mst2_qualified_session_incarnation' THEN
    SELECT count(*) INTO actual FROM (SELECT 1 FROM mst2_qualified_session_incarnation WHERE state='READY' LIMIT 4097) bounded;
  ELSIF TG_TABLE_NAME='mst2_qualified_lease_binding' THEN
    SELECT count(*) INTO actual FROM (SELECT 1 FROM mst2_qualified_lease_binding WHERE state='ACTIVE' LIMIT 4097) bounded;
  ELSE SELECT count(*) INTO actual FROM (SELECT 1 FROM mst2_metadata_reader_operation WHERE state='ACTIVE' LIMIT 4097) bounded; END IF;
  IF actual>4096 THEN RAISE EXCEPTION 'qualified active serving capacity is exceeded'; END IF;
  RETURN NULL;
END $$;
DO $$ DECLARE name text; BEGIN
  FOREACH name IN ARRAY ARRAY['mst2_metadata_payload','mst2_qualified_session_incarnation',
      'mst2_qualified_lease_binding','mst2_metadata_reader_operation','mst2_metadata_source_entry_reference'] LOOP
    EXECUTE format('CREATE TRIGGER mst2_metadata_capacity_guard AFTER INSERT OR UPDATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_capacity_guard()',name);
  END LOOP;
END $$;

-- Permanent identity evidence and bounded reader owners have independent
-- resident quotas. Admission reads actual stored rows, never hints.
CREATE FUNCTION mst2_metadata_history_capacity_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE actual bigint; bytes bigint; row_limit integer; size_expression text;
BEGIN
  IF TG_TABLE_NAME IN ('mst2_metadata_prepare_page','mst2_metadata_prepare_reuse_root','mst2_metadata_verified_ref') THEN
    row_limit:=262144; size_expression:='0';
  ELSIF TG_TABLE_NAME='mst2_metadata_prepare' THEN
    row_limit:=16384;
    size_expression:='pg_column_size(canonical_plan)::bigint+pg_column_size(canonical_bindings)+pg_column_size(primary_scope)';
  ELSIF TG_TABLE_NAME='mst2_metadata_source_root_attestation' THEN
    row_limit:=16384; size_expression:='pg_column_size(source_proof)::bigint+pg_column_size(source_profile)';
  ELSIF TG_TABLE_NAME='mst2_metadata_page_certificate' THEN
    row_limit:=16384; size_expression:='pg_column_size(canonical_proof)::bigint';
  ELSIF TG_TABLE_NAME='mst2_metadata_scope_source_reference' THEN
    row_limit:=16384; size_expression:='pg_column_size(ancestor_revisions)::bigint';
  ELSIF TG_TABLE_NAME='mst2_qualified_session_incarnation' THEN
    row_limit:=65536; size_expression:='pg_column_size(canonical_descriptor)::bigint+pg_column_size(source_profile)';
  ELSIF TG_TABLE_NAME IN ('mst2_metadata_lifetime','mst2_metadata_current','mst2_metadata_gc_op',
      'mst2_qualified_lease_binding','mst2_metadata_reader_operation') THEN
    row_limit:=65536; size_expression:='0';
  ELSE RAISE EXCEPTION 'qualified history quota has an unknown physical relation'; END IF;
  EXECUTE format('SELECT count(*),coalesce(sum(size),0) FROM (SELECT %s AS size FROM %I.%I LIMIT %s) actual',
    size_expression,TG_TABLE_SCHEMA,TG_TABLE_NAME,row_limit+1) INTO actual,bytes;
  IF actual>row_limit OR bytes>268435456 THEN
    RAISE EXCEPTION 'qualified immutable history capacity is exceeded for %',TG_TABLE_NAME; END IF;
  RETURN NULL;
END $$;
DO $$ DECLARE name text; BEGIN
  FOREACH name IN ARRAY ARRAY['mst2_metadata_prepare','mst2_metadata_prepare_page','mst2_metadata_prepare_reuse_root',
      'mst2_metadata_verified_ref','mst2_metadata_source_root_attestation','mst2_metadata_page_certificate','mst2_metadata_scope_source_reference',
      'mst2_qualified_session_incarnation','mst2_metadata_lifetime','mst2_metadata_current','mst2_metadata_gc_op',
      'mst2_qualified_lease_binding','mst2_metadata_reader_operation'] LOOP
    EXECUTE format('CREATE TRIGGER mst2_metadata_history_capacity_guard AFTER INSERT ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_history_capacity_guard()',name);
  END LOOP;
END $$;

CREATE FUNCTION mst2_metadata_gc_claim(p bytea,g bigint,scope bytea,id uuid) RETURNS uuid LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE life mst2_metadata_lifetime%ROWTYPE; node mst2_metadata_graph_node%ROWTYPE; op mst2_metadata_gc_op%ROWTYPE;
  has_graph boolean; has_body boolean;
BEGIN
  PERFORM mst2_metadata_gc_enter();
  IF NOT mst2_metadata_scope_matches(scope) THEN RAISE EXCEPTION 'qualified claim scope differs from its captured primary'; END IF;
  SELECT * INTO op FROM mst2_metadata_gc_op WHERE operation_id=id;
  IF FOUND THEN
    IF op.page_id IS DISTINCT FROM p OR op.generation<>g OR op.primary_scope IS DISTINCT FROM scope THEN
      RAISE EXCEPTION 'qualified GC operation cannot be reused for a different lifetime'; END IF;
    RETURN id;
  END IF;
  SELECT * INTO STRICT life FROM mst2_metadata_lifetime WHERE page_id=p AND generation=g;
  SELECT * INTO node FROM mst2_metadata_graph_node WHERE page_id=p AND generation=g;
  has_graph:=FOUND; has_body:=EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=p AND generation=g);
  INSERT INTO mst2_metadata_gc_op(operation_id,page_id,generation,primary_scope,graph_domain,metadata_codec,
    expected_size,graph_present,had_payload,certificate_digest,state)
    VALUES(id,p,g,scope,'qualified-v1',life.metadata_codec,life.expected_size,has_graph,has_body,
      CASE WHEN has_graph THEN node.certificate_digest ELSE NULL END,'PENDING');
  UPDATE mst2_metadata_lifetime SET state='DELETING' WHERE page_id=p AND generation=g;
  IF has_graph THEN UPDATE mst2_metadata_graph_node SET state='DELETING' WHERE page_id=p AND generation=g; END IF;
  RETURN id;
END $$;

CREATE OR REPLACE FUNCTION mst2_metadata_gc_apply(id uuid) RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  PERFORM mst2_metadata_gc_enter();
  SELECT * INTO STRICT op FROM mst2_metadata_gc_op WHERE operation_id=id FOR UPDATE;
  IF NOT mst2_metadata_scope_matches(op.primary_scope) THEN RAISE EXCEPTION 'qualified GC replay left its captured primary'; END IF;
  IF op.state='APPLIED' THEN
    PERFORM mst2_metadata_gc_proof(op.page_id,op.generation,'APPLIED',false,false,op.certificate_digest); RETURN;
  END IF;
  PERFORM mst2_metadata_gc_proof(op.page_id,op.generation,'PENDING',op.graph_present,op.had_payload,op.certificate_digest);
  IF op.had_payload THEN
    UPDATE mst2_metadata_gc_op SET payload_delete_xid=txid_current() WHERE operation_id=id;
    DELETE FROM mst2_metadata_payload WHERE page_id=op.page_id AND generation=op.generation;
    IF NOT FOUND THEN RAISE EXCEPTION 'qualified atomic payload deletion lost its exact row'; END IF;
  END IF;
  DELETE FROM mst2_metadata_graph_edge WHERE parent_page=op.page_id AND parent_generation=op.generation;
  DELETE FROM mst2_metadata_reuse_index WHERE root_page=op.page_id AND root_generation=op.generation;
  DELETE FROM mst2_metadata_graph_node WHERE page_id=op.page_id AND generation=op.generation;
  UPDATE mst2_metadata_lifetime SET state='REMOVED' WHERE page_id=op.page_id AND generation=op.generation AND state='DELETING';
  IF NOT FOUND THEN RAISE EXCEPTION 'qualified atomic removal lost its exact lifecycle'; END IF;
  UPDATE mst2_metadata_gc_op SET state='APPLIED' WHERE operation_id=id;
END $$;
CREATE OR REPLACE FUNCTION mst2_metadata_gc_finish(id uuid) RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$ BEGIN PERFORM mst2_metadata_gc_apply(id); END $$;

-- Additional graph/payload guards follow; no history relation is collected.
CREATE OR REPLACE FUNCTION mst2_metadata_payload_fenced() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'qualified immutable payload cannot be updated'; END IF;
  IF TG_OP='DELETE' THEN
    PERFORM mst2_metadata_gc_enter();
    SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='PENDING';
    IF NOT FOUND OR NOT op.had_payload OR op.payload_delete_xid IS DISTINCT FROM txid_current()
      OR NOT mst2_metadata_scope_matches(op.primary_scope) OR op.expected_size<>OLD.byte_size OR op.metadata_codec<>OLD.metadata_codec THEN
      RAISE EXCEPTION 'qualified payload removal needs its exact same-transaction pending evidence'; END IF;
    PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'PENDING',op.graph_present,true,op.certificate_digest);
    RETURN OLD;
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l USING(page_id,generation)
    JOIN mst2_metadata_prepare_page m USING(page_id,generation) JOIN mst2_metadata_prepare q USING(prepare_id)
    WHERE c.page_id=NEW.page_id AND c.generation=NEW.generation AND l.state IN ('RESERVED','LIVE')
      AND l.metadata_codec=NEW.metadata_codec AND l.expected_size=NEW.byte_size AND q.state='PREPARING'
      AND q.graph_domain='qualified-v1' AND mst2_metadata_scope_matches(q.primary_scope)) THEN
    RAISE EXCEPTION 'qualified payload INSERT crossed its exact active generation'; END IF;
  RETURN NEW;
END $$;

CREATE OR REPLACE FUNCTION mst2_metadata_graph_node_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE life mst2_metadata_lifetime%ROWTYPE; op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF TG_OP='DELETE' THEN
    PERFORM mst2_metadata_gc_enter();
    SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='PENDING';
    IF NOT FOUND OR NOT op.graph_present OR op.certificate_digest IS DISTINCT FROM OLD.certificate_digest
      OR OLD.state<>'DELETING' OR NOT mst2_metadata_scope_matches(op.primary_scope)
      OR op.had_payload AND op.payload_delete_xid IS DISTINCT FROM txid_current()
      OR EXISTS(SELECT 1 FROM mst2_metadata_graph_edge WHERE parent_page=OLD.page_id AND parent_generation=OLD.generation) THEN
      RAISE EXCEPTION 'qualified graph removal lacks its atomic byte removal and empty outgoing edges'; END IF;
    PERFORM mst2_metadata_gc_proof(OLD.page_id,OLD.generation,'PENDING',true,false,op.certificate_digest);
    RETURN OLD;
  END IF;
  SELECT l.* INTO life FROM mst2_metadata_current c JOIN mst2_metadata_lifetime l USING(page_id,generation)
    WHERE c.page_id=NEW.page_id AND c.generation=NEW.generation;
  IF NOT FOUND OR life.graph_domain<>'qualified-v1' OR life.metadata_codec<>NEW.metadata_codec OR life.expected_size<>NEW.bytes
    OR NEW.incoming_refs<>(SELECT count(*) FROM mst2_metadata_graph_edge WHERE child_page=NEW.page_id AND child_generation=NEW.generation)
    OR NOT EXISTS(SELECT 1 FROM mst2_metadata_page_certificate certificate WHERE certificate.page_id=NEW.page_id
      AND certificate.generation=NEW.generation AND certificate.certificate_digest=NEW.certificate_digest
      AND certificate.metadata_codec=NEW.metadata_codec AND certificate.byte_size=NEW.bytes) THEN
    RAISE EXCEPTION 'qualified graph identity, certificate or physical counter changed'; END IF;
  IF TG_OP='INSERT' THEN
    IF NEW.state<>'LIVE' OR life.state NOT IN ('RESERVED','LIVE')
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
        WHERE m.page_id=NEW.page_id AND m.generation=NEW.generation AND q.state='PREPARING') THEN
      RAISE EXCEPTION 'qualified graph creation requires an active exact preparation'; END IF;
  ELSE
    IF (to_jsonb(NEW)-ARRAY['state','incoming_refs']) IS DISTINCT FROM (to_jsonb(OLD)-ARRAY['state','incoming_refs']) THEN
      RAISE EXCEPTION 'qualified graph identity is immutable'; END IF;
    IF NEW.state<>OLD.state THEN
      PERFORM mst2_metadata_gc_enter();
      SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.page_id AND generation=OLD.generation AND state='PENDING';
      IF NOT FOUND OR OLD.state<>'LIVE' OR NEW.state<>'DELETING' OR life.state<>'DELETING' OR NEW.incoming_refs<>0
        OR op.certificate_digest IS DISTINCT FROM NEW.certificate_digest OR NOT mst2_metadata_scope_matches(op.primary_scope) THEN
        RAISE EXCEPTION 'qualified graph state change has no exact pending claim'; END IF;
      PERFORM mst2_metadata_assert_uncovered(OLD.page_id,OLD.generation);
    ELSIF NEW.state='LIVE' AND life.state NOT IN ('RESERVED','LIVE') OR NEW.state='DELETING' AND life.state<>'DELETING' THEN
      RAISE EXCEPTION 'qualified graph state differs from its current lifecycle'; END IF;
  END IF;
  RETURN NEW;
END $$;

CREATE OR REPLACE FUNCTION mst2_metadata_graph_edge_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE op mst2_metadata_gc_op%ROWTYPE;
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'qualified edge identity is immutable'; END IF;
  IF TG_OP='DELETE' THEN
    PERFORM mst2_metadata_gc_enter();
    SELECT * INTO op FROM mst2_metadata_gc_op WHERE page_id=OLD.parent_page AND generation=OLD.parent_generation AND state='PENDING';
    IF NOT FOUND OR NOT op.graph_present OR NOT mst2_metadata_scope_matches(op.primary_scope)
      OR op.had_payload AND op.payload_delete_xid IS DISTINCT FROM txid_current()
      OR EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=OLD.parent_page AND generation=OLD.parent_generation)
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_node node JOIN mst2_metadata_current cur USING(page_id,generation)
        JOIN mst2_metadata_lifetime life USING(page_id,generation)
        WHERE node.page_id=OLD.parent_page AND node.generation=OLD.parent_generation AND node.state='DELETING'
          AND life.state='DELETING' AND node.incoming_refs=0 AND node.certificate_digest=op.certificate_digest) THEN
      RAISE EXCEPTION 'qualified edge removal is outside its exact atomic graph deletion phase'; END IF;
    PERFORM mst2_metadata_assert_uncovered(OLD.parent_page,OLD.parent_generation);
    RETURN OLD;
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_verified_ref ref
    JOIN mst2_metadata_page_certificate parent ON parent.page_id=ref.parent_page AND parent.generation=ref.parent_generation
    JOIN mst2_metadata_page_certificate child ON child.page_id=ref.child_page AND child.generation=ref.child_generation
    WHERE ref.parent_page=NEW.parent_page AND ref.parent_generation=NEW.parent_generation
      AND ref.child_page=NEW.child_page AND ref.child_generation=NEW.child_generation
      AND ref.child_certificate_digest=child.certificate_digest AND parent.rank>child.rank) THEN
    RAISE EXCEPTION 'qualified edge differs from its exact canonical reference or strict certified rank'; END IF;
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
    RAISE EXCEPTION 'edge creation requires active exact qualified endpoints'; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION mst2_metadata_edges_removed() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF (SELECT count(*) FROM removed_edges)>257 THEN RAISE EXCEPTION 'qualified GC edge deletion exceeds one canonical node'; END IF;
  IF EXISTS(SELECT 1 FROM mst2_metadata_graph_node node JOIN (SELECT child_page,child_generation,count(*) AS delta
      FROM removed_edges GROUP BY child_page,child_generation) removed
      ON removed.child_page=node.page_id AND removed.child_generation=node.generation
    WHERE node.incoming_refs-removed.delta<>(SELECT count(*) FROM mst2_metadata_graph_edge edge
      WHERE edge.child_page=node.page_id AND edge.child_generation=node.generation)) THEN
    RAISE EXCEPTION 'qualified removal discovered physical incoming counter drift'; END IF;
  UPDATE mst2_metadata_graph_node node SET incoming_refs=node.incoming_refs-removed.delta
    FROM (SELECT child_page,child_generation,count(*) AS delta FROM removed_edges GROUP BY child_page,child_generation) removed
    WHERE node.page_id=removed.child_page AND node.generation=removed.child_generation;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_edges_removed AFTER DELETE ON mst2_metadata_graph_edge
  REFERENCING OLD TABLE AS removed_edges FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_edges_removed();

CREATE FUNCTION mst2_metadata_gc_owner_cleanup(maximum integer)
RETURNS TABLE(examined bigint,readers_expired bigint,leases_expired bigint,prepares_aborted bigint,
  handovers_retired bigint,orphans_retired bigint)
LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE bound integer:=maximum; item record; q mst2_metadata_prepare%ROWTYPE; now_unix bigint;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  IF maximum IS NULL OR maximum NOT BETWEEN 0 AND 64 THEN RAISE EXCEPTION 'qualified owner cleanup budget must be 0..=64'; END IF;
  examined:=0; readers_expired:=0; leases_expired:=0; prepares_aborted:=0; handovers_retired:=0; orphans_retired:=0;
  now_unix:=floor(extract(epoch FROM clock_timestamp()))::bigint;
  FOR item IN SELECT * FROM (
      (SELECT 'READER'::text AS kind,operation_id::text AS owner,lease_id,hard_deadline_unix AS deadline,reader_issuance
        FROM mst2_metadata_reader_operation WHERE state='ACTIVE' AND hard_deadline_unix<=now_unix
        ORDER BY hard_deadline_unix,operation_id LIMIT bound)
      UNION ALL
      (SELECT 'LEASE'::text,lease_id,lease_id,expires_at_unix,NULL::bigint FROM mst2_qualified_lease_binding
        WHERE state='ACTIVE' AND expires_at_unix<=now_unix ORDER BY expires_at_unix,lease_id LIMIT bound)
      UNION ALL
      (SELECT 'PREPARE'::text,prepare_id,NULL::text,floor(extract(epoch FROM orphan_expires_at))::bigint,NULL::bigint
        FROM mst2_metadata_prepare WHERE (state='PREPARING' OR state='COMMITTED' AND coverage_retired_at IS NULL)
          AND orphan_expires_at<=clock_timestamp() ORDER BY orphan_expires_at,prepare_id LIMIT bound)
    ) expired ORDER BY deadline,kind,owner LIMIT bound LOOP
    examined:=examined+1;
    IF item.kind='READER' THEN
      UPDATE mst2_metadata_reader_operation SET state='EXPIRED' WHERE operation_id=item.owner::uuid AND reader_issuance=item.reader_issuance AND state='ACTIVE';
      IF NOT FOUND THEN RAISE EXCEPTION 'qualified expired reader changed behind its mutation barrier'; END IF;
      readers_expired:=readers_expired+1;
      DELETE FROM mst2_metadata_root_anchor WHERE reader_operation_id=item.owner::uuid AND reader_issuance=item.reader_issuance AND anchor_kind IN ('REQUEST','READER');
      PERFORM mst2_metadata_cleanup_lease(item.lease_id);
    ELSIF item.kind='LEASE' THEN
      UPDATE mst2_qualified_lease_binding SET state='EXPIRED',lease_epoch=lease_epoch+1 WHERE lease_id=item.owner AND state='ACTIVE';
      IF NOT FOUND THEN RAISE EXCEPTION 'qualified expired lease changed behind its mutation barrier'; END IF;
      leases_expired:=leases_expired+1; PERFORM mst2_metadata_cleanup_lease(item.lease_id);
    ELSE
      SELECT * INTO STRICT q FROM mst2_metadata_prepare WHERE prepare_id=item.owner;
      IF q.state='PREPARING' THEN
        DELETE FROM mst2_metadata_graph_root WHERE prepare_id=q.prepare_id;
        UPDATE mst2_metadata_prepare SET state='ABORTED',aborted_at=clock_timestamp() WHERE prepare_id=q.prepare_id;
        DELETE FROM mst2_metadata_root_anchor WHERE prepare_id=q.prepare_id AND anchor_kind IN ('PREPARE','REUSE');
        prepares_aborted:=prepares_aborted+1;
      ELSIF mst2_metadata_session_covers_prepare(q.prepare_id) THEN
        UPDATE mst2_metadata_prepare SET coverage_retired_at=clock_timestamp() WHERE prepare_id=q.prepare_id;
        DELETE FROM mst2_metadata_root_anchor WHERE prepare_id=q.prepare_id AND anchor_kind IN ('PREPARE','REUSE');
        handovers_retired:=handovers_retired+1;
      ELSIF mst2_metadata_orphan_prepare_eligible(q.prepare_id) THEN
        UPDATE mst2_metadata_prepare SET coverage_retired_at=clock_timestamp() WHERE prepare_id=q.prepare_id;
        DELETE FROM mst2_metadata_root_anchor WHERE prepare_id=q.prepare_id AND anchor_kind IN ('PREPARE','REUSE');
        orphans_retired:=orphans_retired+1;
      END IF;
    END IF;
  END LOOP;
  RETURN NEXT;
END $$;
DROP TRIGGER mst2_01_family_closed ON mst2_metadata_gc_op;
