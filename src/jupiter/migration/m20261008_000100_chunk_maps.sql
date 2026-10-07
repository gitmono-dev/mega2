CREATE TABLE mst2_chunk_map (
  map_id bytea PRIMARY KEY CHECK (pg_catalog.octet_length(map_id)=32),
  descriptor bytea NOT NULL CHECK (pg_catalog.octet_length(descriptor)=100),
  page_count integer NOT NULL CHECK (page_count BETWEEN 1 AND 32768),
  pages_root bytea NOT NULL CHECK (pg_catalog.octet_length(pages_root)=32)
);
CREATE TABLE mst2_chunk_map_leaf (
  map_id bytea NOT NULL REFERENCES mst2_chunk_map(map_id),
  page_index integer NOT NULL CHECK (page_index BETWEEN 0 AND 32767),
  payload bytea NOT NULL CHECK (pg_catalog.octet_length(payload) BETWEEN 48 AND 8208),
  PRIMARY KEY(map_id,page_index)
);
CREATE TABLE mst2_chunk_map_node (
  map_id bytea NOT NULL REFERENCES mst2_chunk_map(map_id),
  first_page integer NOT NULL CHECK (first_page BETWEEN 0 AND 32767),
  page_count integer NOT NULL CHECK (page_count BETWEEN 1 AND 32768),
  digest bytea NOT NULL CHECK (pg_catalog.octet_length(digest)=32),
  PRIMARY KEY(map_id,first_page,page_count),
  CHECK (first_page+page_count<=32768)
);
CREATE TABLE mst2_chunk_map_source (
  storage_domain text NOT NULL CHECK (storage_domain='git'),
  git_oid text NOT NULL CHECK (git_oid ~ '^([0-9a-f]{40}|[0-9a-f]{64})$'),
  object_kind text NOT NULL CHECK (object_kind='blob'),
  fact_id bigint NOT NULL CHECK (fact_id>0),
  source_id bytea NOT NULL UNIQUE CHECK (pg_catalog.octet_length(source_id)=32),
  source_bytes bytea NOT NULL CHECK (pg_catalog.octet_length(source_bytes) BETWEEN 1 AND 2048),
  primary_scope bytea NOT NULL CHECK (pg_catalog.octet_length(primary_scope) BETWEEN 1 AND 1024),
  map_id bytea NOT NULL REFERENCES mst2_chunk_map(map_id),
  receipt_digest bytea NOT NULL CHECK (pg_catalog.octet_length(receipt_digest)=32),
  PRIMARY KEY(storage_domain,git_oid,object_kind)
);

-- These rows are indexes, not proof that any source body was consumed. The
-- trusted object writer alone publishes the independent immutable receipt.
CREATE FUNCTION mst2_chunk_map_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'chunk map indexes and source receipts are append-only'; END $$;

CREATE FUNCTION mst2_chunk_map_primary() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA
    OR pg_catalog.pg_is_in_recovery()
    OR pg_catalog.current_setting('transaction_isolation')<>'read committed' THEN
    RAISE EXCEPTION 'chunk map insertion requires its actual primary schema and READ COMMITTED';
  END IF;
  RETURN NULL;
END $$;

CREATE FUNCTION mst2_chunk_map_complete() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE m record; leaves bigint; nodes bigint; root bytea; first_leaf integer; last_leaf integer;
BEGIN
  EXECUTE pg_catalog.format('SELECT * FROM %I.mst2_chunk_map WHERE map_id=$1',TG_TABLE_SCHEMA)
    INTO m USING NEW.map_id;
  EXECUTE pg_catalog.format('SELECT pg_catalog.count(*),pg_catalog.min(page_index),pg_catalog.max(page_index) FROM %I.mst2_chunk_map_leaf WHERE map_id=$1',TG_TABLE_SCHEMA)
    INTO leaves,first_leaf,last_leaf USING NEW.map_id;
  EXECUTE pg_catalog.format('SELECT pg_catalog.count(*) FROM %I.mst2_chunk_map_node WHERE map_id=$1',TG_TABLE_SCHEMA)
    INTO nodes USING NEW.map_id;
  EXECUTE pg_catalog.format('SELECT digest FROM %I.mst2_chunk_map_node WHERE map_id=$1 AND first_page=0 AND page_count=$2',TG_TABLE_SCHEMA)
    INTO root USING NEW.map_id,m.page_count;
  IF m.map_id IS NULL OR leaves<>m.page_count OR first_leaf<>0 OR last_leaf<>m.page_count-1
    OR nodes<>2*m.page_count-1 OR root IS DISTINCT FROM m.pages_root
    OR pg_catalog.substring(m.descriptor,69,32) IS DISTINCT FROM m.pages_root
    OR pg_catalog.sha256(pg_catalog.convert_to('mega.mst2.chunkmap','UTF8')||pg_catalog.decode('00','hex')||m.descriptor) IS DISTINCT FROM m.map_id THEN
    RAISE EXCEPTION 'chunk map installation is incomplete or inconsistent';
  END IF;
  RETURN NULL;
END $$;

CREATE FUNCTION mst2_chunk_map_index_insert() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE admitted boolean;
BEGIN
  EXECUTE pg_catalog.format('SELECT EXISTS(SELECT 1 FROM %I.mst2_chunk_map_source WHERE map_id=$1)',TG_TABLE_SCHEMA)
    INTO admitted USING NEW.map_id;
  IF admitted THEN RAISE EXCEPTION 'admitted chunk map index cannot acquire more rows'; END IF;
  RETURN NEW;
END $$;

DO $$ DECLARE t text; BEGIN
  FOREACH t IN ARRAY ARRAY['mst2_chunk_map','mst2_chunk_map_leaf','mst2_chunk_map_node','mst2_chunk_map_source'] LOOP
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_chunk_map_primary BEFORE INSERT ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_chunk_map_primary()',t);
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_chunk_map_immutable BEFORE UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION mst2_chunk_map_immutable()',t);
    EXECUTE pg_catalog.format('CREATE TRIGGER mst2_chunk_map_no_truncate BEFORE TRUNCATE ON %I FOR EACH STATEMENT EXECUTE FUNCTION mst2_chunk_map_immutable()',t);
  END LOOP;
END $$;
CREATE CONSTRAINT TRIGGER mst2_chunk_map_complete AFTER INSERT ON mst2_chunk_map_source
  DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION mst2_chunk_map_complete();
CREATE TRIGGER mst2_chunk_map_index_insert BEFORE INSERT ON mst2_chunk_map_leaf
  FOR EACH ROW EXECUTE FUNCTION mst2_chunk_map_index_insert();
CREATE TRIGGER mst2_chunk_map_index_insert BEFORE INSERT ON mst2_chunk_map_node
  FOR EACH ROW EXECUTE FUNCTION mst2_chunk_map_index_insert();
