-- Captured exactly from c1ff280281440891e732ecfaed4732ee59c41a90; test-only.
-- READER DDL BEGIN
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

-- READER DDL END

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
    SELECT * INTO STRICT r FROM mst2_metadata_reader_operation WHERE operation_id=NEW.operation_id;
    IF r.state='ACTIVE' AND (r.hard_deadline_unix<=floor(extract(epoch FROM clock_timestamp()))::bigint
      OR (SELECT count(*) FROM mst2_metadata_root_anchor a WHERE a.reader_operation_id=r.operation_id
        AND a.anchor_kind IN ('REQUEST','READER') AND a.owner_key=r.operation_id::text
        AND a.lease_id=r.lease_id AND a.snapshot_id=r.snapshot_id AND a.session_incarnation=r.session_incarnation
        AND a.root_page=r.root_page AND a.root_generation=r.root_generation)<>2)
      OR r.state<>'ACTIVE' AND EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.reader_operation_id=r.operation_id) THEN
      RAISE EXCEPTION 'qualified reader cannot commit without both exact owned roots or definitive cleanup'; END IF;
  END IF;
  RETURN NULL;
END $$;

CREATE FUNCTION mst2_metadata_reader_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_qualified_lease_binding%ROWTYPE; now_unix bigint:=floor(extract(epoch FROM clock_timestamp()))::bigint;
BEGIN
  IF TG_OP='DELETE' THEN RAISE EXCEPTION 'qualified reader operation history is immutable'; END IF;
  IF TG_OP='UPDATE' THEN
    IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state')
      OR OLD.state<>'ACTIVE' AND NEW.state<>OLD.state OR NEW.state NOT IN ('ACTIVE','FINISHED','EXPIRED')
      OR NEW.state='EXPIRED' AND OLD.hard_deadline_unix>now_unix THEN
      RAISE EXCEPTION 'qualified reader identity cannot change or be prematurely expired'; END IF;
    RETURN NEW;
  END IF;
  SELECT * INTO l FROM mst2_qualified_lease_binding WHERE lease_id=NEW.lease_id AND state='ACTIVE' AND expires_at_unix>now_unix;
  IF NOT FOUND OR NEW.state<>'ACTIVE' OR substr(NEW.operation_id::text,15,1)<>'4'
    OR substr(NEW.operation_id::text,20,1) NOT IN ('8','9','a','b')
    OR ROW(NEW.snapshot_id,NEW.session_incarnation,NEW.root_page,NEW.root_generation,NEW.lease_epoch) IS DISTINCT FROM
      ROW(l.snapshot_id,l.session_incarnation,l.metadata_root,l.root_generation,l.lease_epoch)
    OR NEW.hard_deadline_unix<=now_unix OR NEW.hard_deadline_unix>least(l.expires_at_unix,now_unix+60)
    OR NOT mst2_metadata_incarnation_proof(l.snapshot_id,l.session_incarnation,true) THEN
    RAISE EXCEPTION 'qualified reader lacks its exact active lease and bounded deadline'; END IF;
  RETURN NEW;
END $$;

CREATE FUNCTION mst2_metadata_begin_reader(sid text,lid text,instance text)
RETURNS TABLE(operation_id uuid,root_generation bigint,certificate_digest bytea) LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_qualified_lease_binding%ROWTYPE; s record; op uuid:=gen_random_uuid(); deadline bigint; kind text;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  PERFORM mst2_metadata_cleanup_expired(64);
  SELECT * INTO s FROM mst2_metadata_session_row(sid,lid,instance);
  IF NOT FOUND THEN RETURN; END IF;
  SELECT * INTO STRICT l FROM mst2_qualified_lease_binding WHERE lease_id=lid;
  deadline:=least(l.expires_at_unix,floor(extract(epoch FROM clock_timestamp()))::bigint+60);
  INSERT INTO mst2_metadata_reader_operation(operation_id,lease_id,snapshot_id,session_incarnation,root_page,root_generation,
    lease_epoch,hard_deadline_unix,state) VALUES(op,lid,sid,l.session_incarnation,l.metadata_root,l.root_generation,l.lease_epoch,deadline,'ACTIVE');
  FOREACH kind IN ARRAY ARRAY['REQUEST','READER'] LOOP
    INSERT INTO mst2_metadata_root_anchor(anchor_id,anchor_kind,owner_key,root_page,root_generation,root_certificate_digest,
      snapshot_id,session_incarnation,lease_id,reader_operation_id) VALUES(gen_random_uuid(),kind,op::text,
        l.metadata_root,l.root_generation,s.certificate_digest,sid,l.session_incarnation,lid,op);
  END LOOP;
  RETURN QUERY SELECT op,l.root_generation,s.certificate_digest::bytea;
END $$;

CREATE FUNCTION mst2_metadata_finish_reader(op uuid) RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE r mst2_metadata_reader_operation%ROWTYPE;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  SELECT * INTO r FROM mst2_metadata_reader_operation WHERE operation_id=op;
  IF NOT FOUND THEN RETURN; END IF;
  IF r.state='ACTIVE' THEN UPDATE mst2_metadata_reader_operation SET state='FINISHED' WHERE operation_id=op; END IF;
  DELETE FROM mst2_metadata_root_anchor WHERE reader_operation_id=op AND anchor_kind IN ('REQUEST','READER');
  PERFORM mst2_metadata_cleanup_lease(r.lease_id);
END $$;

CREATE FUNCTION mst2_metadata_read_source_entries(op uuid,source_id uuid,p bytea,g bigint,c bytea,names jsonb)
RETURNS TABLE(name bytea,git_oid text,kind smallint,byte_size bigint,content_digest bytea,child_root bytea,
  child_generation bigint,child_certificate_digest bytea,child_attestation_id uuid,fact_state text)
LANGUAGE plpgsql VOLATILE SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE a record; profile jsonb;
BEGIN
  IF jsonb_typeof(names)<>'array' OR jsonb_array_length(names)>256
    OR EXISTS(SELECT 1 FROM jsonb_array_elements_text(names) wanted
      WHERE wanted !~ '^([0-9a-f]{2}){1,255}$') THEN
    RAISE EXCEPTION 'qualified source-name read exceeds its bounded exact request'; END IF;
  SELECT source.namespace_uuid,source.source_profile,source.tagged_tree_oid,source.source_body_digest,source.source_revision,
    source.root_page,source.root_generation,source.root_certificate_digest
    INTO a FROM mst2_metadata_source_root_attestation source
    JOIN mst2_metadata_prepare origin ON origin.prepare_id=source.origin_prepare_id
    WHERE source.attestation_id=source_id AND origin.state='COMMITTED'
      AND source.root_page=p AND source.root_generation=g AND source.root_certificate_digest=c
      AND source.namespace_uuid='$NAMESPACE_UUID$'::uuid;
  IF NOT FOUND THEN RAISE EXCEPTION 'qualified selected directory has no definitive source attestation'; END IF;
  SELECT mst2_metadata_native_profile(session.prepare_id) INTO profile
    FROM mst2_metadata_reader_operation reader JOIN mst2_qualified_session_incarnation session
      ON session.snapshot_id=reader.snapshot_id AND session.session_incarnation=reader.session_incarnation
    WHERE reader.operation_id=op AND reader.state='ACTIVE'
      AND reader.hard_deadline_unix>floor(extract(epoch FROM clock_timestamp()))::bigint
      AND EXISTS(SELECT 1 FROM mst2_metadata_root_anchor anchor WHERE anchor.reader_operation_id=reader.operation_id
        AND anchor.anchor_kind='READER' AND anchor.root_page=reader.root_page AND anchor.root_generation=reader.root_generation);
  IF NOT FOUND OR profile IS DISTINCT FROM a.source_profile
    OR NOT mst2_metadata_root_live(a.root_page,a.root_generation,a.root_certificate_digest) THEN
    RAISE EXCEPTION 'qualified source read lost its exact reader profile and current directory'; END IF;
  IF NOT $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest) THEN
    RAISE EXCEPTION 'qualified selected directory source body changed'; END IF;
  RETURN QUERY SELECT reference.name,reference.git_oid,reference.kind,reference.byte_size,reference.content_digest,
    reference.child_root,reference.child_generation,reference.child_certificate_digest,child.attestation_id,
    CASE WHEN reference.kind=4 THEN CASE WHEN child.attestation_id IS NULL THEN 'SOURCE_UNAVAILABLE' ELSE 'READY' END
      WHEN fact.git_oid IS NULL THEN 'MISSING'
      WHEN fact.state<>'VERIFIED' OR fact.verification_version NOT IN (1,2)
        OR fact.size NOT BETWEEN 0 AND 8796093022208 OR octet_length(fact.raw_sha256)<>32 THEN 'INVALID'
      WHEN fact.verification_version=1 THEN 'MISSING'
      WHEN fact.size IS DISTINCT FROM reference.byte_size OR reference.kind=3 AND fact.size NOT BETWEEN 1 AND 4095
        OR CASE WHEN octet_length(fact.raw_sha256)=32 THEN fact.raw_sha256 ELSE NULL END
          IS DISTINCT FROM reference.content_digest THEN 'INVALID'
      ELSE 'READY' END
    FROM (SELECT DISTINCT decode(value,'hex') AS name FROM jsonb_array_elements_text(names)) wanted
    JOIN mst2_metadata_source_entry_reference reference ON reference.attestation_id=source_id AND reference.name=wanted.name
    LEFT JOIN LATERAL (
      SELECT verified.git_oid,verified.state,verified.verification_version,verified.size,verified.raw_sha256
        FROM $CORE_SCHEMA$.mst2_verified_object verified WHERE reference.kind<>4 AND verified.storage_domain='git'
          AND verified.object_kind='blob' AND verified.git_oid=split_part(reference.git_oid,':',2)
        FOR SHARE OF verified NOWAIT
    ) fact ON true
    LEFT JOIN LATERAL (
      SELECT candidate.attestation_id FROM mst2_metadata_source_root_attestation candidate
        JOIN mst2_metadata_prepare origin ON origin.prepare_id=candidate.origin_prepare_id
        JOIN $CORE_SCHEMA$.mega_tree source_tree ON source_tree.tree_id=split_part(candidate.tagged_tree_oid,':',2)
        WHERE reference.kind=4 AND candidate.namespace_uuid=a.namespace_uuid AND origin.state='COMMITTED'
          AND candidate.tagged_tree_oid=reference.git_oid AND candidate.source_profile=a.source_profile
          AND candidate.root_page=reference.child_root AND candidate.root_generation=reference.child_generation
          AND candidate.root_certificate_digest=reference.child_certificate_digest
          AND $CORE_SCHEMA$.mst2_route_source_tree_matches(split_part(candidate.tagged_tree_oid,':',2),candidate.source_revision,candidate.source_body_digest)
          AND mst2_metadata_root_live(candidate.root_page,candidate.root_generation,candidate.root_certificate_digest)
        ORDER BY candidate.attestation_id LIMIT 1
    ) child ON true ORDER BY reference.name;
END $$;

CREATE FUNCTION mst2_metadata_cleanup_expired(maximum integer DEFAULT 64) RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE item record; now_unix bigint; bound integer:=least(64,greatest(0,maximum));
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  now_unix:=floor(extract(epoch FROM clock_timestamp()))::bigint;
  FOR item IN SELECT * FROM (
      (SELECT 'READER'::text AS kind,operation_id::text AS owner,lease_id,hard_deadline_unix AS deadline
        FROM mst2_metadata_reader_operation WHERE state='ACTIVE' AND hard_deadline_unix<=now_unix
        ORDER BY hard_deadline_unix,operation_id LIMIT bound)
      UNION ALL
      (SELECT 'LEASE'::text,lease_id,lease_id,expires_at_unix
        FROM mst2_qualified_lease_binding WHERE state='ACTIVE' AND expires_at_unix<=now_unix
        ORDER BY expires_at_unix,lease_id LIMIT bound)
    ) expired ORDER BY deadline,kind,owner LIMIT bound LOOP
    IF item.kind='READER' THEN
      UPDATE mst2_metadata_reader_operation SET state='EXPIRED' WHERE operation_id=item.owner::uuid AND state='ACTIVE';
      DELETE FROM mst2_metadata_root_anchor WHERE reader_operation_id=item.owner::uuid AND anchor_kind IN ('REQUEST','READER');
    ELSE
      UPDATE mst2_qualified_lease_binding SET state='EXPIRED',lease_epoch=lease_epoch+1 WHERE lease_id=item.owner AND state='ACTIVE';
    END IF;
    PERFORM mst2_metadata_cleanup_lease(item.lease_id);
  END LOOP;
END $$;

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
      (SELECT 'READER'::text AS kind,operation_id::text AS owner,lease_id,hard_deadline_unix AS deadline
        FROM mst2_metadata_reader_operation WHERE state='ACTIVE' AND hard_deadline_unix<=now_unix
        ORDER BY hard_deadline_unix,operation_id LIMIT bound)
      UNION ALL
      (SELECT 'LEASE'::text,lease_id,lease_id,expires_at_unix FROM mst2_qualified_lease_binding
        WHERE state='ACTIVE' AND expires_at_unix<=now_unix ORDER BY expires_at_unix,lease_id LIMIT bound)
      UNION ALL
      (SELECT 'PREPARE'::text,prepare_id,NULL::text,floor(extract(epoch FROM orphan_expires_at))::bigint
        FROM mst2_metadata_prepare WHERE (state='PREPARING' OR state='COMMITTED' AND coverage_retired_at IS NULL)
          AND orphan_expires_at<=clock_timestamp() ORDER BY orphan_expires_at,prepare_id LIMIT bound)
    ) expired ORDER BY deadline,kind,owner LIMIT bound LOOP
    examined:=examined+1;
    IF item.kind='READER' THEN
      UPDATE mst2_metadata_reader_operation SET state='EXPIRED' WHERE operation_id=item.owner::uuid AND state='ACTIVE';
      IF NOT FOUND THEN RAISE EXCEPTION 'qualified expired reader changed behind its mutation barrier'; END IF;
      readers_expired:=readers_expired+1;
      DELETE FROM mst2_metadata_root_anchor WHERE reader_operation_id=item.owner::uuid AND anchor_kind IN ('REQUEST','READER');
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

CREATE FUNCTION mst2_route_family_registration_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE g mst2_metadata_namespace%ROWTYPE; stamp record;
BEGIN
  SELECT * INTO STRICT g FROM mst2_metadata_namespace WHERE singleton=1;
  IF (SELECT count(*) FROM mst2_metadata_namespace)<>1 OR NEW.singleton IS NOT NULL
    OR substr(NEW.namespace_uuid::text,15,1)<>'4' OR substr(NEW.namespace_uuid::text,20,1) NOT IN ('8','9','a','b')
    OR NEW.graph_domain<>'qualified-v1' OR NEW.family_identity<>'v3-rooted-qualified-1'
    OR NEW.admission_state<>'ROOTED_Q_ADMITTED' OR NEW.collector_state<>'ENABLED'
    OR ROW(NEW.core_schema,NEW.core_schema_oid,NEW.database_name,NEW.database_oid,NEW.storage_uuid,
      NEW.server_address,NEW.server_port,NEW.mono_lock_key2) IS DISTINCT FROM
      ROW(g.core_schema,g.core_schema_oid,g.database_name,g.database_oid,g.storage_uuid,
      g.server_address,g.server_port,g.mono_lock_key2)
    OR NEW.metadata_schema IS DISTINCT FROM 'mst2q_'||replace(NEW.namespace_uuid::text,'-','')
    OR NEW.metadata_storage_uuid IS NOT DISTINCT FROM g.storage_uuid
    OR NEW.metadata_storage_uuid IS DISTINCT FROM (NEW.metadata_storage_uuid::uuid)::text
    OR substr(NEW.metadata_storage_uuid,15,1)<>'4' OR substr(NEW.metadata_storage_uuid,20,1) NOT IN ('8','9','a','b')
    OR NOT EXISTS(SELECT 1 FROM pg_catalog.pg_namespace n
      WHERE n.oid=NEW.metadata_schema_oid AND n.nspname=NEW.metadata_schema)
    OR NEW.implementation_fingerprint IS DISTINCT FROM decode('$IMPLEMENTATION_SHA$','hex')
    OR NOT mst2_route_lock_held(1297043024,g.mono_lock_key2)
    OR NOT mst2_route_lock_held(1296718001,pg_catalog.hashtext(g.core_schema))
    OR NOT mst2_route_lock_held(1296717362,pg_catalog.hashtext(NEW.metadata_schema))
    OR NOT mst2_route_lock_held(1296717362,pg_catalog.hashtext(g.metadata_schema)) THEN
    RAISE EXCEPTION 'qualified namespace registration has no exact admitted rooted family scope and lock set';
  END IF;
  EXECUTE pg_catalog.format('SELECT * FROM %I.mst2_metadata_family_identity WHERE singleton=1',NEW.metadata_schema)
    INTO STRICT stamp;
  IF ROW(stamp.namespace_uuid,stamp.storage_uuid,stamp.core_schema_oid,stamp.metadata_schema_oid,
    stamp.family_identity,stamp.implementation_fingerprint) IS DISTINCT FROM
    ROW(NEW.namespace_uuid,NEW.metadata_storage_uuid,NEW.core_schema_oid,NEW.metadata_schema_oid,
    NEW.family_identity,NEW.implementation_fingerprint)
    OR NEW.catalog_fingerprint IS DISTINCT FROM mst2_route_family_catalog(NEW.core_schema_oid,NEW.metadata_schema_oid) THEN
    RAISE EXCEPTION 'qualified namespace registration fingerprint disagrees with its physical family';
  END IF;
  IF NOT EXISTS(SELECT 1 FROM mst2_qualified_family_policy p WHERE p.singleton=1
    AND p.implementation_fingerprint=NEW.implementation_fingerprint
    AND p.authority_catalog=mst2_route_family_catalog(NEW.core_schema_oid,0::oid,NEW.metadata_schema_oid)
    AND p.expected_shape=mst2_route_family_shape(NEW.metadata_schema_oid,NEW.namespace_uuid,NEW.metadata_storage_uuid)) THEN
    RAISE EXCEPTION 'qualified namespace does not have the trusted complete physical family shape';
  END IF;
  RETURN NEW;
END $$;
