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
  SELECT coalesce(jsonb_object_agg(parent,children),'{}'::jsonb) INTO adjacency FROM (
    SELECT value->>'parent' AS parent,jsonb_agg(value->>'child') AS children FROM unnest(edge_rows) e(value)
      GROUP BY value->>'parent') grouped;
  IF EXISTS(WITH RECURSIVE reached(page) AS (SELECT encode(root,'hex') UNION
      SELECT child FROM reached r CROSS JOIN LATERAL jsonb_array_elements_text(adjacency->r.page) children(child))
    SELECT 1 FROM (SELECT d.value FROM unnest(delta_rows) d(value) UNION ALL SELECT r.value FROM unnest(reuse_rows) r(value)) nodes
      WHERE NOT EXISTS(SELECT 1 FROM reached r WHERE r.page=nodes.value->>'page')) THEN
    RAISE EXCEPTION 'rooted plan includes members outside its bounded delta and boundary closure';
  END IF;
  RETURN identity||jsonb_build_object('root',encode(root,'hex'),'delta',to_jsonb(delta_rows),'edges',to_jsonb(edge_rows),
    'reused',to_jsonb(reuse_rows),'source_roots',to_jsonb(source_rows),'total_delta_bytes',total_bytes);
END $$;