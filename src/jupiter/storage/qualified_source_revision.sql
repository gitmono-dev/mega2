-- This records only trees actually attested by Q, not the global Git inventory.
CREATE TABLE mst2_rooted_source_tree_revision (
  tree_id text PRIMARY KEY CHECK(tree_id ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
  tree_row_id bigint NOT NULL,revision uuid NOT NULL,body_digest bytea NOT NULL CHECK(octet_length(body_digest)=32),
  valid boolean NOT NULL,
  CHECK(substr(revision::text,15,1)='4' AND substr(revision::text,20,1) IN ('8','9','a','b'))
);

CREATE FUNCTION mst2_route_source_tree_revision_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE source_id bigint; body bytea;
BEGIN
  IF TG_OP='DELETE' OR TG_OP='TRUNCATE' THEN RAISE EXCEPTION 'rooted source revision watermarks are immutable'; END IF;
  IF pg_is_in_recovery() OR current_setting('transaction_isolation')<>'read committed'
    OR TG_RELID<>'mst2_rooted_source_tree_revision'::regclass THEN
    RAISE EXCEPTION 'rooted source revision requires its captured primary relation'; END IF;
  IF TG_OP='UPDATE' THEN
    IF NEW.tree_id IS DISTINCT FROM OLD.tree_id THEN RAISE EXCEPTION 'rooted source revision cannot retarget its OID'; END IF;
    IF NEW.valid IS FALSE THEN
      -- A nested caller cannot invent invalidation: independently observe the
      -- actual core mutation after it happened in this still-uncommitted writer.
      IF pg_trigger_depth()<2 OR NOT OLD.valid OR NEW.tree_row_id IS DISTINCT FROM OLD.tree_row_id
        OR NEW.body_digest IS DISTINCT FROM OLD.body_digest OR NEW.revision IS DISTINCT FROM OLD.revision THEN
        RAISE EXCEPTION 'rooted source invalidation is outside its actual source writer'; END IF;
      PERFORM mst2_route_enter($CORE_LITERAL$);
      SELECT id,sub_trees INTO source_id,body FROM mega_tree WHERE tree_id=OLD.tree_id;
      IF FOUND AND source_id=OLD.tree_row_id AND octet_length(body)<=67108864
        AND sha256(body)=OLD.body_digest THEN
        RAISE EXCEPTION 'rooted source invalidation has no actual changed or deleted core bytes'; END IF;
      NEW.revision:=gen_random_uuid(); RETURN NEW;
    END IF;
    IF OLD.valid OR NEW.revision IS DISTINCT FROM OLD.revision OR NEW.valid IS DISTINCT FROM true THEN
      RAISE EXCEPTION 'ordinary DML cannot rewrite a valid rooted source revision'; END IF;
  END IF;
  PERFORM mst2_route_enter($CORE_LITERAL$);
  SELECT id,sub_trees INTO source_id,body FROM mega_tree WHERE tree_id=NEW.tree_id FOR SHARE;
  IF NOT FOUND OR octet_length(body)>67108864 THEN RAISE EXCEPTION 'rooted source capture has no bounded actual core tree'; END IF;
  IF NEW.body_digest IS DISTINCT FROM sha256(body) THEN
    RAISE EXCEPTION 'rooted source revision was not independently derived from its actual core bytes'; END IF;
  NEW.tree_row_id:=source_id; NEW.revision:=gen_random_uuid(); NEW.valid:=true;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_route_source_tree_revision_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_rooted_source_tree_revision
  FOR EACH ROW EXECUTE FUNCTION mst2_route_source_tree_revision_guard();
CREATE TRIGGER mst2_route_source_tree_revision_truncate BEFORE TRUNCATE ON mst2_rooted_source_tree_revision
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_route_source_tree_revision_guard();

CREATE FUNCTION mst2_route_source_tree_inventory_guard() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF (SELECT count(*) FROM (SELECT 1 FROM mst2_rooted_source_tree_revision LIMIT 65537) bounded)>65536 THEN
    RAISE EXCEPTION 'rooted attested source inventory capacity is exceeded'; END IF;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_route_source_tree_inventory_guard AFTER INSERT ON mst2_rooted_source_tree_revision
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_route_source_tree_inventory_guard();

CREATE FUNCTION mst2_route_source_tree_writer_enter() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_RELID<>'mega_tree'::regclass THEN RAISE EXCEPTION 'rooted source writer left its captured core relation'; END IF;
  IF TG_OP='TRUNCATE' THEN RAISE EXCEPTION 'core tree truncation cannot bypass rooted source revision invalidation'; END IF;
  PERFORM mst2_route_enter($CORE_LITERAL$);
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_route_source_tree_writer_enter BEFORE INSERT OR UPDATE OR DELETE OR TRUNCATE ON mega_tree
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_route_source_tree_writer_enter();

CREATE FUNCTION mst2_route_source_tree_invalidate() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_RELID<>'mega_tree'::regclass THEN RAISE EXCEPTION 'rooted invalidation left its captured core relation'; END IF;
  IF TG_OP='UPDATE' AND NEW.tree_id IS NOT DISTINCT FROM OLD.tree_id AND NEW.id IS NOT DISTINCT FROM OLD.id
    AND NEW.sub_trees IS NOT DISTINCT FROM OLD.sub_trees THEN RETURN NEW; END IF;
  UPDATE mst2_rooted_source_tree_revision SET valid=false WHERE tree_id=OLD.tree_id AND valid;
  IF TG_OP='DELETE' THEN RETURN OLD; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_route_source_tree_invalidate AFTER UPDATE OR DELETE ON mega_tree
  FOR EACH ROW EXECUTE FUNCTION mst2_route_source_tree_invalidate();
CREATE FUNCTION mst2_route_source_tree_inserted() RETURNS trigger LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
BEGIN
  IF TG_RELID<>'mega_tree'::regclass THEN RAISE EXCEPTION 'rooted insertion left its captured core relation'; END IF;
  UPDATE mst2_rooted_source_tree_revision SET valid=false WHERE tree_id=NEW.tree_id AND valid;
  RETURN NULL;
END $$;
CREATE TRIGGER mst2_route_source_tree_inserted AFTER INSERT ON mega_tree
  FOR EACH ROW EXECUTE FUNCTION mst2_route_source_tree_inserted();

CREATE FUNCTION mst2_route_capture_source_tree(oid_text text,expected_digest bytea) RETURNS uuid LANGUAGE plpgsql VOLATILE
SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE captured mst2_rooted_source_tree_revision%ROWTYPE; actual_id bigint;
BEGIN
  PERFORM mst2_route_enter($CORE_LITERAL$);
  SELECT id INTO actual_id FROM mega_tree WHERE tree_id=oid_text FOR SHARE;
  IF NOT FOUND OR expected_digest IS NULL OR octet_length(expected_digest)<>32 THEN
    RAISE EXCEPTION 'rooted source capture has no exact actual row and body digest'; END IF;
  SELECT * INTO captured FROM mst2_rooted_source_tree_revision WHERE tree_id=oid_text FOR UPDATE;
  IF FOUND THEN
    IF captured.valid THEN
      IF captured.tree_row_id<>actual_id OR captured.body_digest IS DISTINCT FROM expected_digest THEN
        RAISE EXCEPTION 'rooted source bytes changed behind their exact immutable revision'; END IF;
      RETURN captured.revision;
    END IF;
    UPDATE mst2_rooted_source_tree_revision SET valid=true,body_digest=expected_digest WHERE tree_id=oid_text
      RETURNING revision INTO captured.revision;
  ELSE
    INSERT INTO mst2_rooted_source_tree_revision(tree_id,tree_row_id,revision,body_digest,valid)
      VALUES(oid_text,actual_id,gen_random_uuid(),expected_digest,true) RETURNING revision INTO captured.revision;
  END IF;
  RETURN captured.revision;
END $$;

CREATE FUNCTION mst2_route_source_tree_matches(oid_text text,exact_revision uuid,exact_digest bytea)
RETURNS boolean LANGUAGE plpgsql VOLATILE STRICT SET search_path=$CORE_SCHEMA$,pg_catalog,pg_temp AS $$
DECLARE captured mst2_rooted_source_tree_revision%ROWTYPE;
BEGIN
  IF pg_is_in_recovery() OR current_setting('transaction_isolation')<>'read committed' THEN RETURN false; END IF;
  SELECT revision.* INTO captured FROM mst2_rooted_source_tree_revision revision
    JOIN mega_tree source ON source.id=revision.tree_row_id AND source.tree_id=revision.tree_id
    WHERE revision.tree_id=oid_text FOR SHARE OF revision NOWAIT;
  RETURN FOUND AND captured.valid AND captured.revision=exact_revision AND captured.body_digest=exact_digest;
END $$;
