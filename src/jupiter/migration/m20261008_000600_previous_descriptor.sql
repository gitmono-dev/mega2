CREATE FUNCTION mst2_metadata_descriptor(pid text,instance text,commit_id text,tree_id text)
RETURNS bytea LANGUAGE plpgsql VOLATILE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE p record; scope_bytes bytea; view_digest bytea; instance_bytes bytea;
BEGIN
  SELECT tagged_root_tree_oid,scope,metadata_root INTO p FROM mst2_metadata_prepare WHERE prepare_id=pid AND plan_kind='ROOTED'
    AND state='COMMITTED' AND graph_domain='qualified-v1' AND mst2_metadata_scope_matches(primary_scope);
  IF NOT FOUND OR instance IS DISTINCT FROM (instance::uuid)::text
    OR split_part(p.tagged_root_tree_oid,':',2) IS DISTINCT FROM tree_id
    OR NOT EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mega_commit c WHERE c.commit_id=$3 AND c.tree=$4)
    OR octet_length(commit_id)<>octet_length(tree_id)
    OR commit_id !~ '^([0-9a-f]{40}|[0-9a-f]{64})$' THEN
    RAISE EXCEPTION 'qualified descriptor has no exact canonical instance and fixed source';
  END IF;
  PERFORM mst2_metadata_native_profile(pid);
  scope_bytes:=convert_to(p.scope,'UTF8');
  IF p.scope !~ '^/' OR octet_length(scope_bytes)>4096 OR p.scope<>'/' AND
      (p.scope ~ '/$|//|/(\.|\.\.)(/|$)' OR cardinality(string_to_array(substring(p.scope FROM 2),'/'))>256)
    OR EXISTS(SELECT 1 FROM unnest(string_to_array(substring(p.scope FROM 2),'/')) component
      WHERE octet_length(component)>255) THEN
    RAISE EXCEPTION 'qualified descriptor scope is not canonical';
  END IF;
  instance_bytes:=decode(replace(instance,'-',''),'hex');
  view_digest:=sha256(convert_to('mega.mst2.namespaceview','UTF8')||decode('00','hex')||convert_to(commit_id,'UTF8'));
  RETURN convert_to('MSD2','UTF8')||decode('00020001','hex')||instance_bytes||view_digest
    ||decode(lpad(to_hex(octet_length(scope_bytes)),4,'0'),'hex')||scope_bytes
    ||decode('0001000100000000','hex')||p.metadata_root;
END $$;
