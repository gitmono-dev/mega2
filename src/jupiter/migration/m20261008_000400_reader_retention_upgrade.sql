SET LOCAL search_path=$Q_SCHEMA$,pg_catalog,pg_temp;
ALTER TABLE mst2_metadata_reader_operation DISABLE TRIGGER mst2_00_family_barrier;
ALTER TABLE mst2_metadata_reader_operation DISABLE TRIGGER mst2_metadata_reader_guard;
ALTER TABLE mst2_metadata_reader_operation DISABLE TRIGGER mst2_metadata_reader_complete;
ALTER TABLE mst2_metadata_root_anchor DISABLE TRIGGER mst2_00_family_barrier;
ALTER TABLE mst2_metadata_root_anchor DISABLE TRIGGER mst2_metadata_root_anchor_guard;

ALTER TABLE mst2_metadata_reader_operation
  ADD COLUMN reader_issuance bigint NOT NULL DEFAULT 0 CHECK(reader_issuance>=0),
  ADD COLUMN terminal_xid bigint;
UPDATE mst2_metadata_reader_operation SET terminal_xid=0 WHERE state IN ('FINISHED','EXPIRED');
ALTER TABLE mst2_metadata_reader_operation ALTER COLUMN reader_issuance DROP DEFAULT;
ALTER TABLE mst2_metadata_reader_operation
  ADD CONSTRAINT mst2_metadata_reader_identity UNIQUE(operation_id,reader_issuance),
  ADD CONSTRAINT mst2_metadata_reader_terminal_state CHECK((state='ACTIVE')=(terminal_xid IS NULL));
CREATE TABLE mst2_metadata_reader_issuance (
  singleton smallint PRIMARY KEY CHECK(singleton=1),high_water bigint NOT NULL CHECK(high_water>=0)
);
INSERT INTO mst2_metadata_reader_issuance VALUES(1,0);
CREATE INDEX mst2_metadata_reader_terminal ON mst2_metadata_reader_operation(terminal_xid,operation_id) WHERE state IN ('FINISHED','EXPIRED');
ALTER TABLE mst2_metadata_root_anchor ADD COLUMN reader_issuance bigint;
UPDATE mst2_metadata_root_anchor SET reader_issuance=0 WHERE reader_operation_id IS NOT NULL;
ALTER TABLE mst2_metadata_root_anchor
  DROP CONSTRAINT mst2_metadata_root_anchor_reader_operation_id_fkey,
  ADD CONSTRAINT mst2_metadata_root_anchor_reader_identity FOREIGN KEY(reader_operation_id,reader_issuance)
    REFERENCES mst2_metadata_reader_operation(operation_id,reader_issuance) MATCH FULL,
  ADD CONSTRAINT mst2_metadata_root_anchor_reader_kind CHECK((anchor_kind IN ('REQUEST','READER'))=(reader_operation_id IS NOT NULL)),
  ADD CONSTRAINT mst2_metadata_root_anchor_reader_fields CHECK((reader_operation_id IS NULL)=(reader_issuance IS NULL));
CREATE INDEX mst2_metadata_root_anchor_reader ON mst2_metadata_root_anchor(reader_operation_id,reader_issuance) WHERE reader_operation_id IS NOT NULL;
DROP FUNCTION mst2_metadata_begin_reader(text,text,text);
DROP FUNCTION mst2_metadata_finish_reader(uuid);
DROP FUNCTION mst2_metadata_read_source_entries(uuid,uuid,bytea,bigint,bytea,jsonb);

$READER_FUNCTIONS_SQL$

CREATE TRIGGER mst2_00_family_barrier BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_reader_issuance
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_dml_barrier();
CREATE TRIGGER mst2_metadata_truncate_guard BEFORE TRUNCATE ON mst2_metadata_reader_issuance
  FOR EACH STATEMENT EXECUTE FUNCTION mst2_metadata_immutable();
CREATE TRIGGER mst2_metadata_reader_issuance_guard BEFORE INSERT OR UPDATE OR DELETE ON mst2_metadata_reader_issuance
  FOR EACH ROW EXECUTE FUNCTION mst2_metadata_reader_issuance_guard();
ALTER TABLE mst2_metadata_reader_operation ENABLE TRIGGER mst2_00_family_barrier;
ALTER TABLE mst2_metadata_reader_operation ENABLE TRIGGER mst2_metadata_reader_guard;
ALTER TABLE mst2_metadata_reader_operation ENABLE TRIGGER mst2_metadata_reader_complete;
ALTER TABLE mst2_metadata_root_anchor ENABLE TRIGGER mst2_00_family_barrier;
ALTER TABLE mst2_metadata_root_anchor ENABLE TRIGGER mst2_metadata_root_anchor_guard;
