WITH q AS (
  SELECT n.oid,n.nspname FROM pg_catalog.pg_namespace n WHERE n.oid=q_oid
), relations AS (
  SELECT c.* FROM pg_catalog.pg_class c WHERE c.relnamespace=q_oid
), objects AS (
  SELECT 'relation' AS kind,c.relname AS key,jsonb_build_array(c.relname,c.relkind,c.relpersistence,c.relowner,c.relacl,
    c.relrowsecurity,c.relforcerowsecurity,c.reloptions,c.relispartition,a.amname) AS value
    FROM relations c LEFT JOIN pg_catalog.pg_am a ON a.oid=c.relam
  UNION ALL SELECT 'column',c.relname||':'||a.attnum,jsonb_build_array(c.relname,a.attnum,a.attname,
    CASE WHEN tn.oid=q_oid THEN '$Q_SCHEMA$' ELSE tn.nspname END,t.typname,a.attnotnull,a.attisdropped,a.attidentity,a.attgenerated,a.attislocal,a.attinhcount,
    a.atttypmod,a.attndims,a.attacl,a.attlen,a.attbyval,a.attalign,a.attstorage,a.attcompression,
    CASE WHEN cn.oid=q_oid THEN '$Q_SCHEMA$' ELSE cn.nspname END,co.collname,
    pg_catalog.replace(pg_catalog.pg_get_expr(d.adbin,d.adrelid),(SELECT nspname FROM q),'$Q_SCHEMA$'))
    FROM pg_catalog.pg_attribute a JOIN relations c ON c.oid=a.attrelid
    JOIN pg_catalog.pg_type t ON t.oid=a.atttypid JOIN pg_catalog.pg_namespace tn ON tn.oid=t.typnamespace
    LEFT JOIN pg_catalog.pg_collation co ON co.oid=a.attcollation LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid=co.collnamespace
    LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attnum>0
  UNION ALL SELECT 'constraint',c.relname||':'||x.conname,jsonb_build_array(c.relname,x.conname,x.contype,
    pg_catalog.replace(pg_catalog.pg_get_constraintdef(x.oid,true),(SELECT nspname FROM q),'$Q_SCHEMA$'),
    x.condeferrable,x.condeferred,x.convalidated,x.conislocal,x.coninhcount,x.connoinherit)
    FROM pg_catalog.pg_constraint x JOIN relations c ON c.oid=x.conrelid
  UNION ALL SELECT 'index',c.relname||':'||ic.relname,jsonb_build_array(c.relname,ic.relname,
    pg_catalog.replace(pg_catalog.pg_get_indexdef(i.indexrelid),(SELECT nspname FROM q),'$Q_SCHEMA$'),
    i.indisunique,i.indisprimary,i.indisexclusion,i.indimmediate,i.indisvalid,i.indisready,i.indislive,
    i.indnullsnotdistinct,i.indisclustered,i.indisreplident)
    FROM pg_catalog.pg_index i JOIN relations c ON c.oid=i.indrelid JOIN relations ic ON ic.oid=i.indexrelid
  UNION ALL SELECT 'function',p.proname||':'||pg_catalog.replace(pg_catalog.pg_get_function_identity_arguments(p.oid),(SELECT nspname FROM q),'$Q_SCHEMA$'),jsonb_build_array(
    p.proname,pg_catalog.replace(pg_catalog.pg_get_function_identity_arguments(p.oid),(SELECT nspname FROM q),'$Q_SCHEMA$'),
    pg_catalog.replace(pg_catalog.pg_get_function_result(p.oid),(SELECT nspname FROM q),'$Q_SCHEMA$'),l.lanname,
    p.proowner,p.proacl,p.prokind,p.provolatile,p.proisstrict,p.prosecdef,p.proleakproof,p.proparallel,
    p.procost,p.prorows,p.probin,p.pronargdefaults,
    pg_catalog.replace(pg_catalog.pg_get_expr(p.proargdefaults,0),(SELECT nspname FROM q),'$Q_SCHEMA$'),
    pg_catalog.replace(pg_catalog.replace(pg_catalog.replace(pg_catalog.replace(p.prosrc,
      pg_catalog.quote_literal((SELECT nspname FROM q)),pg_catalog.quote_literal('$Q_SCHEMA$')),
      pg_catalog.quote_literal(q_oid::text),pg_catalog.quote_literal('$Q_OID$')),
      pg_catalog.quote_literal(n_uuid::text),pg_catalog.quote_literal('$NAMESPACE_UUID$')),
      pg_catalog.quote_literal(s_uuid),pg_catalog.quote_literal('$STORAGE_UUID$')),
    pg_catalog.replace(pg_catalog.array_to_string(p.proconfig,E'\n'),(SELECT nspname FROM q),'$Q_SCHEMA$'))
    FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_language l ON l.oid=p.prolang WHERE p.pronamespace=q_oid
  UNION ALL SELECT 'trigger',c.relname||':'||p.proname||':'||t.tgtype||':'||coalesce(x.conname,t.tgname),jsonb_build_array(
    CASE WHEN c.relnamespace=q_oid THEN '$Q_SCHEMA$' ELSE cns.nspname END,c.relname,
    CASE WHEN t.tgisinternal THEN NULL ELSE t.tgname END,t.tgtype,t.tgenabled,t.tgisinternal,
    CASE WHEN pn.oid=q_oid THEN '$Q_SCHEMA$' ELSE pn.nspname END,p.proname,
    pg_catalog.replace(pg_catalog.pg_get_function_identity_arguments(p.oid),(SELECT nspname FROM q),'$Q_SCHEMA$'),
    t.tgdeferrable,t.tginitdeferred,t.tgnargs,
    pg_catalog.replace(encode(t.tgargs,'hex'),encode(pg_catalog.convert_to((SELECT nspname FROM q),'UTF8'),'hex'),
      encode(pg_catalog.convert_to('$Q_SCHEMA$','UTF8'),'hex')),t.tgattr::text,
    pg_catalog.replace(pg_catalog.pg_get_expr(t.tgqual,t.tgrelid),(SELECT nspname FROM q),'$Q_SCHEMA$'),t.tgoldtable,t.tgnewtable,x.conname,
    CASE WHEN rn.oid=q_oid THEN '$Q_SCHEMA$' ELSE rn.nspname END,rc.relname,
    CASE WHEN t.tgparentid=0 THEN NULL ELSE 'unexpected parent trigger' END)
    FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid=t.tgrelid
    JOIN pg_catalog.pg_namespace cns ON cns.oid=c.relnamespace
    JOIN pg_catalog.pg_proc p ON p.oid=t.tgfoid JOIN pg_catalog.pg_namespace pn ON pn.oid=p.pronamespace
    LEFT JOIN pg_catalog.pg_constraint x ON x.oid=t.tgconstraint
    LEFT JOIN pg_catalog.pg_class rc ON rc.oid=t.tgconstrrelid LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid=rc.relnamespace
    WHERE c.relnamespace=q_oid OR EXISTS(SELECT 1 FROM pg_catalog.pg_constraint fk
      WHERE fk.oid=t.tgconstraint AND fk.conrelid IN (SELECT oid FROM relations))
  UNION ALL SELECT 'rule',c.relname||':'||r.rulename,pg_catalog.to_jsonb(r)
    FROM pg_catalog.pg_rewrite r JOIN relations c ON c.oid=r.ev_class
  UNION ALL SELECT 'policy',c.relname||':'||p.polname,pg_catalog.to_jsonb(p)
    FROM pg_catalog.pg_policy p JOIN relations c ON c.oid=p.polrelid
)
SELECT pg_catalog.sha256(pg_catalog.convert_to(
  pg_catalog.jsonb_agg(pg_catalog.jsonb_build_array(kind,key,value) ORDER BY kind,key,value::text)::text,'UTF8')) AS fingerprint
FROM objects
