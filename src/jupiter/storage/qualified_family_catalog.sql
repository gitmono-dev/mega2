WITH selected_namespaces AS (
  SELECT oid,nspname,nspowner,nspacl FROM pg_catalog.pg_namespace
  WHERE oid IN ($CORE_OID$,$Q_OID$)
), selected_relations AS (
  SELECT c.oid,c.relnamespace,c.relname,c.relkind,c.relpersistence,c.relowner,c.relacl,
    c.relrowsecurity,c.relforcerowsecurity,c.reloptions,c.relam,c.relhasrules,c.relispartition
  FROM pg_catalog.pg_class c
  WHERE c.relnamespace=$Q_OID$
    OR (c.relnamespace=$CORE_OID$ AND (c.relname IN ('mst2_metadata_namespace','mst2_qualified_family_policy',
      'mega_tree','mst2_rooted_source_tree_revision','mst2_verified_object','mst2_retention_node','mst2_retention_gc_op',
      'mega_commit','mega_refs','mst2_native_head','mst2_native_publication','mst2_publication','mst2_publication_outbox',
      'mst2_snapshot_storage_route','mst2_lease_storage_route','mst2_generic_session_storage_binding',
      'mst2_snapshot_context','mst2_snapshot_lease')
      OR c.oid IN (SELECT i.indexrelid FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class parent ON parent.oid=i.indrelid
        WHERE parent.relnamespace=$CORE_OID$ AND parent.relname IN ('mst2_metadata_namespace','mst2_qualified_family_policy',
          'mega_tree','mst2_rooted_source_tree_revision','mst2_verified_object','mst2_retention_node','mst2_retention_gc_op',
      'mega_commit','mega_refs','mst2_native_head','mst2_native_publication','mst2_publication','mst2_publication_outbox',
      'mst2_snapshot_storage_route','mst2_lease_storage_route','mst2_generic_session_storage_binding',
      'mst2_snapshot_context','mst2_snapshot_lease'))))
), selected_triggers AS (
  SELECT t.* FROM pg_catalog.pg_trigger t
  WHERE (t.tgrelid IN (SELECT oid FROM selected_relations)
    OR EXISTS(SELECT 1 FROM pg_catalog.pg_constraint x
      WHERE x.oid=t.tgconstraint AND x.conrelid IN (SELECT oid FROM pg_catalog.pg_class WHERE relnamespace=$Q_OID$)))
    AND NOT ($Q_OID$=0 AND $EXEMPT_Q_OID$<>0 AND t.tgisinternal
      AND EXISTS(SELECT 1 FROM pg_catalog.pg_constraint fk
        JOIN pg_catalog.pg_class source ON source.oid=fk.conrelid
        JOIN pg_catalog.pg_class target ON target.oid=fk.confrelid
        WHERE fk.oid=t.tgconstraint AND fk.contype='f' AND fk.connamespace=$EXEMPT_Q_OID$
          AND source.relnamespace=$EXEMPT_Q_OID$ AND target.relnamespace=$CORE_OID$
          AND t.tgrelid=fk.confrelid AND t.tgconstrrelid=fk.conrelid))
), objects AS (
  SELECT 'namespace' AS kind,oid::text AS key,pg_catalog.to_jsonb(n) AS value FROM selected_namespaces n
  UNION ALL SELECT 'relation',oid::text,pg_catalog.to_jsonb(c) FROM selected_relations c
  UNION ALL SELECT 'column',a.attrelid||':'||a.attnum,pg_catalog.to_jsonb(a)
    FROM pg_catalog.pg_attribute a JOIN selected_relations c ON c.oid=a.attrelid WHERE a.attnum>0
  UNION ALL SELECT 'default',d.oid::text,pg_catalog.to_jsonb(d)
    FROM pg_catalog.pg_attrdef d JOIN selected_relations c ON c.oid=d.adrelid
  UNION ALL SELECT 'constraint',x.oid::text,pg_catalog.to_jsonb(x)
    FROM pg_catalog.pg_constraint x JOIN selected_relations c ON c.oid=x.conrelid
  UNION ALL SELECT 'index',i.indexrelid::text,pg_catalog.to_jsonb(i)
    FROM pg_catalog.pg_index i JOIN selected_relations c ON c.oid=i.indrelid
  UNION ALL SELECT 'trigger',t.oid::text,pg_catalog.to_jsonb(t)
    FROM selected_triggers t
  UNION ALL SELECT 'function',p.oid::text,pg_catalog.to_jsonb(p)
    FROM pg_catalog.pg_proc p WHERE p.pronamespace=$Q_OID$
      OR (p.pronamespace=$CORE_OID$ AND (pg_catalog.left(p.proname,11)='mst2_route_'
        OR p.proname='mst2_metadata_has_generic_overlap'))
      OR EXISTS(SELECT 1 FROM selected_triggers t WHERE t.tgfoid=p.oid)
  UNION ALL SELECT 'policy',p.oid::text,pg_catalog.to_jsonb(p)
    FROM pg_catalog.pg_policy p JOIN selected_relations c ON c.oid=p.polrelid
  UNION ALL SELECT 'rule',r.oid::text,pg_catalog.to_jsonb(r)
    FROM pg_catalog.pg_rewrite r JOIN selected_relations c ON c.oid=r.ev_class
)
SELECT pg_catalog.sha256(pg_catalog.convert_to(
  pg_catalog.jsonb_agg(pg_catalog.jsonb_build_array(kind,key,value) ORDER BY kind,key)::text,'UTF8')) AS fingerprint
FROM objects
