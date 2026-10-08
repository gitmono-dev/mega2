CREATE FUNCTION mst2_metadata_next_reader_issuance(issued bigint) RETURNS bigint LANGUAGE plpgsql IMMUTABLE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF issued IS NULL OR issued<0 OR issued=9223372036854775807 THEN
    RAISE EXCEPTION 'qualified reader issuance is exhausted or invalid'; END IF;
  RETURN issued+1;
END $$;

CREATE FUNCTION mst2_metadata_reader_issuance_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_OP<>'UPDATE' THEN RAISE EXCEPTION 'qualified reader issuance cannot be reset or removed'; END IF;
  IF NEW.singleton IS DISTINCT FROM OLD.singleton OR NEW.high_water IS DISTINCT FROM mst2_metadata_next_reader_issuance(OLD.high_water) THEN
    RAISE EXCEPTION 'qualified reader issuance must advance exactly once'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_reader_issuance_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_reader_issuance
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_reader_issuance_guard();

CREATE FUNCTION mst2_metadata_reader_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_qualified_lease_binding%ROWTYPE; issued bigint;
  now_unix bigint:=floor(extract(epoch FROM clock_timestamp()))::bigint;
BEGIN
  IF TG_OP='DELETE' THEN
    IF OLD.state NOT IN ('FINISHED','EXPIRED') OR OLD.terminal_xid IS NULL OR OLD.terminal_xid=txid_current()
      OR OLD.state='EXPIRED' AND OLD.hard_deadline_unix>now_unix
      OR EXISTS(SELECT 1 FROM mst2_metadata_root_anchor WHERE reader_operation_id=OLD.operation_id) THEN
      RAISE EXCEPTION 'qualified reader still has its active owner, deferred completion, or owned roots'; END IF;
    RETURN OLD;
  END IF;
  IF TG_OP='UPDATE' THEN
    IF (to_jsonb(NEW)-'state') IS DISTINCT FROM (to_jsonb(OLD)-'state')
      OR OLD.state<>'ACTIVE' AND NEW.state<>OLD.state OR NEW.state NOT IN ('ACTIVE','FINISHED','EXPIRED')
      OR NEW.state='EXPIRED' AND OLD.hard_deadline_unix>now_unix THEN
      RAISE EXCEPTION 'qualified reader identity cannot change or be prematurely expired'; END IF;
    IF OLD.state<>'ACTIVE' THEN RETURN NULL; END IF;
    IF NEW.state<>'ACTIVE' THEN NEW.terminal_xid:=txid_current(); END IF;
    RETURN NEW;
  END IF;
  SELECT * INTO l FROM mst2_qualified_lease_binding WHERE lease_id=NEW.lease_id AND state='ACTIVE' AND expires_at_unix>now_unix;
  IF NOT FOUND OR NEW.state<>'ACTIVE' OR NEW.terminal_xid IS NOT NULL OR NEW.reader_issuance<=0
    OR substr(NEW.operation_id::text,15,1)<>'4' OR substr(NEW.operation_id::text,20,1) NOT IN ('8','9','a','b')
    OR ROW(NEW.snapshot_id,NEW.session_incarnation,NEW.root_page,NEW.root_generation,NEW.lease_epoch) IS DISTINCT FROM
      ROW(l.snapshot_id,l.session_incarnation,l.metadata_root,l.root_generation,l.lease_epoch)
    OR NEW.hard_deadline_unix<=now_unix OR NEW.hard_deadline_unix>least(l.expires_at_unix,now_unix+60)
    OR NOT mst2_metadata_incarnation_proof(l.snapshot_id,l.session_incarnation,true) THEN
    RAISE EXCEPTION 'qualified reader lacks its exact active lease and bounded deadline'; END IF;
  SELECT high_water INTO STRICT issued FROM mst2_metadata_reader_issuance WHERE singleton=1;
  IF NEW.reader_issuance<>mst2_metadata_next_reader_issuance(issued) THEN RAISE EXCEPTION 'qualified reader cannot replay an issued identity'; END IF;
  UPDATE mst2_metadata_reader_issuance SET high_water=NEW.reader_issuance WHERE singleton=1;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_metadata_reader_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_reader_operation
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_reader_guard();

CREATE FUNCTION mst2_metadata_prune_readers(maximum integer) RETURNS bigint LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE removed bigint; now_unix bigint:=floor(extract(epoch FROM clock_timestamp()))::bigint;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  IF maximum IS NULL OR maximum NOT BETWEEN 0 AND 64 THEN RAISE EXCEPTION 'qualified reader prune budget must be 0..=64'; END IF;
  WITH candidates AS (
    SELECT r.operation_id,r.reader_issuance FROM mst2_metadata_reader_operation r
    WHERE r.state IN ('FINISHED','EXPIRED') AND r.terminal_xid<txid_current()
      AND (r.state='FINISHED' OR r.hard_deadline_unix<=now_unix)
      AND NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.reader_operation_id=r.operation_id)
    ORDER BY r.terminal_xid,r.operation_id LIMIT maximum
  ), deleted AS (
    DELETE FROM mst2_metadata_reader_operation r USING candidates c
    WHERE r.operation_id=c.operation_id AND r.reader_issuance=c.reader_issuance RETURNING r.operation_id
  ) SELECT count(*) INTO removed FROM deleted;
  RETURN removed;
END $$;

CREATE FUNCTION mst2_metadata_begin_reader(sid text,lid text,instance text)
RETURNS TABLE(operation_id uuid,reader_issuance bigint,root_generation bigint,certificate_digest bytea) LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE l mst2_qualified_lease_binding%ROWTYPE; s record; op uuid:=gen_random_uuid(); issued bigint; deadline bigint; kind text;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  PERFORM mst2_metadata_prune_readers(64);
  PERFORM mst2_metadata_cleanup_expired(64);
  SELECT * INTO s FROM mst2_metadata_session_row(sid,lid,instance);
  IF NOT FOUND THEN RETURN; END IF;
  SELECT * INTO STRICT l FROM mst2_qualified_lease_binding WHERE lease_id=lid;
  SELECT high_water INTO STRICT issued FROM mst2_metadata_reader_issuance WHERE singleton=1;
  issued:=mst2_metadata_next_reader_issuance(issued);
  deadline:=least(l.expires_at_unix,floor(extract(epoch FROM clock_timestamp()))::bigint+60);
  INSERT INTO mst2_metadata_reader_operation(operation_id,lease_id,snapshot_id,session_incarnation,root_page,root_generation,
    lease_epoch,hard_deadline_unix,state,reader_issuance) VALUES(op,lid,sid,l.session_incarnation,l.metadata_root,l.root_generation,l.lease_epoch,deadline,'ACTIVE',issued);
  FOREACH kind IN ARRAY ARRAY['REQUEST','READER'] LOOP
    INSERT INTO mst2_metadata_root_anchor(anchor_id,anchor_kind,owner_key,root_page,root_generation,root_certificate_digest,
      snapshot_id,session_incarnation,lease_id,reader_operation_id,reader_issuance) VALUES(gen_random_uuid(),kind,op::text,
        l.metadata_root,l.root_generation,s.certificate_digest,sid,l.session_incarnation,lid,op,issued);
  END LOOP;
  RETURN QUERY SELECT op,issued,l.root_generation,s.certificate_digest::bytea;
END $$;

CREATE FUNCTION mst2_metadata_finish_reader(op uuid,issuance bigint) RETURNS void LANGUAGE plpgsql VOLATILE
SET search_path=$Q_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE r mst2_metadata_reader_operation%ROWTYPE;
BEGIN
  PERFORM $CORE_SCHEMA$.mst2_route_enter($CORE_LITERAL$);
  SELECT * INTO r FROM mst2_metadata_reader_operation operation WHERE operation.operation_id=op AND operation.reader_issuance=issuance;
  IF NOT FOUND THEN RETURN; END IF;
  IF r.state='ACTIVE' THEN UPDATE mst2_metadata_reader_operation SET state='FINISHED' WHERE operation_id=op AND reader_issuance=issuance; END IF;
  DELETE FROM mst2_metadata_root_anchor WHERE reader_operation_id=op AND reader_issuance=issuance AND anchor_kind IN ('REQUEST','READER');
  PERFORM mst2_metadata_cleanup_lease(r.lease_id);
END $$;
