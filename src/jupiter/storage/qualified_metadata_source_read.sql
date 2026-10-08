-- Indexed fixed-source names are independently derived by the attestation
-- proof. Selected windows revalidate current file facts without source scans.
CREATE TABLE mst2_metadata_source_entry_reference (
  attestation_id uuid NOT NULL REFERENCES mst2_metadata_source_root_attestation(attestation_id),
  name bytea NOT NULL CHECK(octet_length(name) BETWEEN 1 AND 255),
  git_oid text NOT NULL CHECK(git_oid ~ '^(sha1:[0-9a-f]{40}|sha256:[0-9a-f]{64}|blake3:[0-9a-f]{64})$'),
  kind smallint NOT NULL CHECK(kind BETWEEN 1 AND 4),
  byte_size bigint,content_digest bytea,child_root bytea,child_generation bigint,child_certificate_digest bytea,
  PRIMARY KEY(attestation_id,name),
  CHECK(((kind=4 AND byte_size IS NULL AND content_digest IS NULL AND octet_length(child_root)=32
      AND child_generation>0 AND octet_length(child_certificate_digest)=32)
    OR (kind<>4 AND byte_size BETWEEN 0 AND 8796093022208 AND octet_length(content_digest)=32
      AND child_root IS NULL AND child_generation IS NULL AND child_certificate_digest IS NULL)) IS TRUE),
  CHECK((kind<>3 OR byte_size BETWEEN 1 AND 4095) IS TRUE)
);

CREATE FUNCTION mst2_metadata_source_entry_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  RAISE EXCEPTION 'qualified fixed source entry history is immutable';
END $$;
CREATE TRIGGER mst2_metadata_source_entry_guard BEFORE UPDATE OR DELETE ON mst2_metadata_source_entry_reference
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_source_entry_guard();

CREATE FUNCTION mst2_metadata_source_entries_proof() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE source_id uuid; expected jsonb; supplied jsonb;
BEGIN
  FOR source_id IN SELECT DISTINCT attestation_id FROM added_source_entries LOOP
    SELECT source_proof->'source_entries' INTO expected FROM mst2_metadata_source_root_attestation
      WHERE attestation_id=source_id;
    SELECT jsonb_object_agg(encode(reference.name,'hex'),
      jsonb_build_object('kind',reference.kind,'git_oid',reference.git_oid)||CASE WHEN reference.kind=4
        THEN jsonb_build_object('child_root',encode(reference.child_root,'hex'),
          'child_generation',reference.child_generation,'child_certificate',encode(reference.child_certificate_digest,'hex'))
        ELSE jsonb_build_object('size',reference.byte_size,'content_digest',encode(reference.content_digest,'hex')) END)
      INTO supplied FROM added_source_entries reference WHERE reference.attestation_id=source_id;
    IF expected IS NULL OR expected IS DISTINCT FROM supplied THEN
      RAISE EXCEPTION 'fixed source entry batch differs from independently derived complete body and fact proof'; END IF;
  END LOOP;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_source_entries_proof AFTER INSERT ON mst2_metadata_source_entry_reference
  REFERENCING NEW TABLE AS added_source_entries FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_source_entries_proof();

CREATE FUNCTION mst2_metadata_source_entries_install() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  INSERT INTO mst2_metadata_source_entry_reference(attestation_id,name,git_oid,kind,byte_size,content_digest,
    child_root,child_generation,child_certificate_digest)
    SELECT NEW.attestation_id,decode(key,'hex'),value->>'git_oid',(value->>'kind')::smallint,
      (value->>'size')::bigint,decode(value->>'content_digest','hex'),decode(value->>'child_root','hex'),
      (value->>'child_generation')::bigint,decode(value->>'child_certificate','hex')
    FROM jsonb_each(NEW.source_proof->'source_entries');
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_metadata_source_entries_install AFTER INSERT ON mst2_metadata_source_root_attestation
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_source_entries_install();

CREATE FUNCTION mst2_metadata_source_entries_complete() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF (SELECT count(*) FROM mst2_metadata_source_entry_reference WHERE attestation_id=NEW.attestation_id)
    IS DISTINCT FROM (NEW.source_proof->>'source_entry_count')::bigint THEN
    RAISE EXCEPTION 'source attestation cannot commit with incomplete exact name references'; END IF;
  RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER mst2_metadata_source_entries_complete AFTER INSERT ON mst2_metadata_source_root_attestation
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_metadata_source_entries_complete();

CREATE FUNCTION mst2_metadata_read_source_entries(op uuid,issuance bigint,source_id uuid,p bytea,g bigint,c bytea,names jsonb)
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
    WHERE reader.operation_id=op AND reader.reader_issuance=issuance AND reader.state='ACTIVE'
      AND reader.hard_deadline_unix>floor(extract(epoch FROM clock_timestamp()))::bigint
      AND EXISTS(SELECT 1 FROM mst2_metadata_root_anchor anchor WHERE anchor.reader_operation_id=reader.operation_id AND anchor.reader_issuance=reader.reader_issuance
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
