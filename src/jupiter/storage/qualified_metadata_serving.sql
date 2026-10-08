-- Root ownership is derived from durable identities, never an application flag.
CREATE FUNCTION mst2_metadata_publication_valid(instance text,commit_id text,tree_id text,
  sequence_id bigint,epoch_id bigint,receipt_id bigint,current_head boolean DEFAULT false)
RETURNS boolean LANGUAGE sql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mst2_native_publication c
    JOIN $CORE_SCHEMA$.mst2_publication p ON p.id=c.receipt_id
    JOIN $CORE_SCHEMA$.mst2_publication_outbox o ON o.operation_id=p.operation_id
    JOIN $CORE_SCHEMA$.mega_commit source ON source.commit_id=c.root_commit AND source.tree=c.root_tree
    JOIN $CORE_SCHEMA$.mega_commit previous ON previous.commit_id=c.old_root_commit AND previous.tree=c.old_root_tree
    JOIN $CORE_SCHEMA$.mega_commit path_source ON path_source.commit_id=c.path_commit AND path_source.tree=c.path_tree
    WHERE c.receipt_id=$6 AND c.namespace='/' AND c.instance_id=$1
      AND c.root_commit=$2 AND c.root_tree=$3 AND c.sequence=$4 AND c.writer_epoch=$5
      AND $4>0 AND $5>0 AND p.writer_epoch=$5 AND p.writer_kind='trunk_push'
      AND p.native_certificate_version=1 AND p.request_digest_version=1
      AND p.request_digest ~ '^sha256:[0-9a-f]{64}$' AND c.origin_ref='refs/heads/main'
      AND c.origin_path=p.namespace AND c.path_commit=p.new_oid AND c.old_root_commit=p.old_oid
      AND c.old_path_commit IS DISTINCT FROM c.path_commit AND o.namespace=p.namespace AND o.sequence=p.sequence
      AND (NOT $7 OR (EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mst2_native_head h
          WHERE h.namespace='/' AND h.state='READY' AND h.instance_id=$1 AND h.root_commit=$2
            AND h.root_tree=$3 AND h.sequence=$4 AND h.writer_epoch=$5
            AND h.certificate_receipt_id=$6)
        AND (SELECT count(*) FROM $CORE_SCHEMA$.mega_refs r
          WHERE r.path='/' AND r.ref_name='refs/heads/main' AND NOT r.is_cl)=1
        AND EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mega_refs r WHERE r.path='/'
          AND r.ref_name='refs/heads/main' AND NOT r.is_cl AND r.ref_commit_hash=$2 AND r.ref_tree_hash=$3))))
$$;

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

CREATE FUNCTION mst2_metadata_root_live(p bytea,g bigint,c bytea) RETURNS boolean
LANGUAGE sql VOLATILE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM mst2_metadata_current cur JOIN mst2_metadata_lifetime life USING(page_id,generation)
    JOIN mst2_metadata_graph_node node USING(page_id,generation)
    JOIN mst2_metadata_page_certificate proof USING(page_id,generation)
    JOIN mst2_metadata_payload body USING(page_id,generation)
    WHERE cur.page_id=p AND cur.generation=g AND life.state='LIVE' AND life.graph_domain='qualified-v1'
      AND life.metadata_codec=1 AND node.state='LIVE' AND node.metadata_codec=1 AND body.metadata_codec=1
      AND node.certificate_digest=c AND proof.certificate_digest=c AND proof.proof_revision=1
      AND proof.namespace_uuid='$NAMESPACE_UUID$'::uuid AND life.expected_size=proof.byte_size
      AND node.bytes=proof.byte_size AND body.byte_size=proof.byte_size AND octet_length(body.payload)=proof.byte_size
      AND p=sha256(convert_to('mega.mst2.metapage','UTF8')||decode('00','hex')||body.payload)
      AND NOT EXISTS(SELECT 1 FROM mst2_metadata_gc_op gc WHERE gc.page_id=p AND gc.generation=g))
$$;

CREATE FUNCTION mst2_metadata_serving_source(pid text,a_id uuid,p bytea,g bigint,c bytea)
RETURNS boolean LANGUAGE plpgsql VOLATILE STRICT SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE a record; scope text; certificate record;
BEGIN
  SELECT proof.tagged_tree_oid,proof.source_revision,proof.source_body_digest,proof.profile_digest,proof.source_profile
    INTO a FROM mst2_metadata_source_root_attestation proof
    JOIN mst2_metadata_prepare origin ON origin.prepare_id=proof.origin_prepare_id
    WHERE proof.attestation_id=a_id AND proof.namespace_uuid='$NAMESPACE_UUID$'::uuid
      AND proof.root_page=p AND proof.root_generation=g AND proof.root_certificate_digest=c
      AND origin.state='COMMITTED' AND proof.source_profile=mst2_metadata_native_profile(pid)
      AND proof.tagged_tree_oid=mst2_metadata_rooted_scope_tree(pid);
  IF NOT FOUND THEN RETURN false; END IF;
  SELECT relative_path_bytes,relative_components INTO certificate FROM mst2_metadata_page_certificate WHERE page_id=p AND generation=g AND certificate_digest=c;
  IF NOT FOUND THEN RETURN false; END IF;
  SELECT q.scope INTO STRICT scope FROM mst2_metadata_prepare q WHERE q.prepare_id=pid;
  IF (CASE WHEN scope='/' THEN 0 ELSE octet_length(scope) END)::bigint+certificate.relative_path_bytes>4096
    OR (CASE WHEN scope='/' THEN 0 ELSE cardinality(string_to_array(substring(scope FROM 2),'/')) END)::bigint
      +certificate.relative_components>256 THEN RETURN false; END IF;
  RETURN $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest)
    AND a.profile_digest=sha256(convert_to('mega.mst2.native-profile.v1','UTF8')||decode('00','hex')
      ||convert_to(a.source_profile::text,'UTF8'));
END $$;

CREATE FUNCTION mst2_metadata_snapshot_candidate(sid text,descriptor bytea,instance text,commit_id text,
  tree_id text,root bytea,profile jsonb) RETURNS boolean LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE p record; a record;
BEGIN
  IF sid IS DISTINCT FROM 'sha256:'||encode(sha256(convert_to('mega.mst2.descriptor','UTF8')
      ||decode('00','hex')||descriptor),'hex') THEN RETURN false; END IF;
  FOR p IN SELECT q.prepare_id FROM mst2_metadata_prepare q WHERE q.metadata_root=root AND q.state='COMMITTED'
      AND q.plan_kind='ROOTED' AND q.graph_domain='qualified-v1' AND q.coverage_retired_at IS NULL
      AND q.tagged_root_tree_oid=profile->>'tagged_root_tree_oid' AND q.scope=profile->>'scope'
      AND $CORE_SCHEMA$.mst2_route_profile(q.source_domain,q.tagged_root_tree_oid,q.scope,q.schema_version,
        q.metadata_codec,q.materialization_policy,q.fs_semantics,q.access_projection,
        q.verification_revision,q.projection_revision)=profile ORDER BY q.prepare_id LIMIT 1 LOOP
    IF descriptor IS DISTINCT FROM mst2_metadata_descriptor(p.prepare_id,instance,commit_id,tree_id) THEN CONTINUE; END IF;
    FOR a IN SELECT proof.attestation_id,proof.root_generation,proof.root_certificate_digest FROM mst2_metadata_source_root_attestation proof
        JOIN mst2_metadata_current current_root ON current_root.page_id=proof.root_page AND current_root.generation=proof.root_generation
        WHERE proof.root_page=root AND proof.tagged_tree_oid=mst2_metadata_rooted_scope_tree(p.prepare_id)
          AND proof.source_profile=mst2_metadata_native_profile(p.prepare_id)
          AND $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(proof.tagged_tree_oid,':',2),proof.source_revision,proof.source_body_digest)
          ORDER BY proof.attestation_id LIMIT 1 LOOP
      IF mst2_metadata_serving_source(p.prepare_id,a.attestation_id,root,a.root_generation,a.root_certificate_digest)
        AND mst2_metadata_root_live(root,a.root_generation,a.root_certificate_digest) THEN RETURN true; END IF;
    END LOOP;
  END LOOP;
  RETURN false;
END $$;

CREATE FUNCTION mst2_metadata_incarnation_proof(sid text,inc uuid,require_live boolean DEFAULT false)
RETURNS boolean LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE s mst2_qualified_session_incarnation%ROWTYPE; p record;
  a record; profile jsonb;
BEGIN
  SELECT * INTO s FROM mst2_qualified_session_incarnation WHERE snapshot_id=sid AND session_incarnation=inc;
  IF NOT FOUND OR s.namespace_uuid<>'$NAMESPACE_UUID$'::uuid OR s.authorization_epoch<>1 THEN RETURN false; END IF;
  SELECT prepare_id,source_domain,tagged_root_tree_oid,scope,schema_version,metadata_codec,
    materialization_policy,fs_semantics,access_projection,verification_revision,projection_revision
    INTO p FROM mst2_metadata_prepare WHERE prepare_id=s.prepare_id AND state='COMMITTED'
    AND plan_kind='ROOTED' AND storage_seal=s.storage_seal AND metadata_root=s.metadata_root;
  IF NOT FOUND THEN RETURN false; END IF;
  profile:=$CORE_SCHEMA$.mst2_route_profile(p.source_domain,p.tagged_root_tree_oid,p.scope,p.schema_version,
    p.metadata_codec,p.materialization_policy,p.fs_semantics,p.access_projection,p.verification_revision,p.projection_revision);
  IF s.source_profile IS DISTINCT FROM convert_to(profile::text,'UTF8')
    OR s.canonical_descriptor IS DISTINCT FROM mst2_metadata_descriptor(p.prepare_id,s.instance_id,s.commit_oid,s.root_tree_oid)
    OR s.snapshot_id IS DISTINCT FROM 'sha256:'||encode(sha256(convert_to('mega.mst2.descriptor','UTF8')
        ||decode('00','hex')||s.canonical_descriptor),'hex')
    OR NOT EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mst2_snapshot_storage_route r
      WHERE r.snapshot_id=s.snapshot_id AND r.namespace_uuid=s.namespace_uuid AND r.canonical_descriptor=s.canonical_descriptor
        AND r.instance_id=s.instance_id AND r.commit_oid=s.commit_oid AND r.root_tree_oid=s.root_tree_oid
        AND r.metadata_root=s.metadata_root AND r.source_profile=profile)
    OR NOT mst2_metadata_publication_valid(s.instance_id,s.commit_oid,s.root_tree_oid,
        s.publication_sequence,s.writer_epoch,s.certificate_receipt_id,false) THEN RETURN false; END IF;
  SELECT attestation_id,root_certificate_digest INTO a FROM mst2_metadata_source_root_attestation WHERE attestation_id=s.attestation_id
    AND attestation_digest=s.attestation_digest AND root_page=s.metadata_root AND root_generation=s.root_generation;
  IF NOT FOUND OR NOT mst2_metadata_serving_source(s.prepare_id,a.attestation_id,s.metadata_root,
      s.root_generation,a.root_certificate_digest) THEN RETURN false; END IF;
  IF require_live AND (s.state<>'READY' OR NOT mst2_metadata_root_live(s.metadata_root,s.root_generation,a.root_certificate_digest)
    OR NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor anchor WHERE anchor.anchor_kind='SESSION'
      AND anchor.owner_key=s.snapshot_id||':'||s.session_incarnation::text AND anchor.snapshot_id=s.snapshot_id
      AND anchor.session_incarnation=s.session_incarnation AND anchor.root_page=s.metadata_root
      AND anchor.root_generation=s.root_generation AND anchor.root_certificate_digest=a.root_certificate_digest)) THEN RETURN false; END IF;
  RETURN true;
END $$;

CREATE FUNCTION mst2_metadata_snapshot_route_proof(sid text,descriptor bytea,instance text,commit_id text,
  tree_id text,root bytea,profile jsonb) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM mst2_qualified_session_incarnation s WHERE s.snapshot_id=sid
    AND s.canonical_descriptor=descriptor AND s.instance_id=instance AND s.commit_oid=commit_id AND s.root_tree_oid=tree_id
    AND s.metadata_root=root AND s.source_profile=convert_to(profile::text,'UTF8')
    AND mst2_metadata_incarnation_proof(s.snapshot_id,s.session_incarnation,false))
$$;

CREATE FUNCTION mst2_metadata_lease_route_proof(lid text,sid text,namespace uuid,inc uuid,pid text,root bytea,
  auth bigint,publication bigint,epoch bigint,receipt bigint) RETURNS boolean LANGUAGE sql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
  SELECT EXISTS(SELECT 1 FROM mst2_qualified_lease_binding l JOIN mst2_qualified_session_incarnation s
    ON s.snapshot_id=l.snapshot_id AND s.session_incarnation=l.session_incarnation
    WHERE l.lease_id=lid AND l.snapshot_id=sid AND l.namespace_uuid=namespace AND namespace='$NAMESPACE_UUID$'::uuid
      AND l.session_incarnation=inc AND l.prepare_id=pid AND l.metadata_root=root
      AND l.authorization_epoch=auth AND l.publication_sequence=publication AND l.writer_epoch=epoch
      AND l.certificate_receipt_id=receipt AND l.storage_seal=s.storage_seal AND l.root_generation=s.root_generation
      AND ROW(l.authorization_epoch,l.publication_sequence,l.writer_epoch,l.certificate_receipt_id)
        IS NOT DISTINCT FROM ROW(s.authorization_epoch,s.publication_sequence,s.writer_epoch,s.certificate_receipt_id)
      AND mst2_metadata_incarnation_proof(s.snapshot_id,s.session_incarnation,false))
$$;

CREATE UNIQUE INDEX mst2_qualified_snapshot_ready ON mst2_qualified_session_incarnation(snapshot_id) WHERE state='READY';
CREATE INDEX mst2_metadata_committed_source ON mst2_metadata_prepare(metadata_root,tagged_root_tree_oid,scope,prepare_id)
  WHERE state='COMMITTED' AND plan_kind='ROOTED';
CREATE INDEX mst2_qualified_lease_expiry ON mst2_qualified_lease_binding(expires_at_unix,lease_id) WHERE state='ACTIVE';
CREATE FUNCTION mst2_metadata_session_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE p record; a record; profile jsonb;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified session incarnation history is immutable'; END IF;
  IF TG_OP='UPDATE' THEN
    IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state') OR OLD.state='RETIRED' AND NEW.state<>'RETIRED'
      OR NEW.state NOT IN ('READY','RETIRED') THEN RAISE EXCEPTION 'qualified fixed incarnation cannot change or revive'; END IF;
    IF NEW.state='RETIRED' AND EXISTS(SELECT 1 FROM mst2_qualified_lease_binding l
      WHERE l.snapshot_id=OLD.snapshot_id AND l.session_incarnation=OLD.session_incarnation AND l.state='ACTIVE') THEN
      RAISE EXCEPTION 'qualified ready session still has active lease identities'; END IF;
    RETURN NEW;
  END IF;
  SELECT prepare_id,source_domain,tagged_root_tree_oid,scope,schema_version,metadata_codec,
    materialization_policy,fs_semantics,access_projection,verification_revision,projection_revision
    INTO p FROM mst2_metadata_prepare WHERE prepare_id=NEW.prepare_id AND state='COMMITTED'
    AND plan_kind='ROOTED' AND storage_seal=NEW.storage_seal AND metadata_root=NEW.metadata_root AND coverage_retired_at IS NULL;
  IF NOT FOUND OR NEW.namespace_uuid<>'$NAMESPACE_UUID$'::uuid OR NEW.state<>'READY' OR NEW.authorization_epoch<>1
    OR substr(NEW.session_incarnation::text,15,1)<>'4' OR substr(NEW.session_incarnation::text,20,1) NOT IN ('8','9','a','b')
    OR EXISTS(SELECT 1 FROM mst2_qualified_session_incarnation WHERE snapshot_id=NEW.snapshot_id AND state='READY') THEN
    RAISE EXCEPTION 'qualified incarnation requires a fresh server identity and definitive rooted preparation'; END IF;
  profile:=$CORE_SCHEMA$.mst2_route_profile(p.source_domain,p.tagged_root_tree_oid,p.scope,p.schema_version,
    p.metadata_codec,p.materialization_policy,p.fs_semantics,p.access_projection,p.verification_revision,p.projection_revision);
  SELECT attestation_id,root_certificate_digest INTO a FROM mst2_metadata_source_root_attestation WHERE attestation_id=NEW.attestation_id
    AND attestation_digest=NEW.attestation_digest AND root_page=NEW.metadata_root AND root_generation=NEW.root_generation;
  IF NOT FOUND OR NEW.source_profile IS DISTINCT FROM convert_to(profile::text,'UTF8')
    OR NEW.canonical_descriptor IS DISTINCT FROM mst2_metadata_descriptor(p.prepare_id,NEW.instance_id,NEW.commit_oid,NEW.root_tree_oid)
    OR NEW.snapshot_id IS DISTINCT FROM 'sha256:'||encode(sha256(convert_to('mega.mst2.descriptor','UTF8')
      ||decode('00','hex')||NEW.canonical_descriptor),'hex')
    OR NOT mst2_metadata_serving_source(p.prepare_id,a.attestation_id,NEW.metadata_root,NEW.root_generation,a.root_certificate_digest)
    OR NOT mst2_metadata_root_live(NEW.metadata_root,NEW.root_generation,a.root_certificate_digest)
    OR NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor anchor WHERE anchor.anchor_kind='PREPARE'
      AND anchor.prepare_id=NEW.prepare_id AND anchor.owner_key=NEW.prepare_id AND anchor.root_page=NEW.metadata_root
      AND anchor.root_generation=NEW.root_generation AND anchor.root_certificate_digest=a.root_certificate_digest)
    OR NOT mst2_metadata_publication_valid(NEW.instance_id,NEW.commit_oid,NEW.root_tree_oid,
      NEW.publication_sequence,NEW.writer_epoch,NEW.certificate_receipt_id,true)
    OR NOT EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mst2_snapshot_storage_route r WHERE r.snapshot_id=NEW.snapshot_id
      AND r.namespace_uuid=NEW.namespace_uuid AND r.canonical_descriptor=NEW.canonical_descriptor
      AND r.instance_id=NEW.instance_id AND r.commit_oid=NEW.commit_oid AND r.root_tree_oid=NEW.root_tree_oid
      AND r.metadata_root=NEW.metadata_root AND r.source_profile=profile) THEN
    RAISE EXCEPTION 'qualified incarnation differs from its independently derived source and permanent route'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_session_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_qualified_session_incarnation
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_session_guard();

CREATE FUNCTION mst2_metadata_lease_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE s mst2_qualified_session_incarnation%ROWTYPE; now_unix bigint:=floor(extract(epoch FROM clock_timestamp()))::bigint;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified lease binding history is immutable'; END IF;
  IF TG_OP='UPDATE' THEN
    IF (to_jsonb(NEW)-ARRAY['state','expires_at_unix','lease_epoch']) IS DISTINCT FROM
      (to_jsonb(OLD)-ARRAY['state','expires_at_unix','lease_epoch']) OR OLD.state<>'ACTIVE' AND NEW IS DISTINCT FROM OLD THEN
      RAISE EXCEPTION 'qualified fixed lease cannot change or revive'; END IF;
    IF NEW.state='ACTIVE' THEN
      IF OLD.expires_at_unix<=now_unix OR NEW.expires_at_unix<=now_unix OR NEW.expires_at_unix>now_unix+3600
        OR NEW.lease_epoch<>OLD.lease_epoch THEN RAISE EXCEPTION 'qualified renewal requires its still-active bounded lease'; END IF;
    ELSIF NEW.state IN ('RELEASED','EXPIRED') THEN
      IF NEW.expires_at_unix<>OLD.expires_at_unix OR NEW.lease_epoch<>OLD.lease_epoch+1
        OR NEW.state='EXPIRED' AND OLD.expires_at_unix>now_unix THEN
        RAISE EXCEPTION 'qualified terminal lease differs from its exact deadline and epoch'; END IF;
    ELSE RAISE EXCEPTION 'qualified lease state is unsupported'; END IF;
    RETURN NEW;
  END IF;
  SELECT * INTO s FROM mst2_qualified_session_incarnation
    WHERE snapshot_id=NEW.snapshot_id AND session_incarnation=NEW.session_incarnation AND state='READY';
  IF NOT FOUND OR NEW.namespace_uuid<>'$NAMESPACE_UUID$'::uuid OR NEW.namespace_uuid<>s.namespace_uuid
    OR NEW.state<>'ACTIVE' OR NEW.lease_epoch<>1 OR NEW.expires_at_unix<=now_unix OR NEW.expires_at_unix>now_unix+3600
    OR NEW.lease_id IS DISTINCT FROM (NEW.lease_id::uuid)::text OR substr(NEW.lease_id,15,1)<>'4'
    OR substr(NEW.lease_id,20,1) NOT IN ('8','9','a','b')
    OR ROW(NEW.prepare_id,NEW.storage_seal,NEW.metadata_root,NEW.root_generation,NEW.authorization_epoch,
      NEW.publication_sequence,NEW.writer_epoch,NEW.certificate_receipt_id) IS DISTINCT FROM
      ROW(s.prepare_id,s.storage_seal,s.metadata_root,s.root_generation,s.authorization_epoch,
        s.publication_sequence,s.writer_epoch,s.certificate_receipt_id)
    OR NOT mst2_metadata_incarnation_proof(s.snapshot_id,s.session_incarnation,true) THEN
    RAISE EXCEPTION 'qualified lease differs from its exact active incarnation and source'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_lease_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_qualified_lease_binding
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_lease_guard();

$READER_LIFECYCLE_SQL$

CREATE FUNCTION mst2_metadata_serving_complete() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE s mst2_qualified_session_incarnation%ROWTYPE; l mst2_qualified_lease_binding%ROWTYPE;
  r mst2_metadata_reader_operation%ROWTYPE;
BEGIN
  IF TG_TABLE_NAME='mst2_qualified_session_incarnation' THEN
    SELECT * INTO STRICT s FROM mst2_qualified_session_incarnation
      WHERE snapshot_id=NEW.snapshot_id AND session_incarnation=NEW.session_incarnation;
    IF NOT mst2_metadata_incarnation_proof(s.snapshot_id,s.session_incarnation,s.state='READY')
      OR s.state='READY' AND NOT EXISTS(SELECT 1 FROM mst2_qualified_lease_binding lease
        WHERE lease.snapshot_id=s.snapshot_id AND lease.session_incarnation=s.session_incarnation AND lease.state='ACTIVE')
      OR s.state='RETIRED' AND EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a
        WHERE a.anchor_kind='SESSION' AND a.snapshot_id=s.snapshot_id AND a.session_incarnation=s.session_incarnation) THEN
      RAISE EXCEPTION 'qualified session cannot commit without its exact final serving roots'; END IF;
  ELSIF TG_TABLE_NAME='mst2_qualified_lease_binding' THEN
    SELECT * INTO STRICT l FROM mst2_qualified_lease_binding WHERE lease_id=NEW.lease_id;
    IF NOT EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mst2_lease_storage_route route
      WHERE route.lease_id=l.lease_id AND route.namespace_uuid=l.namespace_uuid AND route.snapshot_id=l.snapshot_id
        AND route.session_incarnation=l.session_incarnation AND route.prepare_id=l.prepare_id AND route.metadata_root=l.metadata_root
        AND route.authorization_epoch=l.authorization_epoch AND route.publication_sequence=l.publication_sequence
        AND route.writer_epoch=l.writer_epoch AND route.certificate_receipt_id=l.certificate_receipt_id)
      OR l.state='ACTIVE' AND (l.expires_at_unix<=floor(extract(epoch FROM clock_timestamp()))::bigint
        OR NOT mst2_metadata_incarnation_proof(l.snapshot_id,l.session_incarnation,true)
        OR NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.anchor_kind='LEASE' AND a.owner_key=l.lease_id
          AND a.lease_id=l.lease_id AND a.root_page=l.metadata_root AND a.root_generation=l.root_generation))
      OR l.state<>'ACTIVE' AND NOT EXISTS(SELECT 1 FROM mst2_metadata_reader_operation operation
          WHERE operation.lease_id=l.lease_id AND operation.state='ACTIVE')
        AND EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.anchor_kind='LEASE' AND a.lease_id=l.lease_id) THEN
      RAISE EXCEPTION 'qualified lease cannot commit without its exact final route and owned protection'; END IF;
  ELSE
    SELECT * INTO STRICT r FROM mst2_metadata_reader_operation WHERE operation_id=NEW.operation_id AND reader_issuance=NEW.reader_issuance;
    IF r.state='ACTIVE' AND (r.hard_deadline_unix<=floor(extract(epoch FROM clock_timestamp()))::bigint
      OR (SELECT count(*) FROM mst2_metadata_root_anchor a WHERE a.reader_operation_id=r.operation_id AND a.reader_issuance=r.reader_issuance
        AND a.anchor_kind IN ('REQUEST','READER') AND a.owner_key=r.operation_id::text
        AND a.lease_id=r.lease_id AND a.snapshot_id=r.snapshot_id AND a.session_incarnation=r.session_incarnation
        AND a.root_page=r.root_page AND a.root_generation=r.root_generation)<>2)
      OR r.state<>'ACTIVE' AND EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.reader_operation_id=r.operation_id) THEN
      RAISE EXCEPTION 'qualified reader cannot commit without both exact owned roots or definitive cleanup'; END IF;
  END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_session_complete AFTER INSERT OR UPDATE ON mst2_qualified_session_incarnation
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_serving_complete();
CREATE CONSTRAINT TRIGGER mst2_metadata_lease_complete AFTER INSERT OR UPDATE ON mst2_qualified_lease_binding
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_serving_complete();
CREATE CONSTRAINT TRIGGER mst2_metadata_reader_complete AFTER INSERT OR UPDATE ON mst2_metadata_reader_operation
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_serving_complete();

CREATE FUNCTION mst2_metadata_session_row(sid text,lid text,instance text)
RETURNS TABLE(canonical_descriptor bytea,commit_oid text,root_tree_oid text,expires_at_unix bigint,
  authorization_epoch bigint,session_incarnation uuid,root_generation bigint,certificate_digest bytea,attestation_id uuid)
LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_qualified_lease_binding%ROWTYPE; s mst2_qualified_session_incarnation%ROWTYPE;
  c bytea;
BEGIN
  SELECT * INTO l FROM mst2_qualified_lease_binding lease WHERE lease.lease_id=lid AND lease.snapshot_id=sid
    AND lease.state='ACTIVE' AND lease.expires_at_unix>floor(extract(epoch FROM clock_timestamp()))::bigint;
  IF NOT FOUND THEN RETURN; END IF;
  SELECT * INTO s FROM mst2_qualified_session_incarnation session WHERE session.snapshot_id=sid
    AND session.session_incarnation=l.session_incarnation AND session.state='READY' AND session.instance_id=instance;
  IF NOT FOUND THEN RETURN; END IF;
  SELECT proof.root_certificate_digest INTO c FROM mst2_metadata_source_root_attestation proof
    WHERE proof.attestation_id=s.attestation_id AND proof.attestation_digest=s.attestation_digest;
  IF NOT FOUND OR NOT mst2_metadata_incarnation_proof(sid,s.session_incarnation,true)
    OR NOT $CORE_SCHEMA$.mst2_route_qualified_namespace_valid('$NAMESPACE_UUID$'::uuid)
    OR NOT EXISTS(SELECT 1 FROM $CORE_SCHEMA$.mst2_lease_storage_route route
      WHERE route.lease_id=lid AND route.snapshot_id=sid AND route.namespace_uuid=l.namespace_uuid
        AND route.session_incarnation=l.session_incarnation AND route.prepare_id=l.prepare_id AND route.metadata_root=l.metadata_root
        AND route.authorization_epoch=l.authorization_epoch AND route.publication_sequence=l.publication_sequence
        AND route.writer_epoch=l.writer_epoch AND route.certificate_receipt_id=l.certificate_receipt_id)
    OR l.namespace_uuid IS DISTINCT FROM s.namespace_uuid
    OR ROW(l.prepare_id,l.storage_seal,l.metadata_root,l.root_generation,l.authorization_epoch,
      l.publication_sequence,l.writer_epoch,l.certificate_receipt_id) IS DISTINCT FROM
      ROW(s.prepare_id,s.storage_seal,s.metadata_root,s.root_generation,s.authorization_epoch,
        s.publication_sequence,s.writer_epoch,s.certificate_receipt_id)
    OR NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor anchor WHERE anchor.anchor_kind='LEASE' AND anchor.owner_key=lid
      AND anchor.lease_id=lid AND anchor.snapshot_id=sid AND anchor.session_incarnation=l.session_incarnation
      AND anchor.root_page=l.metadata_root AND anchor.root_generation=l.root_generation AND anchor.root_certificate_digest=c) THEN
    RAISE EXCEPTION 'qualified durable session route, source, current lifetime or root ownership changed'; END IF;
  RETURN QUERY SELECT s.canonical_descriptor,s.commit_oid,s.root_tree_oid,l.expires_at_unix,
    l.authorization_epoch,s.session_incarnation,s.root_generation,c,s.attestation_id;
END $$;

CREATE FUNCTION mst2_metadata_handoff(request jsonb) RETURNS TABLE(lease_id text,expires_at_unix bigint)
LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE sid text:=request->>'snapshot_id'; descriptor bytea:=decode(request->>'canonical_descriptor','hex');
  instance text:=request->>'instance_id'; commit_id text:=request->>'commit_oid'; tree_id text:=request->>'root_tree_oid';
  lid text:=request->>'lease_id'; inc uuid:=(request->>'session_incarnation')::uuid; pid text:=request->>'prepare_id';
  p record; s mst2_qualified_session_incarnation%ROWTYPE;
  a record; profile jsonb; now_unix bigint; deadline bigint;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  PERFORM mst2_metadata_cleanup_expired(64);
  now_unix:=floor(extract(epoch FROM clock_timestamp()))::bigint;
  deadline:=now_unix+least(3600,greatest(1,(request->>'lease_seconds')::bigint));
  IF request->>'authorization_epoch' IS DISTINCT FROM '1' OR lid IS NULL OR inc IS NULL OR descriptor IS NULL
    OR NOT mst2_metadata_publication_valid(instance,commit_id,tree_id,(request->>'publication_sequence')::bigint,
      (request->>'writer_epoch')::bigint,(request->>'certificate_receipt_id')::bigint,true) THEN
    RAISE EXCEPTION 'qualified handoff differs from the independently observed current publication'; END IF;
  SELECT * INTO s FROM mst2_qualified_session_incarnation WHERE snapshot_id=sid AND state='READY';
  IF FOUND THEN
    IF s.state<>'READY' OR s.canonical_descriptor IS DISTINCT FROM descriptor OR s.instance_id IS DISTINCT FROM instance
      OR s.commit_oid IS DISTINCT FROM commit_id OR s.root_tree_oid IS DISTINCT FROM tree_id
      OR ROW(s.publication_sequence,s.writer_epoch,s.certificate_receipt_id) IS DISTINCT FROM
        ROW((request->>'publication_sequence')::bigint,(request->>'writer_epoch')::bigint,(request->>'certificate_receipt_id')::bigint)
      OR NOT mst2_metadata_incarnation_proof(sid,s.session_incarnation,true) THEN
      RAISE EXCEPTION 'qualified handoff cannot revive or replace a historical incarnation'; END IF;
  ELSE
    SELECT metadata_root,storage_seal,source_domain,tagged_root_tree_oid,scope,schema_version,metadata_codec,
      materialization_policy,fs_semantics,access_projection,verification_revision,projection_revision
      INTO p FROM mst2_metadata_prepare WHERE prepare_id=pid AND state='COMMITTED' AND plan_kind='ROOTED'
      AND storage_seal=decode(request->>'storage_seal','hex') AND coverage_retired_at IS NULL;
    IF NOT FOUND THEN RAISE EXCEPTION 'qualified handoff has no exact definitive rooted receipt'; END IF;
    SELECT attestation_id,attestation_digest,root_generation,root_certificate_digest
      INTO a FROM mst2_metadata_source_root_attestation WHERE attestation_id=(request->>'attestation_id')::uuid
      AND attestation_digest=decode(request->>'attestation_digest','hex') AND root_page=p.metadata_root
      AND root_generation=(request->>'root_generation')::bigint AND root_certificate_digest=decode(request->>'certificate_digest','hex');
    IF NOT FOUND OR descriptor IS DISTINCT FROM mst2_metadata_descriptor(pid,instance,commit_id,tree_id)
      OR NOT mst2_metadata_serving_source(pid,a.attestation_id,p.metadata_root,a.root_generation,a.root_certificate_digest)
      OR NOT mst2_metadata_root_live(p.metadata_root,a.root_generation,a.root_certificate_digest)
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor anchor WHERE anchor.anchor_kind='PREPARE'
        AND anchor.prepare_id=pid AND anchor.owner_key=pid AND anchor.root_page=p.metadata_root
        AND anchor.root_generation=a.root_generation AND anchor.root_certificate_digest=a.root_certificate_digest) THEN
      RAISE EXCEPTION 'qualified handoff lacks its continuously owned exact canonical source root'; END IF;
    profile:=$CORE_SCHEMA$.mst2_route_profile(p.source_domain,p.tagged_root_tree_oid,p.scope,p.schema_version,
      p.metadata_codec,p.materialization_policy,p.fs_semantics,p.access_projection,p.verification_revision,p.projection_revision);
    INSERT INTO $CORE_SCHEMA$.mst2_snapshot_storage_route(snapshot_id,namespace_uuid,canonical_descriptor,instance_id,
      commit_oid,root_tree_oid,metadata_root,source_profile)
      VALUES(sid,'$NAMESPACE_UUID$'::uuid,descriptor,instance,commit_id,tree_id,p.metadata_root,profile)
      ON CONFLICT(snapshot_id) DO NOTHING;
    INSERT INTO mst2_qualified_session_incarnation(snapshot_id,session_incarnation,namespace_uuid,prepare_id,storage_seal,
      metadata_root,root_generation,source_profile,instance_id,commit_oid,root_tree_oid,authorization_epoch,
      publication_sequence,writer_epoch,certificate_receipt_id,canonical_descriptor,attestation_id,attestation_digest,state)
      VALUES(sid,inc,'$NAMESPACE_UUID$'::uuid,pid,p.storage_seal,p.metadata_root,a.root_generation,convert_to(profile::text,'UTF8'),
        instance,commit_id,tree_id,1,(request->>'publication_sequence')::bigint,(request->>'writer_epoch')::bigint,
        (request->>'certificate_receipt_id')::bigint,descriptor,a.attestation_id,a.attestation_digest,'READY') RETURNING * INTO s;
    INSERT INTO mst2_metadata_root_anchor(anchor_id,anchor_kind,owner_key,root_page,root_generation,root_certificate_digest,
      snapshot_id,session_incarnation) VALUES(gen_random_uuid(),'SESSION',sid||':'||inc::text,
        s.metadata_root,s.root_generation,a.root_certificate_digest,sid,inc);
  END IF;
  SELECT root_certificate_digest INTO STRICT a FROM mst2_metadata_source_root_attestation WHERE attestation_id=s.attestation_id;
  INSERT INTO mst2_qualified_lease_binding(lease_id,snapshot_id,session_incarnation,prepare_id,storage_seal,metadata_root,
    root_generation,namespace_uuid,authorization_epoch,publication_sequence,writer_epoch,certificate_receipt_id,
    expires_at_unix,state,lease_epoch) VALUES(lid,sid,s.session_incarnation,s.prepare_id,s.storage_seal,s.metadata_root,
      s.root_generation,s.namespace_uuid,s.authorization_epoch,s.publication_sequence,s.writer_epoch,s.certificate_receipt_id,
      deadline,'ACTIVE',1);
  INSERT INTO $CORE_SCHEMA$.mst2_lease_storage_route(lease_id,snapshot_id,namespace_uuid,session_incarnation,prepare_id,
    metadata_root,authorization_epoch,publication_sequence,writer_epoch,certificate_receipt_id)
    VALUES(lid,sid,s.namespace_uuid,s.session_incarnation,s.prepare_id,s.metadata_root,s.authorization_epoch,
      s.publication_sequence,s.writer_epoch,s.certificate_receipt_id);
  INSERT INTO mst2_metadata_root_anchor(anchor_id,anchor_kind,owner_key,root_page,root_generation,root_certificate_digest,
    snapshot_id,session_incarnation,lease_id) VALUES(gen_random_uuid(),'LEASE',lid,s.metadata_root,s.root_generation,
      a.root_certificate_digest,sid,s.session_incarnation,lid);
  IF pid IS NOT NULL THEN
    SELECT coverage_retired_at INTO p FROM mst2_metadata_prepare WHERE prepare_id=pid AND state='COMMITTED' AND plan_kind='ROOTED'
      AND storage_seal=decode(request->>'storage_seal','hex') AND metadata_root=s.metadata_root;
    IF NOT FOUND OR s.root_generation IS DISTINCT FROM (request->>'root_generation')::bigint
      OR a.root_certificate_digest IS DISTINCT FROM decode(request->>'certificate_digest','hex')
      OR descriptor IS DISTINCT FROM mst2_metadata_descriptor(pid,instance,commit_id,tree_id)
      OR NOT mst2_metadata_session_covers_prepare(pid)
      OR NOT EXISTS(SELECT 1 FROM mst2_metadata_source_root_attestation receipt
        WHERE receipt.attestation_id=(request->>'attestation_id')::uuid
          AND receipt.attestation_digest=decode(request->>'attestation_digest','hex')
          AND receipt.root_page=s.metadata_root AND receipt.root_generation=s.root_generation
          AND receipt.root_certificate_digest=a.root_certificate_digest
          AND mst2_metadata_serving_source(pid,receipt.attestation_id,s.metadata_root,s.root_generation,a.root_certificate_digest)) THEN
      RAISE EXCEPTION 'qualified losing preparation cannot retire a different serving identity'; END IF;
    IF p.coverage_retired_at IS NULL THEN
      UPDATE mst2_metadata_prepare SET coverage_retired_at=clock_timestamp() WHERE prepare_id=pid;
      DELETE FROM mst2_metadata_root_anchor WHERE prepare_id=pid AND anchor_kind IN ('PREPARE','REUSE');
    END IF;
  END IF;
  RETURN QUERY SELECT lid,deadline;
END $$;

CREATE FUNCTION mst2_metadata_cleanup_lease(lid text) RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_qualified_lease_binding%ROWTYPE;
BEGIN
  SELECT * INTO l FROM mst2_qualified_lease_binding WHERE lease_id=lid;
  IF NOT FOUND THEN RETURN; END IF;
  IF l.state<>'ACTIVE' AND NOT EXISTS(SELECT 1 FROM mst2_metadata_reader_operation WHERE lease_id=lid AND state='ACTIVE') THEN
    DELETE FROM mst2_metadata_root_anchor WHERE lease_id=lid AND anchor_kind='LEASE';
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_qualified_lease_binding lease WHERE lease.snapshot_id=l.snapshot_id
      AND lease.session_incarnation=l.session_incarnation AND lease.state='ACTIVE') THEN
    UPDATE mst2_qualified_session_incarnation SET state='RETIRED'
      WHERE snapshot_id=l.snapshot_id AND session_incarnation=l.session_incarnation AND state='READY';
    DELETE FROM mst2_metadata_root_anchor WHERE snapshot_id=l.snapshot_id
      AND session_incarnation=l.session_incarnation AND anchor_kind='SESSION';
  END IF;
END $$;

CREATE FUNCTION mst2_metadata_cleanup_expired(maximum integer DEFAULT 64) RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE item record; now_unix bigint; bound integer:=least(64,greatest(0,maximum));
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  now_unix:=floor(extract(epoch FROM clock_timestamp()))::bigint;
  FOR item IN SELECT * FROM (
      (SELECT 'READER'::text AS kind,operation_id::text AS owner,lease_id,hard_deadline_unix AS deadline,reader_issuance
        FROM mst2_metadata_reader_operation WHERE state='ACTIVE' AND hard_deadline_unix<=now_unix
        ORDER BY hard_deadline_unix,operation_id LIMIT bound)
      UNION ALL
      (SELECT 'LEASE'::text,lease_id,lease_id,expires_at_unix,NULL::bigint
        FROM mst2_qualified_lease_binding WHERE state='ACTIVE' AND expires_at_unix<=now_unix
        ORDER BY expires_at_unix,lease_id LIMIT bound)
    ) expired ORDER BY deadline,kind,owner LIMIT bound LOOP
    IF item.kind='READER' THEN
      UPDATE mst2_metadata_reader_operation SET state='EXPIRED' WHERE operation_id=item.owner::uuid AND reader_issuance=item.reader_issuance AND state='ACTIVE';
      DELETE FROM mst2_metadata_root_anchor WHERE reader_operation_id=item.owner::uuid AND reader_issuance=item.reader_issuance AND anchor_kind IN ('REQUEST','READER');
    ELSE
      UPDATE mst2_qualified_lease_binding SET state='EXPIRED',lease_epoch=lease_epoch+1 WHERE lease_id=item.owner AND state='ACTIVE';
    END IF;
    PERFORM mst2_metadata_cleanup_lease(item.lease_id);
  END LOOP;
END $$;

CREATE FUNCTION mst2_metadata_renew_lease(lid text,seconds bigint,instance text)
RETURNS TABLE(snapshot_id text,expires_at_unix bigint) LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_qualified_lease_binding%ROWTYPE; now_unix bigint; deadline bigint;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  PERFORM mst2_metadata_cleanup_expired(64);
  now_unix:=floor(extract(epoch FROM clock_timestamp()))::bigint;
  SELECT * INTO l FROM mst2_qualified_lease_binding WHERE lease_id=lid AND state='ACTIVE';
  IF NOT FOUND THEN RETURN; END IF;
  IF l.expires_at_unix<=now_unix THEN
    UPDATE mst2_qualified_lease_binding SET state='EXPIRED',lease_epoch=lease_epoch+1 WHERE lease_id=lid;
    PERFORM mst2_metadata_cleanup_lease(lid); RETURN;
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_metadata_session_row(l.snapshot_id,lid,instance)) THEN RETURN; END IF;
  deadline:=greatest(l.expires_at_unix,now_unix+least(3600,greatest(1,seconds)));
  UPDATE mst2_qualified_lease_binding lease SET expires_at_unix=deadline WHERE lease.lease_id=lid;
  RETURN QUERY SELECT l.snapshot_id,deadline;
END $$;

CREATE FUNCTION mst2_metadata_release_lease(lid text) RETURNS boolean LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_qualified_lease_binding%ROWTYPE; changed boolean;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  PERFORM mst2_metadata_cleanup_expired(64);
  SELECT * INTO l FROM mst2_qualified_lease_binding WHERE lease_id=lid;
  IF NOT FOUND THEN RETURN false; END IF;
  IF NOT mst2_metadata_lease_route_proof(lid,l.snapshot_id,l.namespace_uuid,l.session_incarnation,l.prepare_id,l.metadata_root,
    l.authorization_epoch,l.publication_sequence,l.writer_epoch,l.certificate_receipt_id) THEN
    RAISE EXCEPTION 'qualified release lost its fixed historical source identity'; END IF;
  changed:=l.state='ACTIVE';
  IF changed THEN UPDATE mst2_qualified_lease_binding SET state='RELEASED',lease_epoch=lease_epoch+1 WHERE lease_id=lid; END IF;
  PERFORM mst2_metadata_cleanup_lease(lid);
  RETURN changed;
END $$;

DROP TRIGGER mst2_01_family_closed ON mst2_qualified_session_incarnation;
DROP TRIGGER mst2_01_family_closed ON mst2_qualified_lease_binding;
DROP TRIGGER mst2_01_family_closed ON mst2_metadata_reader_operation;
