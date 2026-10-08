-- Physical receipt generations are never reused. APPLIED history remains
-- bounded and is rechecked: a timed-out backend create can complete late.
CREATE SEQUENCE mst2_chunk_generation_sequence START 2;
ALTER TABLE mst2_chunk_map_source ADD COLUMN receipt_generation bigint NOT NULL DEFAULT 1 CHECK(receipt_generation>0);
ALTER TABLE mst2_chunk_map_source ADD COLUMN receipt_key text GENERATED ALWAYS AS
  (CASE WHEN receipt_generation=1 THEN pg_catalog.encode(source_id,'hex')
    ELSE pg_catalog.encode(source_id,'hex')||'-'||pg_catalog.lpad(pg_catalog.to_hex(receipt_generation),16,'0') END) STORED;
CREATE UNIQUE INDEX mst2_chunk_map_source_receipt_key ON mst2_chunk_map_source(receipt_key);

CREATE TABLE mst2_chunk_receipt_generation (
  receipt_key text PRIMARY KEY CHECK(receipt_key ~ '^[0-9a-f]{64}(-[0-9a-f]{16})?$'),
  source_id bytea NOT NULL CHECK(pg_catalog.octet_length(source_id)=32),
  generation bigint NOT NULL CHECK(generation>0),
  source_bytes bytea NOT NULL CHECK(pg_catalog.octet_length(source_bytes) BETWEEN 1 AND 2048),
  primary_scope bytea NOT NULL CHECK(pg_catalog.octet_length(primary_scope) BETWEEN 1 AND 1024),
  map_id bytea CHECK(map_id IS NULL OR pg_catalog.octet_length(map_id)=32),
  map_generation bigint CHECK(map_generation IS NULL OR map_generation>0),
  receipt_bytes bytea CHECK(receipt_bytes IS NULL OR pg_catalog.octet_length(receipt_bytes) BETWEEN 129 AND 4096),
  create_completed boolean NOT NULL DEFAULT false,
  create_completed_at timestamptz,
  owner uuid,
  state text NOT NULL CHECK(state IN ('RESERVED','CREATING','LIVE','DELETING','APPLIED')),
  reserved_pages integer NOT NULL CHECK(reserved_pages BETWEEN 0 AND 32768),
  reserved_nodes integer NOT NULL CHECK(reserved_nodes BETWEEN 0 AND 65535),
  reserved_bytes bigint NOT NULL CHECK(reserved_bytes BETWEEN 0 AND 536870912),
  deadline timestamptz,
  created_at timestamptz NOT NULL DEFAULT pg_catalog.clock_timestamp(),
  last_progress timestamptz NOT NULL DEFAULT pg_catalog.clock_timestamp(),
  checked_at timestamptz NOT NULL DEFAULT '-infinity',
  UNIQUE(source_id,generation),
  CHECK(receipt_key=CASE WHEN generation=1 THEN pg_catalog.encode(source_id,'hex') ELSE pg_catalog.encode(source_id,'hex')||'-'||pg_catalog.lpad(pg_catalog.to_hex(generation),16,'0') END),
  CHECK(state NOT IN ('CREATING','LIVE') OR (map_id IS NOT NULL AND receipt_bytes IS NOT NULL)),
  CHECK(state NOT IN ('RESERVED','CREATING') OR (owner IS NOT NULL AND deadline IS NOT NULL))
);
CREATE INDEX mst2_chunk_receipt_generation_maintenance ON mst2_chunk_receipt_generation(state,checked_at,receipt_key);
CREATE INDEX mst2_chunk_receipt_generation_created ON mst2_chunk_receipt_generation(created_at) WHERE receipt_bytes IS NOT NULL;
CREATE INDEX mst2_chunk_receipt_generation_completed ON mst2_chunk_receipt_generation(create_completed_at) WHERE receipt_bytes IS NOT NULL;
CREATE INDEX mst2_chunk_receipt_generation_install ON mst2_chunk_receipt_generation(map_id,map_generation,deadline) WHERE state='CREATING';
CREATE TABLE mst2_chunk_map_lifetime (
  map_id bytea PRIMARY KEY REFERENCES mst2_chunk_map(map_id) DEFERRABLE INITIALLY DEFERRED,
  generation bigint NOT NULL CHECK(generation>0),
  state text NOT NULL CHECK(state IN ('LIVE','DELETING')),
  last_used timestamptz NOT NULL DEFAULT pg_catalog.clock_timestamp(),
  UNIQUE(map_id,generation)
);
CREATE TABLE mst2_chunk_reader (
  owner uuid PRIMARY KEY,
  receipt_key text NOT NULL REFERENCES mst2_chunk_receipt_generation(receipt_key),
  source_generation bigint NOT NULL CHECK(source_generation>0),
  map_id bytea NOT NULL CHECK(pg_catalog.octet_length(map_id)=32),
  map_generation bigint NOT NULL CHECK(map_generation>0),
  deadline timestamptz NOT NULL,
  last_progress timestamptz NOT NULL DEFAULT pg_catalog.clock_timestamp()
);
CREATE INDEX mst2_chunk_reader_receipt ON mst2_chunk_reader(receipt_key,source_generation,deadline);
CREATE INDEX mst2_chunk_reader_map ON mst2_chunk_reader(map_id,map_generation,deadline);
CREATE TABLE mst2_chunk_map_gc (
  map_id bytea NOT NULL CHECK(pg_catalog.octet_length(map_id)=32),
  generation bigint NOT NULL CHECK(generation>0),
  state text NOT NULL CHECK(state IN ('PENDING','APPLIED')),
  created_at timestamptz NOT NULL DEFAULT pg_catalog.clock_timestamp(),
  applied_at timestamptz,
  PRIMARY KEY(map_id,generation)
);

-- Bootstrap existing #77 data from actual bounded rows. SQL does not infer
-- backing receipt presence; repository admission separately inventories it.
DO $$ DECLARE maps bigint; sources bigint; leaves bigint; nodes bigint; bytes bigint; BEGIN
  SELECT pg_catalog.count(*) INTO maps FROM mst2_chunk_map;
  SELECT pg_catalog.count(*) INTO sources FROM mst2_chunk_map_source;
  SELECT pg_catalog.count(*) INTO leaves FROM mst2_chunk_map_leaf;
  SELECT pg_catalog.count(*) INTO nodes FROM mst2_chunk_map_node;
  SELECT COALESCE(pg_catalog.sum(pg_catalog.pg_total_relation_size(c.oid)),0) INTO bytes
    FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname=pg_catalog.current_schema() AND c.relkind='r'
      AND c.relname IN ('mst2_chunk_map','mst2_chunk_map_source','mst2_chunk_map_leaf','mst2_chunk_map_node');
  IF maps>4096 OR sources>16384 OR leaves>65536 OR nodes>131072 OR bytes>536870912 THEN
    RAISE EXCEPTION 'existing chunk-map indexes exceed bounded retention bootstrap';
  END IF;
END $$;

INSERT INTO mst2_chunk_map_lifetime(map_id,generation,state) SELECT map_id,1,'LIVE' FROM mst2_chunk_map;
INSERT INTO mst2_chunk_receipt_generation(receipt_key,source_id,generation,source_bytes,primary_scope,map_id,map_generation,receipt_bytes,create_completed,create_completed_at,state,reserved_pages,reserved_nodes,reserved_bytes)
  SELECT s.receipt_key,s.source_id,1,s.source_bytes,s.primary_scope,s.map_id,1,
    pg_catalog.convert_to('MST2-CHUNK-MAP-RECEIPT','UTF8')||pg_catalog.decode('00','hex')||
    pg_catalog.int4send(pg_catalog.octet_length(s.primary_scope))||s.primary_scope||
    pg_catalog.int4send(pg_catalog.octet_length(s.source_bytes))||s.source_bytes||m.descriptor,
    true,pg_catalog.clock_timestamp(),'LIVE',0,0,0 FROM mst2_chunk_map_source s JOIN mst2_chunk_map m ON m.map_id=s.map_id;

CREATE FUNCTION mst2_chunk_retention_barrier() RETURNS void LANGUAGE plpgsql AS $$
BEGIN
  IF pg_catalog.pg_is_in_recovery() OR pg_catalog.current_setting('transaction_isolation')<>'read committed' THEN
    RAISE EXCEPTION 'chunk retention requires primary READ COMMITTED';
  END IF;
  PERFORM pg_catalog.pg_advisory_xact_lock(1296717363,pg_catalog.hashtext(pg_catalog.current_schema()));
END $$;

CREATE FUNCTION mst2_chunk_retention_bytes() RETURNS bigint LANGUAGE sql AS $$
  SELECT COALESCE(pg_catalog.sum(pg_catalog.pg_total_relation_size(c.oid)),0)::bigint
    FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname=pg_catalog.current_schema() AND c.relkind='r'
      AND c.relname IN ('mst2_chunk_map','mst2_chunk_map_source','mst2_chunk_map_leaf','mst2_chunk_map_node',
        'mst2_chunk_receipt_generation','mst2_chunk_map_lifetime','mst2_chunk_reader','mst2_chunk_map_gc')
$$;
DO $$ BEGIN
  IF mst2_chunk_retention_bytes()>536870912 THEN RAISE EXCEPTION 'bounded chunk retention bootstrap exceeds actual durable byte quota'; END IF;
END $$;

CREATE OR REPLACE FUNCTION mst2_chunk_map_primary() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN
    RAISE EXCEPTION 'chunk resident insertion requires its actual primary schema';
  END IF;
  PERFORM mst2_chunk_retention_barrier();
  IF mst2_chunk_retention_bytes()+1048576>536870912 THEN
    RAISE EXCEPTION 'chunk resident insertion lacks actual durable byte headroom';
  END IF;
  RETURN NULL;
END $$;

-- Preserve the original immutable/source-completeness oracles. Only exact
-- terminal generations with no live owners may lose their resident indexes.
CREATE OR REPLACE FUNCTION mst2_chunk_map_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE permitted boolean:=false;
BEGIN
  IF TG_OP<>'DELETE' OR pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN
    RAISE EXCEPTION 'chunk map indexes and source receipts are immutable';
  END IF;
  PERFORM mst2_chunk_retention_barrier();
  IF TG_TABLE_NAME='mst2_chunk_map_source' THEN
    EXECUTE pg_catalog.format('SELECT EXISTS(SELECT 1 FROM %I.mst2_chunk_receipt_generation g WHERE g.receipt_key=$1 AND g.source_id=$2 AND g.generation=$3 AND g.map_id=$4 AND g.state=''APPLIED'') AND NOT EXISTS(SELECT 1 FROM %I.mst2_chunk_reader r WHERE r.receipt_key=$1 AND r.source_generation=$3 AND r.deadline>pg_catalog.clock_timestamp())',TG_TABLE_SCHEMA,TG_TABLE_SCHEMA)
      INTO permitted USING OLD.receipt_key,OLD.source_id,OLD.receipt_generation,OLD.map_id;
  ELSE
    EXECUTE pg_catalog.format('SELECT EXISTS(SELECT 1 FROM %I.mst2_chunk_map_lifetime l JOIN %I.mst2_chunk_map_gc g ON g.map_id=l.map_id AND g.generation=l.generation WHERE l.map_id=$1 AND l.state=''DELETING'' AND g.state=''PENDING'') AND NOT EXISTS(SELECT 1 FROM %I.mst2_chunk_map_source s WHERE s.map_id=$1) AND NOT EXISTS(SELECT 1 FROM %I.mst2_chunk_reader r JOIN %I.mst2_chunk_map_lifetime l ON l.map_id=r.map_id AND l.generation=r.map_generation WHERE r.map_id=$1 AND r.deadline>pg_catalog.clock_timestamp())',TG_TABLE_SCHEMA,TG_TABLE_SCHEMA,TG_TABLE_SCHEMA,TG_TABLE_SCHEMA,TG_TABLE_SCHEMA)
      INTO permitted USING OLD.map_id;
  END IF;
  IF NOT permitted THEN RAISE EXCEPTION 'chunk map deletion lacks an exact retired generation'; END IF;
  RETURN OLD;
END $$;

CREATE FUNCTION mst2_chunk_generation_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE owned boolean;
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN
    RAISE EXCEPTION 'chunk retention mutation requires its actual primary schema';
  END IF;
  PERFORM mst2_chunk_retention_barrier();
  IF TG_OP='INSERT' THEN
    EXECUTE pg_catalog.format('SELECT pg_catalog.count(*)>=131072 FROM %I.mst2_chunk_receipt_generation',TG_TABLE_SCHEMA) INTO owned;
    IF owned THEN RAISE EXCEPTION 'chunk receipt history quota exhausted'; END IF;
  END IF;
  IF TG_OP='DELETE' OR TG_OP='TRUNCATE' THEN
    RAISE EXCEPTION 'chunk receipt generations and collection history are retained';
  END IF;
  IF mst2_chunk_retention_bytes()+(CASE WHEN NEW.state IN ('DELETING','APPLIED') THEN 262144 ELSE 1048576 END)>536870912 THEN
    RAISE EXCEPTION 'chunk receipt mutation lacks actual durable byte headroom';
  END IF;
  IF TG_OP='UPDATE' AND (NEW.receipt_key IS DISTINCT FROM OLD.receipt_key OR NEW.source_id IS DISTINCT FROM OLD.source_id
    OR NEW.generation IS DISTINCT FROM OLD.generation OR NEW.source_bytes IS DISTINCT FROM OLD.source_bytes
    OR NEW.primary_scope IS DISTINCT FROM OLD.primary_scope OR NEW.created_at IS DISTINCT FROM OLD.created_at
    OR (OLD.receipt_bytes IS NOT NULL AND NEW.receipt_bytes IS DISTINCT FROM OLD.receipt_bytes)
    OR (OLD.map_id IS NOT NULL AND NEW.map_id IS DISTINCT FROM OLD.map_id)
    OR (OLD.map_generation IS NOT NULL AND NEW.map_generation IS DISTINCT FROM OLD.map_generation)
    OR (OLD.create_completed AND NOT NEW.create_completed)
    OR (OLD.create_completed_at IS NOT NULL AND NEW.create_completed_at IS DISTINCT FROM OLD.create_completed_at)
    OR (OLD.state='APPLIED' AND NEW.state<>'APPLIED')
    OR (OLD.state='DELETING' AND NEW.state NOT IN ('DELETING','APPLIED'))
    OR (OLD.state='LIVE' AND NEW.state NOT IN ('LIVE','DELETING'))
    OR (OLD.state='CREATING' AND NEW.state NOT IN ('CREATING','LIVE','DELETING'))
    OR (OLD.state='RESERVED' AND NEW.state NOT IN ('RESERVED','CREATING','DELETING','APPLIED'))) THEN
    RAISE EXCEPTION 'chunk receipt generation identity or transition changed';
  END IF;
  IF NEW.state='DELETING' AND (TG_OP='INSERT' OR OLD.state<>'DELETING') THEN
    EXECUTE pg_catalog.format('SELECT EXISTS(SELECT 1 FROM %I.mst2_chunk_reader WHERE receipt_key=$1 AND source_generation=$2 AND deadline>pg_catalog.clock_timestamp())',TG_TABLE_SCHEMA)
      INTO owned USING NEW.receipt_key,NEW.generation;
    IF owned THEN RAISE EXCEPTION 'receipt retirement cannot cross a live reader'; END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_chunk_generation_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_chunk_receipt_generation
  FOR EACH ROW EXECUTE FUNCTION mst2_chunk_generation_guard();
CREATE TRIGGER mst2_chunk_generation_no_truncate BEFORE TRUNCATE ON mst2_chunk_receipt_generation
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_chunk_generation_guard();

CREATE FUNCTION mst2_chunk_gc_history_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE full_history boolean;
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN RAISE EXCEPTION 'wrong chunk history schema'; END IF;
  PERFORM mst2_chunk_retention_barrier();
  IF TG_OP='INSERT' THEN
    EXECUTE pg_catalog.format('SELECT pg_catalog.count(*)>=131072 FROM %I.mst2_chunk_map_gc',TG_TABLE_SCHEMA) INTO full_history;
    IF full_history THEN RAISE EXCEPTION 'chunk collection history quota exhausted'; END IF;
  END IF;
  IF TG_OP='DELETE' OR TG_OP='TRUNCATE' THEN RAISE EXCEPTION 'chunk collection history is retained'; END IF;
  IF mst2_chunk_retention_bytes()+262144>536870912 THEN RAISE EXCEPTION 'chunk collection history lacks actual durable byte headroom'; END IF;
  IF TG_OP='UPDATE' AND (NEW.map_id IS DISTINCT FROM OLD.map_id OR NEW.generation IS DISTINCT FROM OLD.generation
    OR NEW.created_at IS DISTINCT FROM OLD.created_at OR OLD.state='APPLIED' OR NEW.state<>'APPLIED') THEN
    RAISE EXCEPTION 'chunk collection history is immutable except terminal completion';
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_chunk_gc_history_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_chunk_map_gc
  FOR EACH ROW EXECUTE FUNCTION mst2_chunk_gc_history_guard();
CREATE TRIGGER mst2_chunk_gc_no_truncate BEFORE TRUNCATE ON mst2_chunk_map_gc
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_chunk_gc_history_guard();

CREATE FUNCTION mst2_chunk_reader_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE live boolean;
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN RAISE EXCEPTION 'wrong chunk reader schema'; END IF;
  PERFORM mst2_chunk_retention_barrier();
  IF TG_OP='TRUNCATE' THEN RAISE EXCEPTION 'chunk readers cannot be truncated'; END IF;
  IF TG_OP='DELETE' THEN RETURN OLD; END IF;
  IF mst2_chunk_retention_bytes()+1048576>536870912 THEN RAISE EXCEPTION 'chunk reader mutation lacks actual durable byte headroom'; END IF;
  IF TG_OP='INSERT' THEN
    EXECUTE pg_catalog.format('SELECT pg_catalog.count(*)>=4096 FROM %I.mst2_chunk_reader',TG_TABLE_SCHEMA) INTO live;
    IF live THEN RAISE EXCEPTION 'chunk reader quota exhausted'; END IF;
  END IF;
  IF NEW.deadline>pg_catalog.clock_timestamp()+interval '60 seconds' OR NEW.deadline<=pg_catalog.clock_timestamp()
    OR (TG_OP='UPDATE' AND (OLD.deadline<=pg_catalog.clock_timestamp() OR NEW.owner IS DISTINCT FROM OLD.owner
      OR NEW.receipt_key IS DISTINCT FROM OLD.receipt_key OR NEW.source_generation IS DISTINCT FROM OLD.source_generation
      OR NEW.map_id IS DISTINCT FROM OLD.map_id OR NEW.map_generation IS DISTINCT FROM OLD.map_generation)) THEN
    RAISE EXCEPTION 'chunk reader cannot renew an expired or different owner';
  END IF;
  EXECUTE pg_catalog.format('SELECT EXISTS(SELECT 1 FROM %I.mst2_chunk_receipt_generation g JOIN %I.mst2_chunk_map_lifetime l ON l.map_id=g.map_id AND l.generation=g.map_generation JOIN %I.mst2_chunk_map_source s ON s.receipt_key=g.receipt_key AND s.receipt_generation=g.generation WHERE g.receipt_key=$1 AND g.generation=$2 AND g.map_id=$3 AND g.map_generation=$4 AND g.state=''LIVE'' AND l.state=''LIVE'')',TG_TABLE_SCHEMA,TG_TABLE_SCHEMA,TG_TABLE_SCHEMA)
    INTO live USING NEW.receipt_key,NEW.source_generation,NEW.map_id,NEW.map_generation;
  IF NOT live THEN RAISE EXCEPTION 'chunk reader lacks exact live source and map generations'; END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_chunk_reader_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_chunk_reader
  FOR EACH ROW EXECUTE FUNCTION mst2_chunk_reader_guard();
CREATE TRIGGER mst2_chunk_reader_no_truncate BEFORE TRUNCATE ON mst2_chunk_reader
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_chunk_reader_guard();

CREATE FUNCTION mst2_chunk_map_lifetime_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE permitted boolean;
BEGIN
  IF pg_catalog.current_schema() IS DISTINCT FROM TG_TABLE_SCHEMA THEN RAISE EXCEPTION 'wrong chunk map lifetime schema'; END IF;
  PERFORM mst2_chunk_retention_barrier();
  IF TG_OP='TRUNCATE' THEN RAISE EXCEPTION 'chunk map lifetimes cannot be truncated'; END IF;
  IF TG_OP='DELETE' THEN
    EXECUTE pg_catalog.format('SELECT $2=''DELETING'' AND EXISTS(SELECT 1 FROM %I.mst2_chunk_map_gc g WHERE g.map_id=$1 AND g.generation=$3 AND g.state=''PENDING'') AND NOT EXISTS(SELECT 1 FROM %I.mst2_chunk_map_source s WHERE s.map_id=$1) AND NOT EXISTS(SELECT 1 FROM %I.mst2_chunk_reader r WHERE r.map_id=$1 AND r.map_generation=$3 AND r.deadline>pg_catalog.clock_timestamp())',TG_TABLE_SCHEMA,TG_TABLE_SCHEMA,TG_TABLE_SCHEMA)
      INTO permitted USING OLD.map_id,OLD.state,OLD.generation;
    IF NOT permitted THEN RAISE EXCEPTION 'map lifetime deletion lacks exact reader-free collection claim'; END IF;
    RETURN OLD;
  END IF;
  IF mst2_chunk_retention_bytes()+(CASE WHEN NEW.state='DELETING' THEN 262144 ELSE 1048576 END)>536870912 THEN
    RAISE EXCEPTION 'chunk map lifetime mutation lacks actual durable byte headroom';
  END IF;
  IF TG_OP='UPDATE' AND (NEW.map_id IS DISTINCT FROM OLD.map_id OR NEW.generation IS DISTINCT FROM OLD.generation
    OR (OLD.state='DELETING' AND NEW.state<>'DELETING')) THEN RAISE EXCEPTION 'map lifetime cannot reincarnate in place'; END IF;
  IF TG_OP='UPDATE' AND NEW.state='DELETING' AND OLD.state='LIVE' THEN
    EXECUTE pg_catalog.format('SELECT EXISTS(SELECT 1 FROM %I.mst2_chunk_map_gc g WHERE g.map_id=$1 AND g.generation=$2 AND g.state=''PENDING'') AND NOT EXISTS(SELECT 1 FROM %I.mst2_chunk_map_source WHERE map_id=$1) AND NOT EXISTS(SELECT 1 FROM %I.mst2_chunk_reader WHERE map_id=$1 AND map_generation=$2 AND deadline>pg_catalog.clock_timestamp()) AND NOT EXISTS(SELECT 1 FROM %I.mst2_chunk_receipt_generation WHERE map_id=$1 AND map_generation=$2 AND state=''CREATING'' AND deadline>pg_catalog.clock_timestamp())',TG_TABLE_SCHEMA,TG_TABLE_SCHEMA,TG_TABLE_SCHEMA,TG_TABLE_SCHEMA)
      INTO permitted USING OLD.map_id,OLD.generation;
    IF NOT permitted THEN RAISE EXCEPTION 'map collection cannot cross a reader or active partial installation'; END IF;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER mst2_chunk_map_lifetime_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_chunk_map_lifetime
  FOR EACH ROW EXECUTE FUNCTION mst2_chunk_map_lifetime_guard();
CREATE TRIGGER mst2_chunk_map_lifetime_no_truncate BEFORE TRUNCATE ON mst2_chunk_map_lifetime
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_chunk_map_lifetime_guard();
