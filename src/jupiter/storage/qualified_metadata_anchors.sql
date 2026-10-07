CREATE TABLE mst2_metadata_source_root_attestation (
  attestation_id uuid PRIMARY KEY,namespace_uuid uuid NOT NULL REFERENCES mst2_metadata_family_identity(namespace_uuid),
  origin_prepare_id text NOT NULL REFERENCES mst2_metadata_prepare(prepare_id),
  tagged_tree_oid text NOT NULL CHECK(tagged_tree_oid ~ '^(sha1:[0-9a-f]{40}|sha256:[0-9a-f]{64}|blake3:[0-9a-f]{64})$'),
  source_profile jsonb NOT NULL,profile_digest bytea NOT NULL CHECK(octet_length(profile_digest)=32),
  source_body_digest bytea NOT NULL CHECK(octet_length(source_body_digest)=32),
  source_revision uuid NOT NULL,
  root_page bytea NOT NULL,root_generation bigint NOT NULL,
  root_certificate_digest bytea NOT NULL CHECK(octet_length(root_certificate_digest)=32),
  source_proof jsonb NOT NULL,attestation_digest bytea NOT NULL CHECK(octet_length(attestation_digest)=32),
  FOREIGN KEY(root_page,root_generation,root_certificate_digest)
    REFERENCES mst2_metadata_page_certificate(page_id,generation,certificate_digest),
  UNIQUE(attestation_id,root_page,root_generation,attestation_digest)
);
CREATE INDEX mst2_metadata_source_root_lookup ON mst2_metadata_source_root_attestation(profile_digest,tagged_tree_oid,root_page,root_generation);
CREATE TABLE mst2_metadata_prepare_reuse_root (
  prepare_id text NOT NULL REFERENCES mst2_metadata_prepare(prepare_id),root_page bytea NOT NULL,
  root_generation bigint NOT NULL,attestation_id uuid NOT NULL,attestation_digest bytea NOT NULL,
  PRIMARY KEY(prepare_id,root_page),
  FOREIGN KEY(attestation_id,root_page,root_generation,attestation_digest)
    REFERENCES mst2_metadata_source_root_attestation(attestation_id,root_page,root_generation,attestation_digest)
);
CREATE INDEX mst2_metadata_prepare_reuse_root_page ON mst2_metadata_prepare_reuse_root(root_page,root_generation,prepare_id);
CREATE TABLE mst2_metadata_reuse_index (
  profile_digest bytea NOT NULL,tagged_tree_oid text NOT NULL,attestation_id uuid NOT NULL,
  root_page bytea NOT NULL,root_generation bigint NOT NULL,attestation_digest bytea NOT NULL,
  PRIMARY KEY(profile_digest,tagged_tree_oid),
  FOREIGN KEY(attestation_id,root_page,root_generation,attestation_digest)
    REFERENCES mst2_metadata_source_root_attestation(attestation_id,root_page,root_generation,attestation_digest)
);
CREATE INDEX mst2_metadata_reuse_index_page ON mst2_metadata_reuse_index(root_page,root_generation);

ALTER TABLE mst2_qualified_session_incarnation ADD COLUMN canonical_descriptor bytea NOT NULL,
  ADD COLUMN attestation_id uuid NOT NULL,ADD COLUMN attestation_digest bytea NOT NULL,
  ADD COLUMN state text NOT NULL CHECK(state IN ('READY','RETIRED')),
  ADD FOREIGN KEY(attestation_id,metadata_root,root_generation,attestation_digest)
    REFERENCES mst2_metadata_source_root_attestation(attestation_id,root_page,root_generation,attestation_digest);
ALTER TABLE mst2_qualified_lease_binding ADD COLUMN namespace_uuid uuid NOT NULL,
  ADD COLUMN authorization_epoch bigint NOT NULL,ADD COLUMN publication_sequence bigint NOT NULL,
  ADD COLUMN writer_epoch bigint NOT NULL,ADD COLUMN certificate_receipt_id bigint NOT NULL,
  ADD COLUMN expires_at_unix bigint NOT NULL,ADD COLUMN state text NOT NULL CHECK(state IN ('ACTIVE','RELEASED','EXPIRED')),
  ADD COLUMN lease_epoch bigint NOT NULL CHECK(lease_epoch>0);
CREATE INDEX mst2_qualified_lease_active_incarnation ON mst2_qualified_lease_binding(snapshot_id,session_incarnation,state,lease_id);
CREATE TABLE mst2_metadata_reader_operation (
  operation_id uuid PRIMARY KEY,lease_id text NOT NULL REFERENCES mst2_qualified_lease_binding(lease_id),
  snapshot_id text NOT NULL,session_incarnation uuid NOT NULL,root_page bytea NOT NULL,root_generation bigint NOT NULL,
  lease_epoch bigint NOT NULL CHECK(lease_epoch>0),hard_deadline_unix bigint NOT NULL,
  state text NOT NULL CHECK(state IN ('ACTIVE','FINISHED','EXPIRED')),
  FOREIGN KEY(snapshot_id,session_incarnation) REFERENCES mst2_qualified_session_incarnation(snapshot_id,session_incarnation)
);
CREATE INDEX mst2_metadata_reader_active_lease ON mst2_metadata_reader_operation(lease_id,state,operation_id);
CREATE INDEX mst2_metadata_reader_active_deadline ON mst2_metadata_reader_operation(hard_deadline_unix,operation_id) WHERE state='ACTIVE';
CREATE TABLE mst2_metadata_root_anchor (
  anchor_id uuid PRIMARY KEY,anchor_kind text NOT NULL CHECK(anchor_kind IN ('PREPARE','REUSE','SESSION','LEASE','REQUEST','READER')),
  owner_key text NOT NULL CHECK(octet_length(owner_key) BETWEEN 1 AND 512),
  root_page bytea NOT NULL,root_generation bigint NOT NULL,root_certificate_digest bytea NOT NULL,
  prepare_id text REFERENCES mst2_metadata_prepare(prepare_id),
  snapshot_id text,session_incarnation uuid,lease_id text REFERENCES mst2_qualified_lease_binding(lease_id),
  reader_operation_id uuid REFERENCES mst2_metadata_reader_operation(operation_id),
  UNIQUE(anchor_kind,owner_key,root_page,root_generation),
  FOREIGN KEY(root_page,root_generation) REFERENCES mst2_metadata_graph_node(page_id,generation),
  FOREIGN KEY(root_page,root_generation,root_certificate_digest)
    REFERENCES mst2_metadata_page_certificate(page_id,generation,certificate_digest),
  FOREIGN KEY(snapshot_id,session_incarnation) REFERENCES mst2_qualified_session_incarnation(snapshot_id,session_incarnation)
);
CREATE INDEX mst2_metadata_root_anchor_page ON mst2_metadata_root_anchor(root_page,root_generation,anchor_kind,owner_key);
CREATE INDEX mst2_metadata_root_anchor_prepare ON mst2_metadata_root_anchor(prepare_id,anchor_kind,anchor_id);
CREATE INDEX mst2_metadata_root_anchor_lease ON mst2_metadata_root_anchor(lease_id,anchor_kind,anchor_id);

CREATE FUNCTION mst2_metadata_session_covers_prepare(pid text) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM mst2_metadata_prepare q
    JOIN mst2_metadata_current cur ON cur.page_id=q.metadata_root
    JOIN mst2_metadata_page_certificate certificate USING(page_id,generation)
    JOIN mst2_qualified_session_incarnation session ON session.metadata_root=q.metadata_root
      AND session.root_generation=cur.generation
    JOIN mst2_metadata_source_root_attestation source ON source.attestation_id=session.attestation_id
      AND source.root_page=session.metadata_root AND source.root_generation=session.root_generation
      AND source.attestation_digest=session.attestation_digest
      AND source.root_certificate_digest=certificate.certificate_digest
    JOIN mst2_metadata_root_anchor anchor ON anchor.snapshot_id=session.snapshot_id
      AND anchor.session_incarnation=session.session_incarnation AND anchor.anchor_kind='SESSION'
      AND anchor.owner_key=session.snapshot_id||':'||session.session_incarnation::text
      AND anchor.root_page=session.metadata_root AND anchor.root_generation=session.root_generation
      AND anchor.root_certificate_digest=certificate.certificate_digest
    WHERE q.prepare_id=pid AND q.plan_kind='ROOTED' AND q.state='COMMITTED' AND session.state='READY'
      AND session.namespace_uuid=(SELECT namespace_uuid FROM mst2_metadata_family_identity WHERE singleton=1)
      AND convert_from(session.source_profile,'UTF8')::jsonb=$CORE_SCHEMA$.mst2_route_profile(
        q.source_domain,q.tagged_root_tree_oid,q.scope,q.schema_version,q.metadata_codec,
        q.materialization_policy,q.fs_semantics,q.access_projection,q.verification_revision,q.projection_revision))
$$;

CREATE FUNCTION mst2_metadata_native_profile(pid text) RETURNS jsonb LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE q record;
BEGIN
  SELECT source_domain,tagged_root_tree_oid,schema_version,metadata_codec,materialization_policy,
    fs_semantics,access_projection,verification_revision,projection_revision
    INTO q FROM mst2_metadata_prepare WHERE prepare_id=pid AND state IN ('PREPARING','COMMITTED')
    AND graph_domain='qualified-v1' AND mst2_metadata_scope_matches(primary_scope);
  IF NOT FOUND OR q.source_domain<>'native-git' OR q.schema_version<>2 OR q.metadata_codec<>1
    OR q.materialization_policy<>1 OR q.fs_semantics<>1 OR q.access_projection<>0
    OR q.verification_revision<>2 OR q.projection_revision<>1
    OR q.tagged_root_tree_oid !~ '^(sha1:[0-9a-f]{40}|sha256:[0-9a-f]{64}|blake3:[0-9a-f]{64})$' THEN
    RAISE EXCEPTION 'source attestation requires the exact current native metadata profile';
  END IF;
  RETURN jsonb_build_object('source_domain',q.source_domain,'hash_kind',split_part(q.tagged_root_tree_oid,':',1),
    'schema_version',q.schema_version,'metadata_codec',q.metadata_codec,'materialization_policy',q.materialization_policy,
    'fs_semantics',q.fs_semantics,'access_projection',q.access_projection,
    'verification_revision',q.verification_revision,'projection_revision',q.projection_revision);
END $$;

CREATE FUNCTION mst2_metadata_decode_git_tree(b bytea,hash_kind text) RETURNS jsonb
LANGUAGE plpgsql IMMUTABLE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE width integer; p integer:=0; start integer; mode text; name bytea; oid bytea; kind integer;
  result jsonb[]:=ARRAY[]::jsonb[]; sorted jsonb; count integer:=0;
BEGIN
  width:=CASE hash_kind WHEN 'sha1' THEN 20 WHEN 'sha256' THEN 32 WHEN 'blake3' THEN 32 ELSE 0 END;
  IF width=0 OR octet_length(b)>67108864 THEN RAISE EXCEPTION 'source Git tree kind or byte budget is invalid'; END IF;
  WHILE p<octet_length(b) LOOP
    start:=p;
    WHILE p<octet_length(b) AND get_byte(b,p)<>32 AND p-start<6 LOOP p:=p+1; END LOOP;
    IF p>=octet_length(b) OR get_byte(b,p)<>32 THEN RAISE EXCEPTION 'source Git tree mode is malformed'; END IF;
    mode:=convert_from(substring(b FROM start+1 FOR p-start),'UTF8'); p:=p+1; start:=p;
    WHILE p<octet_length(b) AND get_byte(b,p)<>0 AND p-start<=255 LOOP p:=p+1; END LOOP;
    IF p>=octet_length(b) OR get_byte(b,p)<>0 THEN RAISE EXCEPTION 'source Git tree name is malformed'; END IF;
    name:=substring(b FROM start+1 FOR p-start); PERFORM mst2_metadata_valid_name(name); p:=p+1;
    IF p>octet_length(b)-width THEN RAISE EXCEPTION 'source Git tree object identity is truncated'; END IF;
    oid:=substring(b FROM p+1 FOR width); p:=p+width;
    kind:=CASE mode WHEN '40000' THEN 4 WHEN '100644' THEN 1 WHEN '100664' THEN 1 WHEN '100640' THEN 1
      WHEN '100755' THEN 2 WHEN '120000' THEN 3 ELSE 0 END;
    IF kind=0 THEN RAISE EXCEPTION 'source Git tree contains an unsupported entry'; END IF;
    result:=array_append(result,jsonb_build_object('kind',kind,'name',encode(name,'hex'),
      'oid',hash_kind||':'||encode(oid,'hex'))); count:=count+1;
    IF count>131072 THEN RAISE EXCEPTION 'source Git tree entry budget exceeded'; END IF;
  END LOOP;
  IF EXISTS(SELECT 1 FROM unnest(result) AS rows(value) GROUP BY value->>'name' HAVING count(*)>1) THEN
    RAISE EXCEPTION 'source Git tree has duplicate names';
  END IF;
  SELECT coalesce(jsonb_agg(value ORDER BY decode(value->>'name','hex')),'[]'::jsonb) INTO sorted
    FROM unnest(result) AS rows(value);
  RETURN sorted;
END $$;

CREATE FUNCTION mst2_metadata_compute_source_proof(pid text,tree_oid text,p bytea,g bigint) RETURNS jsonb
LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE profile jsonb:=mst2_metadata_native_profile(pid); profile_hash bytea; body bytea; decoded jsonb;
  item jsonb; mapped jsonb; entry_rows jsonb[]:=ARRAY[]::jsonb[]; entries jsonb; child_root bytea;
  child_binding record; source_entries jsonb; reference_rows jsonb[]:=ARRAY[]::jsonb[];
  reference jsonb; fact record; built jsonb; proof jsonb; source_revision uuid; body_digest bytea;
BEGIN
  IF split_part(tree_oid,':',1) IS DISTINCT FROM profile->>'hash_kind'
    OR tree_oid !~ '^(sha1:[0-9a-f]{40}|sha256:[0-9a-f]{64}|blake3:[0-9a-f]{64})$' THEN
    RAISE EXCEPTION 'source tree attestation crossed its tagged hash kind';
  END IF;
  profile_hash:=sha256(convert_to('mega.mst2.native-profile.v1','UTF8')||decode('00','hex')||convert_to(profile::text,'UTF8'));
  SELECT sub_trees INTO body FROM $CORE_SCHEMA$.mega_tree WHERE tree_id=split_part(tree_oid,':',2);
  IF NOT FOUND THEN RAISE EXCEPTION 'source Git tree is missing from its captured core'; END IF;
  body_digest:=sha256(body);
  source_revision:=$CORE_SCHEMA$.mst2_route_capture_source_tree(split_part(tree_oid,':',2),body_digest);
  decoded:=mst2_metadata_decode_git_tree(body,profile->>'hash_kind');
  FOR item IN SELECT value FROM jsonb_array_elements(decoded) LOOP
    mapped:=jsonb_build_object('kind',(item->>'kind')::integer,'name',item->>'name');
    reference:=jsonb_build_object('kind',(item->>'kind')::integer,'git_oid',item->>'oid');
    IF (item->>'kind')::integer=4 THEN
      SELECT a.root_page,a.root_generation,a.root_certificate_digest INTO child_binding FROM mst2_metadata_source_root_attestation a
        JOIN mst2_metadata_current cur ON cur.page_id=a.root_page AND cur.generation=a.root_generation
        JOIN mst2_metadata_lifetime life USING(page_id,generation)
        JOIN mst2_metadata_graph_node node USING(page_id,generation)
        JOIN mst2_metadata_prepare origin ON origin.prepare_id=a.origin_prepare_id
        JOIN $CORE_SCHEMA$.mega_tree child_tree ON child_tree.tree_id=split_part(a.tagged_tree_oid,':',2)
        WHERE a.tagged_tree_oid=item->>'oid' AND a.profile_digest=profile_hash AND a.source_profile=profile
          AND a.namespace_uuid=(SELECT namespace_uuid FROM mst2_metadata_family_identity WHERE singleton=1)
          AND node.state='LIVE' AND node.certificate_digest=a.root_certificate_digest
          AND $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest)
          AND (life.state='LIVE' AND origin.state='COMMITTED' OR life.state='RESERVED' AND origin.prepare_id=pid AND origin.state='PREPARING')
          AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=a.root_page AND gc.generation=a.root_generation)
        ORDER BY a.attestation_id LIMIT 1;
      IF NOT FOUND THEN RAISE EXCEPTION 'source directory lacks its exact-profile certified child root'; END IF;
      child_root:=child_binding.root_page;
      mapped:=mapped||jsonb_build_object('child',encode(child_root,'hex'));
      reference:=reference||jsonb_build_object('child_root',encode(child_root,'hex'),
        'child_generation',child_binding.root_generation,'child_certificate',encode(child_binding.root_certificate_digest,'hex'));
    ELSE
      SELECT size,raw_sha256 INTO fact FROM $CORE_SCHEMA$.mst2_verified_object
        WHERE storage_domain='git' AND git_oid=split_part(item->>'oid',':',2) AND object_kind='blob'
          AND state='VERIFIED' AND verification_version=2 FOR SHARE NOWAIT;
      IF NOT FOUND OR fact.size<0 OR fact.size>8796093022208 OR octet_length(fact.raw_sha256)<>32
        OR (item->>'kind')::integer=3 AND fact.size NOT BETWEEN 1 AND 4095 THEN
        RAISE EXCEPTION 'source file lacks its valid current verified-object fact';
      END IF;
      mapped:=mapped||jsonb_build_object('size',fact.size,'content_id',encode(fact.raw_sha256,'hex'));
      reference:=reference||jsonb_build_object('size',fact.size,'content_digest',encode(fact.raw_sha256,'hex'));
    END IF;
    reference_rows:=array_append(reference_rows,reference||jsonb_build_object('name',item->>'name'));
    entry_rows:=array_append(entry_rows,mapped);
  END LOOP;
  entries:=to_jsonb(entry_rows);
  SELECT coalesce(jsonb_object_agg(value->>'name',value-'name'),'{}'::jsonb) INTO source_entries
    FROM unnest(reference_rows) input(value);
  built:=mst2_metadata_build_map(entries);
  IF decode(built->>'page_id','hex')<>p OR NOT EXISTS(SELECT 1 FROM mst2_metadata_page_certificate c
    JOIN mst2_metadata_current cur USING(page_id,generation) JOIN mst2_metadata_graph_node n USING(page_id,generation)
    WHERE c.page_id=p AND c.generation=g AND n.state='LIVE' AND n.certificate_digest=c.certificate_digest) THEN
    RAISE EXCEPTION 'source projection differs from its independently canonical certified root';
  END IF;
  proof:=jsonb_build_object('namespace',(SELECT namespace_uuid::text FROM mst2_metadata_family_identity WHERE singleton=1),
    'source_profile',profile,'profile_digest',encode(profile_hash,'hex'),'tagged_tree_oid',tree_oid,
    'source_body_digest',encode(body_digest,'hex'),'source_revision',source_revision::text,'root_page',encode(p,'hex'),'root_generation',g,
    'root_certificate',(SELECT encode(certificate_digest,'hex') FROM mst2_metadata_page_certificate WHERE page_id=p AND generation=g),
    'source_entry_count',jsonb_array_length(entries),'source_entries',source_entries,'source_work_units',built->'source_work_units',
    'encoded_map_digest',encode(sha256(convert_to(entries::text,'UTF8')),'hex'));
  RETURN proof||jsonb_build_object('attestation',encode(sha256(convert_to('mega.mst2.source-root.v1','UTF8')
    ||decode('00','hex')||convert_to(proof::text,'UTF8')),'hex'));
END $$;

CREATE FUNCTION mst2_metadata_source_attestation_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE proof jsonb;
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'source root attestation history is immutable'; END IF;
  proof:=mst2_metadata_compute_source_proof(NEW.origin_prepare_id,NEW.tagged_tree_oid,NEW.root_page,NEW.root_generation);
  IF NEW.source_revision IS NULL THEN NEW.source_revision:=(proof->>'source_revision')::uuid; END IF;
  IF NEW.namespace_uuid::text IS DISTINCT FROM proof->>'namespace' OR NEW.source_profile IS DISTINCT FROM proof->'source_profile'
    OR NEW.profile_digest IS DISTINCT FROM decode(proof->>'profile_digest','hex')
    OR NEW.source_body_digest IS DISTINCT FROM decode(proof->>'source_body_digest','hex')
    OR NEW.source_revision IS DISTINCT FROM (proof->>'source_revision')::uuid
    OR NEW.root_certificate_digest IS DISTINCT FROM decode(proof->>'root_certificate','hex')
    OR NEW.attestation_digest IS DISTINCT FROM decode(proof->>'attestation','hex') OR NEW.source_proof IS DISTINCT FROM proof THEN
    RAISE EXCEPTION 'source attestation was not independently derived from the captured core and canonical root';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_source_attestation_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_source_root_attestation
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_source_attestation_guard();

CREATE FUNCTION mst2_metadata_source_attestation_committed() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare WHERE prepare_id=NEW.origin_prepare_id AND state='COMMITTED') THEN
    RAISE EXCEPTION 'source attestation cannot commit without its definitive finalized preparation';
  END IF;
  IF NOT $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(NEW.tagged_tree_oid,':',2),NEW.source_revision,NEW.source_body_digest)
    OR NEW.source_profile IS DISTINCT FROM mst2_metadata_native_profile(NEW.origin_prepare_id) THEN
    RAISE EXCEPTION 'source attestation cannot commit after its exact captured source body or profile changed';
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_source_attestation_committed AFTER INSERT ON mst2_metadata_source_root_attestation
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_source_attestation_committed();

CREATE FUNCTION mst2_metadata_reuse_root_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE profile jsonb; profile_hash bytea;
BEGIN
  IF TG_OP<>'INSERT' THEN RAISE EXCEPTION 'rooted reuse membership history is immutable'; END IF;
  profile:=mst2_metadata_native_profile(NEW.prepare_id);
  profile_hash:=sha256(convert_to('mega.mst2.native-profile.v1','UTF8')||decode('00','hex')||convert_to(profile::text,'UTF8'));
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare q,mst2_metadata_source_root_attestation a
      JOIN mst2_metadata_current cur ON cur.page_id=a.root_page AND cur.generation=a.root_generation
      JOIN mst2_metadata_lifetime life USING(page_id,generation)
      JOIN mst2_metadata_graph_node node USING(page_id,generation)
      JOIN mst2_metadata_prepare origin ON origin.prepare_id=a.origin_prepare_id
      JOIN $CORE_SCHEMA$.mega_tree tree ON tree.tree_id=split_part(a.tagged_tree_oid,':',2)
      WHERE q.prepare_id=NEW.prepare_id AND q.state='PREPARING' AND a.attestation_id=NEW.attestation_id
        AND a.root_page=NEW.root_page AND a.root_generation=NEW.root_generation AND a.attestation_digest=NEW.attestation_digest
        AND a.profile_digest=profile_hash AND a.source_profile=profile
        AND $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest)
        AND origin.state='COMMITTED' AND life.state='LIVE' AND life.graph_domain='qualified-v1'
        AND node.state='LIVE' AND node.certificate_digest=a.root_certificate_digest
        AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=a.root_page AND gc.generation=a.root_generation)) THEN
    RAISE EXCEPTION 'rooted reuse membership lacks its exact finalized source and current lifetime';
  END IF;
  IF (SELECT count(*) FROM mst2_metadata_prepare_reuse_root WHERE prepare_id=NEW.prepare_id)>=4096 THEN
    RAISE EXCEPTION 'rooted reuse boundary budget exceeded';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_reuse_root_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_prepare_reuse_root
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_reuse_root_guard();

CREATE FUNCTION mst2_metadata_reuse_index_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'rooted reuse index cannot retarget an exact lifetime'; END IF;
  IF TG_OP='DELETE' THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op WHERE page_id=OLD.root_page AND generation=OLD.root_generation AND state='PENDING') THEN
      RAISE EXCEPTION 'rooted reuse index retirement requires its exact GC claim';
    END IF;
    RETURN OLD;
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_source_root_attestation a
    JOIN mst2_metadata_prepare q ON q.prepare_id=a.origin_prepare_id
    JOIN mst2_metadata_current cur ON cur.page_id=a.root_page AND cur.generation=a.root_generation
    JOIN mst2_metadata_lifetime life USING(page_id,generation)
    JOIN mst2_metadata_graph_node node USING(page_id,generation)
    WHERE a.attestation_id=NEW.attestation_id AND a.profile_digest=NEW.profile_digest AND a.tagged_tree_oid=NEW.tagged_tree_oid
      AND a.root_page=NEW.root_page AND a.root_generation=NEW.root_generation AND a.attestation_digest=NEW.attestation_digest
      AND q.state='COMMITTED' AND life.state='LIVE' AND node.state='LIVE' AND node.certificate_digest=a.root_certificate_digest
      AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=a.root_page AND gc.generation=a.root_generation)) THEN
    RAISE EXCEPTION 'rooted reuse index is not bound to its definitive exact current source proof';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_reuse_index_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_reuse_index
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_reuse_index_guard();

CREATE FUNCTION mst2_metadata_root_anchor_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP='UPDATE' THEN RAISE EXCEPTION 'rooted anchors cannot retarget immutable owner or root identities'; END IF;
  IF TG_OP='DELETE' THEN
    IF OLD.anchor_kind IN ('PREPARE','REUSE') THEN
      IF NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare WHERE prepare_id=OLD.prepare_id AND state IN ('PREPARING','COMMITTED','ABORTED')) THEN
        RAISE EXCEPTION 'temporary anchor owner history is missing';
      END IF;
    ELSIF OLD.anchor_kind='SESSION' THEN
      IF NOT EXISTS(SELECT 1 FROM mst2_qualified_session_incarnation s
          WHERE s.snapshot_id=OLD.snapshot_id AND s.session_incarnation=OLD.session_incarnation AND s.state='RETIRED')
        OR EXISTS(SELECT 1 FROM mst2_qualified_lease_binding l WHERE l.snapshot_id=OLD.snapshot_id
          AND l.session_incarnation=OLD.session_incarnation AND l.state='ACTIVE') THEN
        RAISE EXCEPTION 'session root still has its active incarnation or leases';
      END IF;
    ELSIF OLD.anchor_kind='LEASE' THEN
      IF NOT EXISTS(SELECT 1 FROM mst2_qualified_lease_binding WHERE lease_id=OLD.lease_id AND state IN ('RELEASED','EXPIRED'))
        OR EXISTS(SELECT 1 FROM mst2_metadata_reader_operation WHERE lease_id=OLD.lease_id AND state='ACTIVE') THEN
        RAISE EXCEPTION 'lease root still has its active lease or readers';
      END IF;
    ELSE
      IF NOT EXISTS(SELECT 1 FROM mst2_metadata_reader_operation r WHERE r.operation_id=OLD.reader_operation_id
        AND (r.state='FINISHED' OR r.state='EXPIRED' AND r.hard_deadline_unix<=floor(extract(epoch FROM clock_timestamp()))::bigint)) THEN
        RAISE EXCEPTION 'reader root still has an active operation';
      END IF;
    END IF;
    RETURN OLD;
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_node node
      JOIN mst2_metadata_current cur USING(page_id,generation)
      JOIN mst2_metadata_lifetime life USING(page_id,generation)
      JOIN mst2_metadata_page_certificate proof USING(page_id,generation)
      WHERE node.page_id=NEW.root_page AND node.generation=NEW.root_generation AND node.state='LIVE'
        AND node.certificate_digest=NEW.root_certificate_digest AND proof.certificate_digest=NEW.root_certificate_digest
        AND life.state IN ('RESERVED','LIVE') AND life.graph_domain='qualified-v1'
        AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=node.page_id AND gc.generation=node.generation)) THEN
    RAISE EXCEPTION 'rooted anchor does not protect its exact canonical current graph';
  END IF;
  IF NEW.anchor_kind IN ('PREPARE','REUSE') THEN
    IF NEW.owner_key IS DISTINCT FROM NEW.prepare_id OR NEW.snapshot_id IS NOT NULL OR NEW.session_incarnation IS NOT NULL
      OR NEW.lease_id IS NOT NULL OR NEW.reader_operation_id IS NOT NULL
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare WHERE prepare_id=NEW.prepare_id
        AND state IN ('PREPARING','COMMITTED') AND coverage_retired_at IS NULL)
      OR NEW.anchor_kind='PREPARE' AND NOT (EXISTS(SELECT 1 FROM mst2_metadata_prepare_page
          WHERE prepare_id=NEW.prepare_id AND page_id=NEW.root_page AND generation=NEW.root_generation)
        OR EXISTS(SELECT 1 FROM mst2_metadata_prepare q JOIN mst2_metadata_prepare_reuse_root r USING(prepare_id)
          WHERE q.prepare_id=NEW.prepare_id AND q.plan_kind='ROOTED' AND q.metadata_root=NEW.root_page
            AND r.root_page=NEW.root_page AND r.root_generation=NEW.root_generation))
      OR NEW.anchor_kind='REUSE' AND NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_reuse_root
        WHERE prepare_id=NEW.prepare_id AND root_page=NEW.root_page AND root_generation=NEW.root_generation) THEN
      RAISE EXCEPTION 'temporary anchor differs from its immutable delta or reused-root owner';
    END IF;
  ELSIF NEW.anchor_kind='SESSION' THEN
    IF NEW.owner_key IS DISTINCT FROM NEW.snapshot_id||':'||NEW.session_incarnation::text OR NEW.prepare_id IS NOT NULL
      OR NEW.lease_id IS NOT NULL OR NEW.reader_operation_id IS NOT NULL
      OR NOT EXISTS(SELECT 1 FROM mst2_qualified_session_incarnation s WHERE s.snapshot_id=NEW.snapshot_id
        AND s.session_incarnation=NEW.session_incarnation AND s.metadata_root=NEW.root_page AND s.root_generation=NEW.root_generation AND s.state='READY') THEN
      RAISE EXCEPTION 'session anchor differs from its exact ready incarnation';
    END IF;
  ELSIF NEW.anchor_kind='LEASE' THEN
    IF NEW.owner_key IS DISTINCT FROM NEW.lease_id OR NEW.prepare_id IS NOT NULL OR NEW.reader_operation_id IS NOT NULL
      OR NOT EXISTS(SELECT 1 FROM mst2_qualified_lease_binding l WHERE l.lease_id=NEW.lease_id
        AND l.snapshot_id=NEW.snapshot_id AND l.session_incarnation=NEW.session_incarnation
        AND l.metadata_root=NEW.root_page AND l.root_generation=NEW.root_generation AND l.state='ACTIVE'
        AND l.expires_at_unix>floor(extract(epoch FROM clock_timestamp()))::bigint) THEN
      RAISE EXCEPTION 'lease anchor differs from its exact active lease';
    END IF;
  ELSE
    IF NEW.owner_key IS DISTINCT FROM NEW.reader_operation_id::text OR NEW.prepare_id IS NOT NULL
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_reader_operation r JOIN mst2_qualified_lease_binding l USING(lease_id)
        WHERE r.operation_id=NEW.reader_operation_id AND r.lease_id=NEW.lease_id AND r.snapshot_id=NEW.snapshot_id
          AND r.session_incarnation=NEW.session_incarnation AND r.root_page=NEW.root_page AND r.root_generation=NEW.root_generation
          AND r.state='ACTIVE' AND l.state='ACTIVE' AND l.lease_epoch=r.lease_epoch
          AND r.hard_deadline_unix<=l.expires_at_unix AND r.hard_deadline_unix>floor(extract(epoch FROM clock_timestamp()))::bigint) THEN
      RAISE EXCEPTION 'reader anchor differs from its exact active lease operation';
    END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_root_anchor_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_root_anchor
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_root_anchor_guard();

CREATE FUNCTION mst2_metadata_reuse_root_protected() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF EXISTS(SELECT 1 FROM mst2_metadata_prepare WHERE prepare_id=NEW.prepare_id
      AND state IN ('PREPARING','COMMITTED') AND coverage_retired_at IS NULL)
    AND NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor WHERE anchor_kind='REUSE' AND prepare_id=NEW.prepare_id
      AND root_page=NEW.root_page AND root_generation=NEW.root_generation) THEN
    RAISE EXCEPTION 'reused boundary must commit with continuously owned root protection';
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_reuse_root_protected AFTER INSERT ON mst2_metadata_prepare_reuse_root
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_reuse_root_protected();

CREATE FUNCTION mst2_metadata_temporary_anchor_continuity() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE q mst2_metadata_prepare%ROWTYPE;
BEGIN
  IF OLD.anchor_kind NOT IN ('PREPARE','REUSE') THEN RETURN NULL; END IF;
  SELECT * INTO STRICT q FROM mst2_metadata_prepare WHERE prepare_id=OLD.prepare_id;
  IF q.state='ABORTED' THEN RETURN NULL; END IF;
  IF q.coverage_retired_at IS NULL THEN
    IF NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.anchor_kind=OLD.anchor_kind
        AND a.owner_key=OLD.owner_key AND a.prepare_id=OLD.prepare_id
        AND a.root_page=OLD.root_page AND a.root_generation=OLD.root_generation
        AND a.root_certificate_digest=OLD.root_certificate_digest) THEN
      RAISE EXCEPTION 'active preparation cannot lose its continuously owned canonical root';
    END IF;
  ELSIF q.state<>'COMMITTED' OR NOT (mst2_metadata_session_covers_prepare(q.prepare_id)
    OR mst2_metadata_orphan_prepare_retired(q.prepare_id)) THEN
    RAISE EXCEPTION 'retired preparation coverage requires a definitive independent session root';
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_temporary_anchor_continuity AFTER DELETE ON mst2_metadata_root_anchor
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_temporary_anchor_continuity();
