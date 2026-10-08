use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};

pub(super) async fn restore_empty_history_schema(db: &DatabaseConnection) {
    let n:i64=db.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT (SELECT count(*) FROM mst2_metadata_current)+(SELECT count(*) FROM mst2_metadata_graph_node)
         +(SELECT count(*) FROM mst2_metadata_graph_root)+(SELECT count(*) FROM mst2_metadata_graph_edge)
         +(SELECT count(*) FROM mst2_metadata_gc_op) AS n"))
        .await.unwrap().unwrap().try_get("","n").unwrap();
    assert_eq!(
        n, 0,
        "upgrade fixture has no qualified or current incarnation to erase"
    );
    db.execute_unprepared(
        "DO $$ DECLARE t text; tr text; BEGIN
           FOREACH t IN ARRAY ARRAY['mst2_metadata_lifetime','mst2_metadata_current','mst2_metadata_payload',
             'mst2_metadata_prepare','mst2_metadata_prepare_page','mst2_retention_node','mst2_retention_edge',
             'mst2_retention_root','mst2_retention_gc_op','mst2_snapshot_context','mst2_snapshot_lease'] LOOP
             FOREACH tr IN ARRAY ARRAY['mst2_metadata_statement_barrier','mst2_metadata_generic_domain_guard',
               'mst2_metadata_session_domain_guard','mst2_metadata_qualified_mapping_guard',
               'mst2_metadata_lifetime_insert_guard','mst2_metadata_lifetime_removed',
               'mst2_metadata_current_insert_guard','mst2_metadata_current_protected',
               'mst2_metadata_payload_fenced','mst2_metadata_payload_removed'] LOOP
               EXECUTE format('DROP TRIGGER IF EXISTS %I ON %I',tr,t);
             END LOOP;
           END LOOP;
         END $$;
         DROP TABLE mst2_metadata_graph_root,mst2_metadata_graph_edge,mst2_metadata_graph_node,mst2_metadata_gc_op;
         ALTER TABLE mst2_metadata_lifetime DROP COLUMN graph_domain;
         ALTER TABLE mst2_metadata_prepare_page DROP CONSTRAINT mst2_metadata_prepare_page_prepare_id_page_id_generation_key;
         ALTER TABLE mst2_metadata_prepare DROP CONSTRAINT mst2_metadata_prepare_prepare_id_storage_seal_key;
         DROP INDEX idx_mst2_snapshot_context_metadata_root;
         DO $$ DECLARE function_row record; BEGIN
           FOR function_row IN SELECT proc.oid::regprocedure::text AS signature FROM pg_proc proc JOIN pg_namespace n ON n.oid=proc.pronamespace
             WHERE n.nspname=current_schema() AND proc.proname LIKE 'mst2_metadata_%'
               AND proc.proname NOT IN ('mst2_metadata_lifetime_guard','mst2_metadata_current_guard',
                 'mst2_metadata_generation_seal_guard','mst2_metadata_generation_mapping_guard','mst2_metadata_payload_immutable') LOOP
             EXECUTE 'DROP FUNCTION '||function_row.signature;
           END LOOP;
         END $$;
         CREATE OR REPLACE FUNCTION mst2_metadata_current_guard() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'metadata current watermark requires generation-fenced collection'; END $$;
         CREATE OR REPLACE FUNCTION mst2_metadata_lifetime_guard() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
           IF TG_OP='DELETE' THEN RAISE EXCEPTION 'metadata lifetime watermark cannot be deleted'; END IF;
           IF NEW.page_id IS DISTINCT FROM OLD.page_id OR NEW.node_id IS DISTINCT FROM OLD.node_id
             OR NEW.generation IS DISTINCT FROM OLD.generation OR NEW.metadata_codec IS DISTINCT FROM OLD.metadata_codec
             OR NEW.expected_size IS DISTINCT FROM OLD.expected_size THEN RAISE EXCEPTION 'metadata lifetime identity is immutable'; END IF;
           IF NEW.state IS DISTINCT FROM OLD.state AND NOT (OLD.state='RESERVED' AND NEW.state='LIVE') THEN
             RAISE EXCEPTION 'metadata lifetime transition requires generation collector'; END IF;
           RETURN NEW;
         END $$;
         CREATE TRIGGER mst2_metadata_payload_immutable BEFORE UPDATE OR DELETE ON mst2_metadata_payload
           FOR EACH ROW EXECUTE FUNCTION mst2_metadata_payload_immutable();
         DELETE FROM seaql_migrations WHERE version='m20261007_000400_add_mst2_qualified_metadata_gc'"
    ).await.unwrap();
}
