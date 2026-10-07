CREATE TABLE mst2_metadata_page_certificate (
  page_id bytea NOT NULL,generation bigint NOT NULL,certificate_digest bytea NOT NULL CHECK(octet_length(certificate_digest)=32),
  origin_prepare_id text NOT NULL REFERENCES mst2_metadata_prepare(prepare_id),
  namespace_uuid uuid NOT NULL REFERENCES mst2_metadata_family_identity(namespace_uuid),
  proof_revision integer NOT NULL CHECK(proof_revision=1),metadata_codec smallint NOT NULL CHECK(metadata_codec=1),
  byte_size integer NOT NULL CHECK(byte_size BETWEEN 20 AND 16384),
  rank integer NOT NULL CHECK(rank BETWEEN 0 AND 4095),
  map_entry_count bigint NOT NULL CHECK(map_entry_count BETWEEN 0 AND 131072),
  map_encoded_entry_bytes bigint NOT NULL CHECK(map_encoded_entry_bytes BETWEEN 0 AND 67108864),
  min_name bytea,max_name bytea,
  relative_path_bytes integer NOT NULL CHECK(relative_path_bytes BETWEEN 0 AND 4096),
  relative_components integer NOT NULL CHECK(relative_components BETWEEN 0 AND 256),
  closure_nodes_upper integer NOT NULL CHECK(closure_nodes_upper BETWEEN 1 AND 4096),
  closure_edges_upper integer NOT NULL CHECK(closure_edges_upper BETWEEN 0 AND 16384),
  closure_bytes_upper bigint NOT NULL CHECK(closure_bytes_upper BETWEEN 20 AND 67108864),
  closure_entries_upper bigint NOT NULL CHECK(closure_entries_upper BETWEEN 0 AND 131072),
  canonical_proof jsonb NOT NULL,
  PRIMARY KEY(page_id,generation),UNIQUE(page_id,generation,certificate_digest),
  FOREIGN KEY(page_id,generation) REFERENCES mst2_metadata_lifetime(page_id,generation),
  CHECK((map_entry_count=0)=(min_name IS NULL AND max_name IS NULL)),
  CHECK(map_entry_count=0 OR (min_name IS NOT NULL AND max_name IS NOT NULL AND min_name<=max_name))
);
CREATE TABLE mst2_metadata_verified_ref (
  parent_page bytea NOT NULL,parent_generation bigint NOT NULL,
  reference_ordinal integer NOT NULL CHECK(reference_ordinal BETWEEN 0 AND 256),
  reference_kind text NOT NULL CHECK(reference_kind IN ('DIRECTORY','RADIX')),
  name bytea,label integer,advertised_count bigint,
  child_page bytea NOT NULL,child_generation bigint NOT NULL,
  child_certificate_digest bytea NOT NULL CHECK(octet_length(child_certificate_digest)=32),
  PRIMARY KEY(parent_page,parent_generation,reference_ordinal),
  FOREIGN KEY(parent_page,parent_generation) REFERENCES mst2_metadata_page_certificate(page_id,generation),
  FOREIGN KEY(child_page,child_generation,child_certificate_digest)
    REFERENCES mst2_metadata_page_certificate(page_id,generation,certificate_digest),
  CHECK((reference_kind='DIRECTORY' AND name IS NOT NULL AND label IS NULL AND advertised_count IS NULL)
    OR (reference_kind='RADIX' AND name IS NULL AND label IS NOT NULL AND advertised_count IS NOT NULL
      AND label BETWEEN 0 AND 255 AND advertised_count>0))
);
CREATE INDEX mst2_metadata_verified_ref_child ON mst2_metadata_verified_ref(child_page,child_generation,parent_page,parent_generation);
ALTER TABLE mst2_metadata_graph_node ADD COLUMN certificate_digest bytea NOT NULL,
  ADD FOREIGN KEY(page_id,generation,certificate_digest)
    REFERENCES mst2_metadata_page_certificate(page_id,generation,certificate_digest);

CREATE FUNCTION mst2_metadata_child_certificate(p bytea,g bigint,pid text) RETURNS jsonb
LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE c mst2_metadata_page_certificate%ROWTYPE;
BEGIN
  SELECT proof.* INTO c FROM mst2_metadata_page_certificate proof
    JOIN mst2_metadata_current cur USING(page_id,generation)
    JOIN mst2_metadata_lifetime life USING(page_id,generation)
    JOIN mst2_metadata_graph_node node USING(page_id,generation)
    JOIN mst2_metadata_payload body USING(page_id,generation)
    WHERE proof.page_id=p AND proof.generation=g AND life.graph_domain='qualified-v1'
      AND node.state='LIVE' AND node.certificate_digest=proof.certificate_digest
      AND node.metadata_codec=proof.metadata_codec AND body.metadata_codec=proof.metadata_codec
      AND body.byte_size=proof.byte_size AND node.bytes=proof.byte_size AND life.expected_size=proof.byte_size
      AND (life.state='LIVE' OR (life.state='RESERVED' AND proof.origin_prepare_id=pid
        AND EXISTS(SELECT 1 FROM mst2_metadata_prepare q WHERE q.prepare_id=pid AND q.state='PREPARING')))
      AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op op WHERE op.page_id=p AND op.generation=g);
  IF NOT FOUND THEN RAISE EXCEPTION 'MTP2 child lacks its exact certified current graph'; END IF;
  RETURN jsonb_build_object('page',encode(c.page_id,'hex'),'generation',c.generation,
    'certificate',encode(c.certificate_digest,'hex'),'rank',c.rank,'map_entry_count',c.map_entry_count,
    'map_encoded_entry_bytes',c.map_encoded_entry_bytes,'min_name',encode(c.min_name,'hex'),
    'max_name',encode(c.max_name,'hex'),'relative_path_bytes',c.relative_path_bytes,
    'relative_components',c.relative_components,'closure_nodes_upper',c.closure_nodes_upper,
    'closure_edges_upper',c.closure_edges_upper,'closure_bytes_upper',c.closure_bytes_upper,
    'closure_entries_upper',c.closure_entries_upper);
END $$;

CREATE FUNCTION mst2_metadata_compute_certificate(p bytea,g bigint,pid text) RETURNS jsonb
LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE body mst2_metadata_payload%ROWTYPE; q mst2_metadata_prepare%ROWTYPE; decoded jsonb;
  ref jsonb; entry jsonb; child jsonb; bound jsonb; bound_refs jsonb:='[]'::jsonb;
  seen jsonb:='[]'::jsonb; children jsonb:='{}'::jsonb; child_page bytea; child_generation bigint;
  key text; name bytea; child_min bytea; child_max bytea; prefix bytea; minimum bytea; maximum bytea;
  map_count bigint:=0; map_bytes bigint:=0; path_bytes integer:=0; components integer:=0; rank integer:=0;
  nodes bigint:=1; edges bigint:=0; bytes bigint; entries bigint; proof jsonb;
BEGIN
  SELECT * INTO q FROM mst2_metadata_prepare WHERE prepare_id=pid AND state='PREPARING'
    AND graph_domain='qualified-v1' AND mst2_metadata_scope_matches(primary_scope);
  IF NOT FOUND THEN RAISE EXCEPTION 'MTP2 certification needs an active exact preparation'; END IF;
  SELECT b.* INTO body FROM mst2_metadata_payload b JOIN mst2_metadata_current cur USING(page_id,generation)
    JOIN mst2_metadata_lifetime life USING(page_id,generation)
    JOIN mst2_metadata_prepare_page member USING(page_id,generation)
    WHERE b.page_id=p AND b.generation=g AND member.prepare_id=pid AND life.state='RESERVED'
      AND life.graph_domain='qualified-v1' AND life.metadata_codec=q.metadata_codec
      AND b.metadata_codec=q.metadata_codec AND b.byte_size=member.expected_size AND b.byte_size=life.expected_size
      AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op op WHERE op.page_id=p AND op.generation=g);
  IF NOT FOUND THEN RAISE EXCEPTION 'MTP2 certification crossed its durable payload lifetime'; END IF;
  decoded:=mst2_metadata_decode_local(body.payload);
  IF decode(decoded->>'page_id','hex')<>p THEN RAISE EXCEPTION 'MTP2 durable payload digest differs from its page'; END IF;
  map_count:=jsonb_array_length(decoded->'entries'); map_bytes:=(decoded->>'direct_entry_bytes')::bigint;
  bytes:=body.byte_size; entries:=map_count;
  FOR ref IN SELECT value FROM jsonb_array_elements(decoded->'refs') LOOP
    child_page:=decode(ref->>'child','hex');
    SELECT member.generation INTO child_generation FROM mst2_metadata_prepare_page member
      WHERE member.prepare_id=pid AND member.page_id=child_page;
    IF NOT FOUND THEN
      SELECT reused.root_generation INTO child_generation FROM mst2_metadata_prepare_reuse_root reused
        JOIN mst2_metadata_root_anchor anchor ON anchor.prepare_id=reused.prepare_id
          AND anchor.anchor_kind='REUSE' AND anchor.owner_key=pid
          AND anchor.root_page=reused.root_page AND anchor.root_generation=reused.root_generation
        WHERE reused.prepare_id=pid AND reused.root_page=child_page;
      IF NOT FOUND THEN RAISE EXCEPTION 'MTP2 reference is outside exact delta or reused-root membership'; END IF;
    END IF;
    child:=mst2_metadata_child_certificate(child_page,child_generation,pid);
    bound:=ref||jsonb_build_object('generation',child_generation,'certificate',child->>'certificate');
    bound_refs:=bound_refs||jsonb_build_array(bound);
    key:=encode(child_page,'hex')||':'||child_generation;
    children:=children||jsonb_build_object(encode(child_page,'hex'),child);
    IF NOT seen ? key THEN
      seen:=seen||jsonb_build_array(key); rank:=greatest(rank,(child->>'rank')::integer+1);
      nodes:=nodes+(child->>'closure_nodes_upper')::bigint;
      edges:=edges+1+(child->>'closure_edges_upper')::bigint;
      bytes:=bytes+(child->>'closure_bytes_upper')::bigint;
      entries:=entries+(child->>'closure_entries_upper')::bigint;
    END IF;
    IF ref->>'kind'='RADIX' THEN
      child_min:=decode(child->>'min_name','hex'); child_max:=decode(child->>'max_name','hex');
      prefix:=decode(decoded->>'prefix','hex');
      IF child_min IS NULL OR child_max IS NULL OR (child->>'map_entry_count')::bigint<>(ref->>'count')::bigint
        OR octet_length(child_min)<=octet_length(prefix) OR octet_length(child_max)<=octet_length(prefix)
        OR substring(child_min FROM 1 FOR octet_length(prefix))<>prefix
        OR substring(child_max FROM 1 FOR octet_length(prefix))<>prefix
        OR get_byte(child_min,octet_length(prefix))<>(ref->>'label')::integer
        OR get_byte(child_max,octet_length(prefix))<>(ref->>'label')::integer THEN
        RAISE EXCEPTION 'MTP2 radix child count or name partition differs from its canonical certificate';
      END IF;
      IF minimum IS NULL OR child_min<minimum THEN minimum:=child_min; END IF;
      IF maximum IS NULL OR child_max>maximum THEN maximum:=child_max; END IF;
      map_count:=map_count+(child->>'map_entry_count')::bigint;
      map_bytes:=map_bytes+(child->>'map_encoded_entry_bytes')::bigint;
      path_bytes:=greatest(path_bytes,(child->>'relative_path_bytes')::integer);
      components:=greatest(components,(child->>'relative_components')::integer);
    END IF;
    IF rank>4095 OR nodes>4096 OR edges>16384 OR bytes>67108864 OR entries>131072 THEN
      RAISE EXCEPTION 'MTP2 certified closure upper bound exceeds its fixed budget';
    END IF;
  END LOOP;
  FOR entry IN SELECT value FROM jsonb_array_elements(decoded->'entries') LOOP
    name:=decode(entry->>'name','hex');
    IF minimum IS NULL OR name<minimum THEN minimum:=name; END IF;
    IF maximum IS NULL OR name>maximum THEN maximum:=name; END IF;
    IF (entry->>'kind')::integer=4 THEN
      child:=children->(entry->>'child');
      path_bytes:=greatest(path_bytes,1+octet_length(name)+(child->>'relative_path_bytes')::integer);
      components:=greatest(components,1+(child->>'relative_components')::integer);
    ELSE
      path_bytes:=greatest(path_bytes,1+octet_length(name)); components:=greatest(components,1);
    END IF;
  END LOOP;
  IF map_count<>(decoded->>'count')::bigint OR map_count>131072 OR map_bytes>67108864
    OR path_bytes>4096 OR components>256 THEN RAISE EXCEPTION 'MTP2 canonical map or path budget is invalid'; END IF;
  IF (decoded->>'kind')::integer=1 AND (
      (map_count<=128 AND 20+map_bytes<=16384)
      OR decode(decoded->>'prefix','hex') IS DISTINCT FROM mst2_metadata_lcp(minimum,maximum)) THEN
    RAISE EXCEPTION 'MTP2 branch is leafable or its prefix is not the true canonical LCP';
  END IF;
  proof:=jsonb_build_object('namespace',(SELECT namespace_uuid::text FROM mst2_metadata_family_identity WHERE singleton=1),
    'page',encode(p,'hex'),'generation',g,'codec',body.metadata_codec,'byte_size',body.byte_size,
    'proof_revision',1,'page_kind',(decoded->>'kind')::integer,'references',bound_refs,'rank',rank,
    'map_entry_count',map_count,'map_encoded_entry_bytes',map_bytes,'min_name',encode(minimum,'hex'),
    'max_name',encode(maximum,'hex'),'relative_path_bytes',path_bytes,'relative_components',components,
    'closure_nodes_upper',nodes,'closure_edges_upper',edges,'closure_bytes_upper',bytes,'closure_entries_upper',entries);
  RETURN proof||jsonb_build_object('certificate',encode(sha256(convert_to('mega.mst2.canonical-proof.v1','UTF8')
    ||decode('00','hex')||convert_to(proof::text,'UTF8')),'hex'));
END $$;

CREATE FUNCTION mst2_metadata_certificate_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE proof jsonb;
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'MTP2 canonical certificate history is immutable'; END IF;
  proof:=mst2_metadata_compute_certificate(NEW.page_id,NEW.generation,NEW.origin_prepare_id);
  IF NEW.certificate_digest<>decode(proof->>'certificate','hex') OR NEW.canonical_proof<>proof
    OR NEW.namespace_uuid::text<>proof->>'namespace' OR NEW.proof_revision<>1 OR NEW.metadata_codec<>(proof->>'codec')::smallint
    OR NEW.byte_size<>(proof->>'byte_size')::integer OR NEW.rank<>(proof->>'rank')::integer
    OR NEW.map_entry_count<>(proof->>'map_entry_count')::bigint
    OR NEW.map_encoded_entry_bytes<>(proof->>'map_encoded_entry_bytes')::bigint
    OR NEW.min_name IS DISTINCT FROM decode(proof->>'min_name','hex')
    OR NEW.max_name IS DISTINCT FROM decode(proof->>'max_name','hex')
    OR NEW.relative_path_bytes<>(proof->>'relative_path_bytes')::integer
    OR NEW.relative_components<>(proof->>'relative_components')::integer
    OR NEW.closure_nodes_upper<>(proof->>'closure_nodes_upper')::integer
    OR NEW.closure_edges_upper<>(proof->>'closure_edges_upper')::integer
    OR NEW.closure_bytes_upper<>(proof->>'closure_bytes_upper')::bigint
    OR NEW.closure_entries_upper<>(proof->>'closure_entries_upper')::bigint THEN
    RAISE EXCEPTION 'MTP2 canonical certificate was not derived from its durable bytes and exact child proofs';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_certificate_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_page_certificate
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_certificate_guard();

CREATE FUNCTION mst2_metadata_verified_ref_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE ref jsonb;
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'MTP2 canonical reference history is immutable'; END IF;
  SELECT canonical_proof->'references'->NEW.reference_ordinal INTO ref FROM mst2_metadata_page_certificate
    WHERE page_id=NEW.parent_page AND generation=NEW.parent_generation;
  IF ref IS NULL OR NEW.reference_kind<>ref->>'kind' OR NEW.child_page<>decode(ref->>'child','hex')
    OR NEW.child_generation<>(ref->>'generation')::bigint
    OR NEW.child_certificate_digest<>decode(ref->>'certificate','hex')
    OR NEW.name IS DISTINCT FROM decode(ref->>'name','hex')
    OR NEW.label IS DISTINCT FROM (ref->>'label')::integer
    OR NEW.advertised_count IS DISTINCT FROM (ref->>'count')::bigint THEN
    RAISE EXCEPTION 'MTP2 canonical reference differs from its independently derived occurrence';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_verified_ref_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_verified_ref
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_verified_ref_guard();

CREATE FUNCTION mst2_metadata_certificate_complete() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE p bytea; g bigint; proof jsonb;
BEGIN
  IF TG_TABLE_NAME IN ('mst2_metadata_verified_ref','mst2_metadata_graph_edge') THEN p:=NEW.parent_page; g:=NEW.parent_generation;
  ELSE p:=NEW.page_id; g:=NEW.generation; END IF;
  SELECT canonical_proof INTO proof FROM mst2_metadata_page_certificate WHERE page_id=p AND generation=g;
  IF proof IS NULL OR (SELECT count(*) FROM mst2_metadata_verified_ref WHERE parent_page=p AND parent_generation=g)
      <>jsonb_array_length(proof->'references')
    OR NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_node n WHERE n.page_id=p AND n.generation=g
      AND n.state='LIVE' AND n.certificate_digest=decode(proof->>'certificate','hex'))
    OR EXISTS((SELECT child_page,child_generation FROM mst2_metadata_verified_ref WHERE parent_page=p AND parent_generation=g)
      EXCEPT (SELECT child_page,child_generation FROM mst2_metadata_graph_edge WHERE parent_page=p AND parent_generation=g))
    OR EXISTS((SELECT child_page,child_generation FROM mst2_metadata_graph_edge WHERE parent_page=p AND parent_generation=g)
      EXCEPT (SELECT child_page,child_generation FROM mst2_metadata_verified_ref WHERE parent_page=p AND parent_generation=g)) THEN
    RAISE EXCEPTION 'MTP2 canonical certificate must commit with its complete exact graph and typed references';
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_certificate_complete AFTER INSERT ON mst2_metadata_page_certificate
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_certificate_complete();
CREATE CONSTRAINT TRIGGER mst2_metadata_verified_refs_complete AFTER INSERT ON mst2_metadata_verified_ref
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_certificate_complete();
CREATE CONSTRAINT TRIGGER mst2_metadata_graph_refs_complete AFTER INSERT ON mst2_metadata_graph_edge
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_certificate_complete();

CREATE FUNCTION mst2_metadata_certify_page(pid text,p bytea,g bigint) RETURNS bytea LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE proof jsonb; existing bytea;
BEGIN
  SELECT c.certificate_digest INTO existing FROM mst2_metadata_page_certificate c
    JOIN mst2_metadata_current cur USING(page_id,generation) JOIN mst2_metadata_lifetime life USING(page_id,generation)
    JOIN mst2_metadata_graph_node n USING(page_id,generation) JOIN mst2_metadata_prepare_page member USING(page_id,generation)
    JOIN mst2_metadata_prepare q USING(prepare_id)
    WHERE c.page_id=p AND c.generation=g AND member.prepare_id=pid AND q.state='PREPARING'
      AND mst2_metadata_scope_matches(q.primary_scope) AND life.state='LIVE' AND life.graph_domain='qualified-v1'
      AND n.state='LIVE' AND n.certificate_digest=c.certificate_digest AND n.bytes=member.expected_size
      AND n.metadata_codec=q.metadata_codec
      AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op op WHERE op.page_id=p AND op.generation=g);
  IF FOUND THEN RETURN existing; END IF;
  proof:=mst2_metadata_compute_certificate(p,g,pid);
  INSERT INTO mst2_metadata_page_certificate(page_id,generation,certificate_digest,origin_prepare_id,namespace_uuid,
    proof_revision,metadata_codec,byte_size,rank,map_entry_count,map_encoded_entry_bytes,min_name,max_name,
    relative_path_bytes,relative_components,closure_nodes_upper,closure_edges_upper,closure_bytes_upper,closure_entries_upper,canonical_proof)
    VALUES(p,g,decode(proof->>'certificate','hex'),pid,(proof->>'namespace')::uuid,1,(proof->>'codec')::smallint,
      (proof->>'byte_size')::integer,(proof->>'rank')::integer,(proof->>'map_entry_count')::bigint,
      (proof->>'map_encoded_entry_bytes')::bigint,decode(proof->>'min_name','hex'),decode(proof->>'max_name','hex'),
      (proof->>'relative_path_bytes')::integer,(proof->>'relative_components')::integer,
      (proof->>'closure_nodes_upper')::integer,(proof->>'closure_edges_upper')::integer,
      (proof->>'closure_bytes_upper')::bigint,(proof->>'closure_entries_upper')::bigint,proof);
  INSERT INTO mst2_metadata_verified_ref(parent_page,parent_generation,reference_ordinal,reference_kind,
    name,label,advertised_count,child_page,child_generation,child_certificate_digest)
    SELECT p,g,ordinality::integer-1,value->>'kind',decode(value->>'name','hex'),(value->>'label')::integer,
      (value->>'count')::bigint,decode(value->>'child','hex'),(value->>'generation')::bigint,decode(value->>'certificate','hex')
      FROM jsonb_array_elements(proof->'references') WITH ORDINALITY refs(value,ordinality);
  INSERT INTO mst2_metadata_graph_node(page_id,generation,state,metadata_codec,bytes,incoming_refs,certificate_digest)
    VALUES(p,g,'LIVE',(proof->>'codec')::smallint,(proof->>'byte_size')::integer,0,decode(proof->>'certificate','hex'));
  INSERT INTO mst2_metadata_graph_edge(parent_page,parent_generation,child_page,child_generation)
    SELECT DISTINCT p,g,child_page,child_generation FROM mst2_metadata_verified_ref WHERE parent_page=p AND parent_generation=g;
  RETURN decode(proof->>'certificate','hex');
END $$;

CREATE FUNCTION mst2_metadata_certify_batch(pid text,members jsonb) RETURNS integer LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE member jsonb; certified integer:=0;
BEGIN
  IF members IS NULL OR jsonb_typeof(members)<>'array' OR jsonb_array_length(members) NOT BETWEEN 1 AND 4096 THEN
    RAISE EXCEPTION 'canonical certification batch exceeds its fixed member budget';
  END IF;
  -- Array order is supplied by the independently validated cold DAG. Each
  -- invocation still proves its durable bytes and already certified children.
  FOR member IN SELECT value FROM jsonb_array_elements(members) WITH ORDINALITY AS input(value,ordinal)
      ORDER BY ordinal LOOP
    IF jsonb_typeof(member)<>'object' OR member->>'page' !~ '^[0-9a-f]{64}$'
      OR member->>'generation' IS NULL THEN RAISE EXCEPTION 'canonical certification batch member is malformed'; END IF;
    PERFORM mst2_metadata_certify_page(pid,decode(member->>'page','hex'),(member->>'generation')::bigint);
    certified:=certified+1;
  END LOOP;
  RETURN certified;
END $$;
