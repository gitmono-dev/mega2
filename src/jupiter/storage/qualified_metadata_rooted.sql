CREATE FUNCTION mst2_metadata_read_be(b bytea,p integer,w integer) RETURNS numeric
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE v numeric:=0; i integer;
BEGIN
  IF w NOT IN (2,4,8) OR p<0 OR p>octet_length(b)-w THEN RAISE EXCEPTION 'rooted integer is out of bounds'; END IF;
  FOR i IN 0..w-1 LOOP v:=v*256+get_byte(b,p+i); END LOOP;
  RETURN v;
END $$;

CREATE FUNCTION mst2_metadata_rooted_string(b bytea,p integer,maximum integer) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE n numeric; value text;
BEGIN
  n:=mst2_metadata_read_be(b,p,4); p:=p+4;
  IF n>maximum OR n>octet_length(b)-p THEN RAISE EXCEPTION 'rooted string exceeds its exact byte boundary'; END IF;
  value:=convert_from(substring(b FROM p+1 FOR n::integer),'UTF8');
  RETURN jsonb_build_object('end',p+n::integer,'text',value);
END $$;

CREATE FUNCTION mst2_metadata_decode_rooted_plan(b bytea) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE domain bytea:=convert_to('mega.mst2.rooted-install.v1','UTF8')||decode('00','hex');
  cursor_pos integer; part jsonb; source_domain text; tree_oid text; scope text; hash_kind text; identity jsonb;
  root bytea; count numeric; i integer; page bytea; parent bytea; child bytea; previous bytea; previous_tree text;
  size numeric; generation numeric; attestation bytea; attestation_digest bytea; certificate_digest bytea;
  delta_rows jsonb[]:=ARRAY[]::jsonb[]; edge_rows jsonb[]:=ARRAY[]::jsonb[];
  reuse_rows jsonb[]:=ARRAY[]::jsonb[]; source_rows jsonb[]:=ARRAY[]::jsonb[]; total_bytes bigint:=0;
  delta_index jsonb; node_index jsonb; adjacency jsonb;
BEGIN
  IF octet_length(b)>2097152 OR substring(b FROM 1 FOR octet_length(domain))<>domain THEN
    RAISE EXCEPTION 'rooted preparation domain or byte budget is invalid';
  END IF;
  cursor_pos:=octet_length(domain);
  IF mst2_metadata_read_be(b,cursor_pos,2)<>1 THEN RAISE EXCEPTION 'rooted preparation version is unsupported'; END IF;
  cursor_pos:=cursor_pos+2;
  part:=mst2_metadata_rooted_string(b,cursor_pos,64); cursor_pos:=(part->>'end')::integer; source_domain:=part->>'text';
  part:=mst2_metadata_rooted_string(b,cursor_pos,128); cursor_pos:=(part->>'end')::integer; tree_oid:=part->>'text';
  part:=mst2_metadata_rooted_string(b,cursor_pos,4096); cursor_pos:=(part->>'end')::integer; scope:=part->>'text';
  IF source_domain<>'native-git' OR tree_oid !~ '^(sha1:[0-9a-f]{40}|sha256:[0-9a-f]{64}|blake3:[0-9a-f]{64})$'
    OR scope NOT LIKE '/%' OR scope<>'/' AND (scope LIKE '%/' OR scope LIKE '%//%' OR EXISTS(
      SELECT 1 FROM unnest(string_to_array(substring(scope FROM 2),'/')) name WHERE name IN ('','.','..')
        OR octet_length(name)>255) OR cardinality(string_to_array(substring(scope FROM 2),'/'))>256) THEN
    RAISE EXCEPTION 'rooted source identity or scope is invalid'; END IF;
  hash_kind:=split_part(tree_oid,':',1);
  identity:=jsonb_build_object('source_domain',source_domain,'tagged_root_tree_oid',tree_oid,'scope',scope,
    'schema_version',mst2_metadata_read_be(b,cursor_pos,2),
    'metadata_codec',mst2_metadata_read_be(b,cursor_pos+2,2),
    'materialization_policy',mst2_metadata_read_be(b,cursor_pos+4,2),
    'fs_semantics',mst2_metadata_read_be(b,cursor_pos+6,2),
    'access_projection',mst2_metadata_read_be(b,cursor_pos+8,2),
    'verification_revision',mst2_metadata_read_be(b,cursor_pos+10,4),
    'projection_revision',mst2_metadata_read_be(b,cursor_pos+14,2));
  cursor_pos:=cursor_pos+16;
  IF (identity->>'schema_version')::integer<>2 OR (identity->>'metadata_codec')::integer<>1
    OR (identity->>'materialization_policy')::integer<>1 OR (identity->>'fs_semantics')::integer<>1
    OR (identity->>'access_projection')::integer<>0 OR (identity->>'verification_revision')::integer<>2
    OR (identity->>'projection_revision')::integer<>1 THEN RAISE EXCEPTION 'rooted source profile is not current native'; END IF;
  IF cursor_pos>octet_length(b)-32 THEN RAISE EXCEPTION 'rooted metadata root is truncated'; END IF;
  root:=substring(b FROM cursor_pos+1 FOR 32); cursor_pos:=cursor_pos+32;
  count:=mst2_metadata_read_be(b,cursor_pos,4); cursor_pos:=cursor_pos+4;
  IF count>4096 THEN RAISE EXCEPTION 'rooted delta exceeds its node budget'; END IF;
  IF count>0 THEN FOR i IN 1..count::integer LOOP
    IF cursor_pos>octet_length(b)-40 THEN RAISE EXCEPTION 'rooted delta member is truncated'; END IF;
    page:=substring(b FROM cursor_pos+1 FOR 32); size:=mst2_metadata_read_be(b,cursor_pos+32,8); cursor_pos:=cursor_pos+40;
    IF previous IS NOT NULL AND previous>=page OR size NOT BETWEEN 20 AND 16384 THEN
      RAISE EXCEPTION 'rooted delta is not exactly ordered or has invalid size'; END IF;
    previous:=page; total_bytes:=total_bytes+size::bigint;
    IF total_bytes>67108864 THEN RAISE EXCEPTION 'rooted delta exceeds its metadata byte budget'; END IF;
    delta_rows:=array_append(delta_rows,jsonb_build_object('page',encode(page,'hex'),'size',size));
  END LOOP; END IF;
  previous:=NULL; count:=mst2_metadata_read_be(b,cursor_pos,4); cursor_pos:=cursor_pos+4;
  IF count>16384 THEN RAISE EXCEPTION 'rooted delta exceeds its edge budget'; END IF;
  IF count>0 THEN FOR i IN 1..count::integer LOOP
    IF cursor_pos>octet_length(b)-64 THEN RAISE EXCEPTION 'rooted edge is truncated'; END IF;
    parent:=substring(b FROM cursor_pos+1 FOR 32); child:=substring(b FROM cursor_pos+33 FOR 32); cursor_pos:=cursor_pos+64;
    IF previous IS NOT NULL AND previous>=parent||child OR parent=child THEN RAISE EXCEPTION 'rooted edges are not unique and ordered'; END IF;
    previous:=parent||child;
    edge_rows:=array_append(edge_rows,jsonb_build_object('parent',encode(parent,'hex'),'child',encode(child,'hex')));
  END LOOP; END IF;
  previous:=NULL; count:=mst2_metadata_read_be(b,cursor_pos,4); cursor_pos:=cursor_pos+4;
  IF count>4096 OR coalesce(array_length(delta_rows,1),0)+count>4096 THEN RAISE EXCEPTION 'rooted delta and boundaries exceed node budget'; END IF;
  IF count>0 THEN FOR i IN 1..count::integer LOOP
    IF cursor_pos>octet_length(b)-120 THEN RAISE EXCEPTION 'rooted reuse boundary is truncated'; END IF;
    page:=substring(b FROM cursor_pos+1 FOR 32); generation:=mst2_metadata_read_be(b,cursor_pos+32,8);
    attestation:=substring(b FROM cursor_pos+41 FOR 16); attestation_digest:=substring(b FROM cursor_pos+57 FOR 32);
    certificate_digest:=substring(b FROM cursor_pos+89 FOR 32); cursor_pos:=cursor_pos+120;
    IF previous IS NOT NULL AND previous>=page OR generation NOT BETWEEN 1 AND 9223372036854775807 THEN
      RAISE EXCEPTION 'rooted reuse boundaries are not exact positive ordered lifetimes'; END IF;
    previous:=page;
    reuse_rows:=array_append(reuse_rows,jsonb_build_object('page',encode(page,'hex'),'generation',generation,
      'attestation_id',encode(attestation,'hex')::uuid,'attestation_digest',encode(attestation_digest,'hex'),
      'certificate_digest',encode(certificate_digest,'hex')));
  END LOOP; END IF;
  count:=mst2_metadata_read_be(b,cursor_pos,4); cursor_pos:=cursor_pos+4;
  IF count NOT BETWEEN 1 AND 4096 THEN RAISE EXCEPTION 'rooted source-root budget is invalid'; END IF;
  FOR i IN 1..count::integer LOOP
    part:=mst2_metadata_rooted_string(b,cursor_pos,128); cursor_pos:=(part->>'end')::integer; tree_oid:=part->>'text';
    IF cursor_pos>octet_length(b)-32 THEN RAISE EXCEPTION 'rooted source root is truncated'; END IF;
    page:=substring(b FROM cursor_pos+1 FOR 32); cursor_pos:=cursor_pos+32;
    IF tree_oid !~ '^(sha1:[0-9a-f]{40}|sha256:[0-9a-f]{64}|blake3:[0-9a-f]{64})$'
      OR split_part(tree_oid,':',1)<>hash_kind OR previous_tree IS NOT NULL AND convert_to(previous_tree,'UTF8')>=convert_to(tree_oid,'UTF8') THEN
      RAISE EXCEPTION 'rooted source roots are not unique ordered same-profile identities'; END IF;
    previous_tree:=tree_oid; source_rows:=array_append(source_rows,jsonb_build_object('tree_oid',tree_oid,'page',encode(page,'hex')));
  END LOOP;
  IF cursor_pos<>octet_length(b) THEN RAISE EXCEPTION 'rooted preparation has trailing bytes'; END IF;
  SELECT coalesce(jsonb_object_agg(value->>'page',true),'{}'::jsonb) INTO delta_index FROM unnest(delta_rows) d(value);
  SELECT coalesce(jsonb_object_agg(value->>'page',true),'{}'::jsonb) INTO node_index FROM (
    SELECT d.value FROM unnest(delta_rows) d(value) UNION ALL SELECT r.value FROM unnest(reuse_rows) r(value)) nodes;
  IF coalesce(array_length(delta_rows,1),0)+coalesce(array_length(reuse_rows,1),0)=0
    OR EXISTS(SELECT 1 FROM unnest(reuse_rows) r(value) WHERE delta_index ? (r.value->>'page'))
    OR NOT node_index ? encode(root,'hex')
    OR EXISTS(SELECT 1 FROM unnest(edge_rows) e(value) WHERE NOT delta_index ? (e.value->>'parent')
      OR NOT node_index ? (e.value->>'child'))
    OR EXISTS(SELECT 1 FROM unnest(source_rows) s(value) WHERE NOT node_index ? (s.value->>'page'))
    OR NOT EXISTS(SELECT 1 FROM unnest(source_rows) s(value) WHERE value->>'page'=encode(root,'hex')) THEN
    RAISE EXCEPTION 'rooted plan has overlap or an unbound graph/source endpoint';
  END IF;
  SELECT coalesce(jsonb_object_agg(grouped.parent,grouped.children),'{}'::jsonb) INTO adjacency FROM (
    SELECT value->>'parent' AS parent,jsonb_agg(value->>'child') AS children FROM unnest(edge_rows) e(value)
      GROUP BY value->>'parent') grouped;
  IF EXISTS(WITH RECURSIVE reached(page) AS (SELECT encode(root,'hex') UNION
      SELECT children.child FROM reached r CROSS JOIN LATERAL jsonb_array_elements_text(adjacency->r.page) children(child))
    SELECT 1 FROM (SELECT d.value FROM unnest(delta_rows) d(value) UNION ALL SELECT r.value FROM unnest(reuse_rows) r(value)) nodes
      WHERE NOT EXISTS(SELECT 1 FROM reached r WHERE r.page=nodes.value->>'page')) THEN
    RAISE EXCEPTION 'rooted plan includes members outside its bounded delta and boundary closure';
  END IF;
  RETURN identity||jsonb_build_object('root',encode(root,'hex'),'delta',to_jsonb(delta_rows),'edges',to_jsonb(edge_rows),
    'reused',to_jsonb(reuse_rows),'source_roots',to_jsonb(source_rows),'total_delta_bytes',total_bytes);
END $$;

CREATE FUNCTION mst2_metadata_decode_delta_bindings(b bytea,plan jsonb) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE count numeric; cursor_pos integer:=12; i integer; page bytea; generation numeric; size numeric;
  previous bytea; binding_rows jsonb[]:=ARRAY[]::jsonb[];
BEGIN
  IF octet_length(b) NOT BETWEEN 12 AND 196620 OR substring(b FROM 1 FOR 8)<>convert_to('MST2GEN1','UTF8') THEN
    RAISE EXCEPTION 'rooted generation binding encoding is invalid'; END IF;
  count:=mst2_metadata_read_be(b,8,4);
  IF count<>jsonb_array_length(plan->'delta') OR octet_length(b)<>12+48*count THEN
    RAISE EXCEPTION 'rooted delta binding count differs from its immutable plan'; END IF;
  IF count>0 THEN FOR i IN 1..count::integer LOOP
    page:=substring(b FROM cursor_pos+1 FOR 32); generation:=mst2_metadata_read_be(b,cursor_pos+32,8);
    size:=mst2_metadata_read_be(b,cursor_pos+40,8); cursor_pos:=cursor_pos+48;
    IF previous IS NOT NULL AND previous>=page OR generation NOT BETWEEN 1 AND 9223372036854775807
      OR encode(page,'hex') IS DISTINCT FROM plan->'delta'->(i-1)->>'page'
      OR size IS DISTINCT FROM (plan->'delta'->(i-1)->>'size')::numeric THEN
      RAISE EXCEPTION 'rooted generation binding is not its exact positive ordered delta member'; END IF;
    previous:=page; binding_rows:=array_append(binding_rows,jsonb_build_object('page',encode(page,'hex'),'generation',generation,'size',size));
  END LOOP; END IF;
  RETURN to_jsonb(binding_rows);
END $$;

CREATE FUNCTION mst2_metadata_rooted_manifest(q mst2_metadata_prepare) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE plan jsonb:=mst2_metadata_decode_rooted_plan(q.canonical_plan); bindings jsonb;
BEGIN
  bindings:=mst2_metadata_decode_delta_bindings(q.canonical_bindings,plan);
  IF q.plan_kind<>'ROOTED' OR q.source_domain IS DISTINCT FROM plan->>'source_domain'
    OR q.tagged_root_tree_oid IS DISTINCT FROM plan->>'tagged_root_tree_oid' OR q.scope IS DISTINCT FROM plan->>'scope'
    OR q.schema_version IS DISTINCT FROM (plan->>'schema_version')::smallint
    OR q.metadata_codec IS DISTINCT FROM (plan->>'metadata_codec')::smallint
    OR q.materialization_policy IS DISTINCT FROM (plan->>'materialization_policy')::smallint
    OR q.fs_semantics IS DISTINCT FROM (plan->>'fs_semantics')::smallint
    OR q.access_projection IS DISTINCT FROM (plan->>'access_projection')::smallint
    OR q.verification_revision IS DISTINCT FROM (plan->>'verification_revision')::integer
    OR q.projection_revision IS DISTINCT FROM (plan->>'projection_revision')::smallint
    OR q.metadata_root IS DISTINCT FROM decode(plan->>'root','hex')
    OR q.node_count<>jsonb_array_length(plan->'delta') OR q.edge_count<>jsonb_array_length(plan->'edges')
    OR q.total_bytes<>(plan->>'total_delta_bytes')::bigint THEN
    RAISE EXCEPTION 'rooted preparation fields differ from its independently decoded manifest'; END IF;
  RETURN plan||jsonb_build_object('bindings',bindings);
END $$;

CREATE FUNCTION mst2_metadata_check_rooted_members(pid text) RETURNS jsonb
LANGUAGE plpgsql VOLATILE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE q mst2_metadata_prepare%ROWTYPE; plan jsonb;
BEGIN
  SELECT * INTO STRICT q FROM mst2_metadata_prepare WHERE prepare_id=pid AND plan_kind='ROOTED';
  plan:=mst2_metadata_rooted_manifest(q);
  IF EXISTS((SELECT decode(value->>'page','hex'),(value->>'generation')::bigint,(value->>'size')::integer
      FROM jsonb_array_elements(plan->'bindings')) EXCEPT
      (SELECT page_id,generation,expected_size FROM mst2_metadata_prepare_page WHERE prepare_id=pid))
    OR EXISTS((SELECT page_id,generation,expected_size FROM mst2_metadata_prepare_page WHERE prepare_id=pid) EXCEPT
      (SELECT decode(value->>'page','hex'),(value->>'generation')::bigint,(value->>'size')::integer
        FROM jsonb_array_elements(plan->'bindings')))
    OR EXISTS((SELECT decode(value->>'page','hex'),(value->>'generation')::bigint,(value->>'attestation_id')::uuid,
        decode(value->>'attestation_digest','hex'),decode(value->>'certificate_digest','hex')
        FROM jsonb_array_elements(plan->'reused')) EXCEPT
      (SELECT r.root_page,r.root_generation,r.attestation_id,r.attestation_digest,a.root_certificate_digest
        FROM mst2_metadata_prepare_reuse_root r JOIN mst2_metadata_source_root_attestation a USING(attestation_id)
        WHERE r.prepare_id=pid))
    OR EXISTS((SELECT r.root_page,r.root_generation,r.attestation_id,r.attestation_digest,a.root_certificate_digest
        FROM mst2_metadata_prepare_reuse_root r JOIN mst2_metadata_source_root_attestation a USING(attestation_id)
        WHERE r.prepare_id=pid) EXCEPT
      (SELECT decode(value->>'page','hex'),(value->>'generation')::bigint,(value->>'attestation_id')::uuid,
        decode(value->>'attestation_digest','hex'),decode(value->>'certificate_digest','hex')
        FROM jsonb_array_elements(plan->'reused'))) THEN
    RAISE EXCEPTION 'rooted preparation has missing or extra exact delta/reuse lifetime bindings'; END IF;
  RETURN plan;
END $$;

CREATE FUNCTION mst2_metadata_rooted_members_complete() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF EXISTS(SELECT 1 FROM mst2_metadata_prepare WHERE prepare_id=NEW.prepare_id AND plan_kind='ROOTED') THEN
    PERFORM mst2_metadata_check_rooted_members(NEW.prepare_id);
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_rooted_prepare_complete AFTER INSERT OR UPDATE OF bindings_revision ON mst2_metadata_prepare
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_rooted_members_complete();

CREATE FUNCTION mst2_metadata_rooted_members_added() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  -- Every actual membership statement queues one authoritative commit check
  -- per affected prepare, including a late raw-SQL addition. No caller state
  -- can disable the decoded exact-coverage proof.
  UPDATE mst2_metadata_prepare q SET bindings_revision=q.bindings_revision+1
    WHERE q.plan_kind='ROOTED' AND q.prepare_id IN (SELECT DISTINCT prepare_id FROM added_members);
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_rooted_delta_added AFTER INSERT ON mst2_metadata_prepare_page
  REFERENCING NEW TABLE AS added_members FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_rooted_members_added();
CREATE TRIGGER mst2_metadata_rooted_reuse_added AFTER INSERT ON mst2_metadata_prepare_reuse_root
  REFERENCING NEW TABLE AS added_members FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_rooted_members_added();

CREATE TABLE mst2_metadata_scope_source_reference (
  prepare_id text PRIMARY KEY REFERENCES mst2_metadata_prepare(prepare_id),
  scope_tree_oid text NOT NULL CHECK(scope_tree_oid ~ '^(sha1:[0-9a-f]{40}|sha256:[0-9a-f]{64}|blake3:[0-9a-f]{64})$'),
  ancestor_revisions jsonb NOT NULL CHECK((jsonb_typeof(ancestor_revisions)='array' AND jsonb_array_length(ancestor_revisions)<=256) IS TRUE)
);
CREATE FUNCTION mst2_metadata_derive_scope_source(pid text) RETURNS jsonb LANGUAGE plpgsql VOLATILE STRICT
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE q mst2_metadata_prepare%ROWTYPE; tree_oid text; component text; body bytea; item jsonb; scanned_bytes bigint:=0;
  digest bytea; revision uuid; ancestors jsonb[]:=ARRAY[]::jsonb[];
BEGIN
  SELECT * INTO STRICT q FROM mst2_metadata_prepare WHERE prepare_id=pid AND plan_kind='ROOTED' AND state='PREPARING';
  tree_oid:=q.tagged_root_tree_oid;
  IF q.scope='/' THEN RETURN jsonb_build_object('scope_tree_oid',tree_oid,'ancestor_revisions','[]'::jsonb); END IF;
  FOREACH component IN ARRAY string_to_array(substring(q.scope FROM 2),'/') LOOP
    SELECT sub_trees INTO body FROM $CORE_SCHEMA$.mega_tree WHERE tree_id=split_part(tree_oid,':',2);
    IF NOT FOUND THEN RAISE EXCEPTION 'rooted source scope has a missing fixed ancestor'; END IF;
    scanned_bytes:=scanned_bytes+octet_length(body);
    IF scanned_bytes>67108864 THEN RAISE EXCEPTION 'rooted source-scope walk exceeds its fixed byte-work budget'; END IF;
    digest:=sha256(body);
    revision:=$CORE_SCHEMA$.mst2_route_capture_source_tree(split_part(tree_oid,':',2),digest);
    ancestors:=array_append(ancestors,jsonb_build_object('tree_oid',tree_oid,'revision',revision::text,'body_digest',encode(digest,'hex')));
    SELECT value INTO item FROM jsonb_array_elements(mst2_metadata_decode_git_tree(body,split_part(tree_oid,':',1)))
      WHERE decode(value->>'name','hex')=convert_to(component,'UTF8');
    IF NOT FOUND OR (item->>'kind')::integer<>4 THEN RAISE EXCEPTION 'rooted source scope is not its exact fixed directory'; END IF;
    tree_oid:=item->>'oid';
  END LOOP;
  RETURN jsonb_build_object('scope_tree_oid',tree_oid,'ancestor_revisions',to_jsonb(ancestors));
END $$;
CREATE FUNCTION mst2_metadata_scope_source_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE proof jsonb;
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'rooted scope source proof is immutable'; END IF;
  proof:=mst2_metadata_derive_scope_source(NEW.prepare_id);
  IF NEW.scope_tree_oid IS DISTINCT FROM proof->>'scope_tree_oid'
    OR NEW.ancestor_revisions IS DISTINCT FROM proof->'ancestor_revisions' THEN
    RAISE EXCEPTION 'rooted scope source proof was not independently derived from actual core ancestors'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_scope_source_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_scope_source_reference
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_scope_source_guard();

CREATE FUNCTION mst2_metadata_rooted_scope_tree(pid text) RETURNS text LANGUAGE plpgsql VOLATILE STRICT
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE bound mst2_metadata_scope_source_reference%ROWTYPE; proof jsonb; ancestor jsonb;
BEGIN
  SELECT * INTO bound FROM mst2_metadata_scope_source_reference WHERE prepare_id=pid;
  IF NOT FOUND THEN
    proof:=mst2_metadata_derive_scope_source(pid);
    INSERT INTO mst2_metadata_scope_source_reference(prepare_id,scope_tree_oid,ancestor_revisions)
      VALUES(pid,proof->>'scope_tree_oid',proof->'ancestor_revisions') RETURNING * INTO bound;
  END IF;
  FOR ancestor IN SELECT value FROM jsonb_array_elements(bound.ancestor_revisions) LOOP
    IF NOT $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(ancestor->>'tree_oid',':',2),
      (ancestor->>'revision')::uuid,decode(ancestor->>'body_digest','hex')) THEN
      RAISE EXCEPTION 'rooted fixed scope ancestor lost its exact current source revision'; END IF;
  END LOOP;
  RETURN bound.scope_tree_oid;
END $$;

CREATE FUNCTION mst2_metadata_rooted_finalize_proof(pid text) RETURNS jsonb LANGUAGE plpgsql VOLATILE STRICT
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE q mst2_metadata_prepare%ROWTYPE; plan jsonb; selected_root_generation bigint; root_certificate bytea;
  profile jsonb; source jsonb; source_generation bigint; scoped_tree text;
  generation_index jsonb;
BEGIN
  SELECT * INTO STRICT q FROM mst2_metadata_prepare WHERE prepare_id=pid AND plan_kind='ROOTED' AND state='PREPARING';
  plan:=mst2_metadata_check_rooted_members(pid); profile:=mst2_metadata_native_profile(pid);
  SELECT jsonb_object_agg(value->>'page',value->'generation') INTO generation_index FROM (
    SELECT value FROM jsonb_array_elements(plan->'bindings') UNION ALL
    SELECT value FROM jsonb_array_elements(plan->'reused')) members;
  selected_root_generation:=(generation_index->>(plan->>'root'))::bigint;
  SELECT c.certificate_digest INTO root_certificate FROM mst2_metadata_page_certificate c
    JOIN mst2_metadata_graph_node n USING(page_id,generation) JOIN mst2_metadata_current cur USING(page_id,generation)
    JOIN mst2_metadata_lifetime life USING(page_id,generation)
    WHERE c.page_id=q.metadata_root AND c.generation=selected_root_generation AND n.state='LIVE'
      AND n.certificate_digest=c.certificate_digest AND life.state IN ('RESERVED','LIVE') AND life.graph_domain='qualified-v1'
      AND c.relative_path_bytes+CASE WHEN q.scope='/' THEN 0 ELSE octet_length(q.scope) END<=4096
      AND c.relative_components+CASE WHEN q.scope='/' THEN 0 ELSE cardinality(string_to_array(substring(q.scope FROM 2),'/')) END<=256
      AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=c.page_id AND gc.generation=c.generation);
  IF NOT FOUND OR NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.anchor_kind='PREPARE'
      AND a.owner_key=pid AND a.prepare_id=pid AND a.root_page=q.metadata_root AND a.root_generation=selected_root_generation
      AND a.root_certificate_digest=root_certificate) THEN RAISE EXCEPTION 'rooted finalize lacks its independently certified owned root'; END IF;
  IF EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m
      LEFT JOIN mst2_metadata_current cur USING(page_id,generation) LEFT JOIN mst2_metadata_lifetime life USING(page_id,generation)
      LEFT JOIN mst2_metadata_payload body USING(page_id,generation) LEFT JOIN mst2_metadata_graph_node n USING(page_id,generation)
      LEFT JOIN mst2_metadata_page_certificate c USING(page_id,generation)
      WHERE m.prepare_id=pid AND (cur.page_id IS NULL OR life.page_id IS NULL OR body.page_id IS NULL OR n.page_id IS NULL OR c.page_id IS NULL
        OR life.state NOT IN ('RESERVED','LIVE') OR life.graph_domain<>'qualified-v1' OR life.metadata_codec<>q.metadata_codec
        OR body.metadata_codec<>q.metadata_codec OR n.metadata_codec<>q.metadata_codec OR c.metadata_codec<>q.metadata_codec
        OR life.expected_size<>m.expected_size OR body.byte_size<>m.expected_size OR n.bytes<>m.expected_size OR c.byte_size<>m.expected_size
        OR n.state<>'LIVE' OR n.certificate_digest<>c.certificate_digest
        OR life.state='RESERVED' AND c.origin_prepare_id<>pid
        OR n.incoming_refs<>(SELECT count(*) FROM mst2_metadata_graph_edge e WHERE e.child_page=m.page_id AND e.child_generation=m.generation)
        OR EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=m.page_id AND gc.generation=m.generation))) THEN
    RAISE EXCEPTION 'rooted finalize lost its exact durable canonical delta graph'; END IF;
  IF EXISTS((SELECT decode(value->>'parent','hex'),decode(value->>'child','hex') FROM jsonb_array_elements(plan->'edges')) EXCEPT
      (SELECT e.parent_page,e.child_page FROM mst2_metadata_prepare_page m JOIN mst2_metadata_graph_edge e
        ON e.parent_page=m.page_id AND e.parent_generation=m.generation WHERE m.prepare_id=pid))
    OR EXISTS((SELECT e.parent_page,e.child_page FROM mst2_metadata_prepare_page m JOIN mst2_metadata_graph_edge e
        ON e.parent_page=m.page_id AND e.parent_generation=m.generation WHERE m.prepare_id=pid) EXCEPT
      (SELECT decode(value->>'parent','hex'),decode(value->>'child','hex') FROM jsonb_array_elements(plan->'edges')))
    OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_reuse_root r WHERE r.prepare_id=pid AND NOT EXISTS(
      SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.prepare_id=pid AND a.owner_key=pid AND a.anchor_kind='REUSE'
        AND a.root_page=r.root_page AND a.root_generation=r.root_generation)) THEN
    RAISE EXCEPTION 'rooted finalize differs from its exact delta edges or owned reuse boundaries'; END IF;
  scoped_tree:=mst2_metadata_rooted_scope_tree(pid);
  IF NOT EXISTS(SELECT 1 FROM jsonb_array_elements(plan->'source_roots') binding(value)
      WHERE value->>'tree_oid'=scoped_tree AND value->>'page'=plan->>'root') THEN
    RAISE EXCEPTION 'rooted metadata root is not bound to its independently selected fixed source scope'; END IF;
  FOR source IN SELECT value FROM jsonb_array_elements(plan->'source_roots') LOOP
    source_generation:=(generation_index->>(source->>'page'))::bigint;
    IF NOT EXISTS(SELECT 1 FROM mst2_metadata_source_root_attestation a
      JOIN mst2_metadata_current cur ON cur.page_id=a.root_page AND cur.generation=a.root_generation
      JOIN mst2_metadata_lifetime life USING(page_id,generation) JOIN mst2_metadata_graph_node n USING(page_id,generation)
      JOIN mst2_metadata_prepare origin ON origin.prepare_id=a.origin_prepare_id
      JOIN $CORE_SCHEMA$.mega_tree t ON t.tree_id=split_part(a.tagged_tree_oid,':',2)
      WHERE a.tagged_tree_oid=source->>'tree_oid' AND a.root_page=decode(source->>'page','hex') AND a.root_generation=source_generation
        AND a.source_profile=profile
        AND $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest) AND n.state='LIVE'
        AND n.certificate_digest=a.root_certificate_digest
        AND (a.origin_prepare_id=pid AND origin.state='PREPARING' AND life.state IN ('RESERVED','LIVE')
          OR origin.state='COMMITTED' AND life.state='LIVE' AND EXISTS(SELECT 1 FROM mst2_metadata_prepare_reuse_root r
            WHERE r.prepare_id=pid AND r.root_page=a.root_page AND r.root_generation=a.root_generation
              AND EXISTS(SELECT 1 FROM mst2_metadata_source_root_attestation boundary
                WHERE boundary.attestation_id=r.attestation_id AND boundary.root_certificate_digest=a.root_certificate_digest)))) THEN
      RAISE EXCEPTION 'rooted source binding lacks its independently attested exact current directory'; END IF;
  END LOOP;
  RETURN jsonb_build_object('root',plan->>'root','generation',selected_root_generation,'certificate',encode(root_certificate,'hex'),
    'scoped_tree_oid',scoped_tree,'delta_nodes',q.node_count,'delta_edges',q.edge_count,'delta_bytes',q.total_bytes);
END $$;
