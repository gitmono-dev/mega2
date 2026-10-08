use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};

async fn trigger_modes<C: ConnectionTrait>(db: &C) -> String {
    db.query_one_raw(Statement::from_string(
        DbBackend::Postgres,
        "SELECT coalesce(string_agg(c.relname||':'||t.tgname||':'||t.tgenabled::text,','
         ORDER BY c.relname,t.tgname),'') AS modes FROM pg_trigger t
         JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname=current_schema() AND NOT t.tgisinternal",
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get_by_index(0)
    .unwrap()
}

pub(super) async fn restore_pre_route_schema(db: &DatabaseConnection) {
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared("SELECT mst2_route_enter(current_schema())")
        .await
        .unwrap();
    let untouched = txn.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT coalesce(string_agg(c.relname||':'||t.tgname||':'||t.tgenabled::text,',' ORDER BY c.relname,t.tgname),'')
         FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid
         JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_proc p ON p.oid=t.tgfoid
         WHERE n.nspname=current_schema() AND NOT t.tgisinternal AND left(p.proname,11)<>'mst2_route_'",
    )).await.unwrap().unwrap().try_get_by_index::<String>(0).unwrap();
    // Remove only this additive layer in an isolated deployment-upgrade fixture.
    // The original contexts, leases, plans, payloads and protection stay intact.
    txn.execute_unprepared(
        "DO $$ DECLARE r record; functions text; BEGIN
           IF EXISTS(SELECT 1 FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1') THEN
             RAISE EXCEPTION 'route upgrade fixture cannot tear down a provisioned qualified family';
           END IF;
           IF EXISTS(SELECT 1 FROM mst2_rooted_source_tree_revision) THEN
             RAISE EXCEPTION 'route upgrade fixture cannot erase actual qualified source history';
           END IF;
           DROP FUNCTION mst2_metadata_has_generic_overlap(bytea);
           FOR r IN SELECT c.relname,t.tgname FROM pg_trigger t
             JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace
             JOIN pg_proc p ON p.oid=t.tgfoid
             WHERE n.nspname=current_schema() AND NOT t.tgisinternal AND left(p.proname,11)='mst2_route_'
           LOOP EXECUTE format('DROP TRIGGER %I ON %I.%I',r.tgname,current_schema(),r.relname); END LOOP;
           DROP TABLE mst2_lease_storage_route,mst2_generic_session_storage_binding,mst2_snapshot_storage_route,mst2_metadata_namespace,mst2_qualified_family_policy,mst2_rooted_source_tree_revision;
           FOR r IN SELECT conname FROM pg_constraint
             WHERE conrelid='mst2_snapshot_context'::regclass AND contype='u'
               AND pg_get_constraintdef(oid)='UNIQUE (snapshot_id, prepare_id, metadata_root)'
           LOOP EXECUTE format('ALTER TABLE mst2_snapshot_context DROP CONSTRAINT %I',r.conname); END LOOP;
           SELECT string_agg(format('%I.%I(%s)',n.nspname,p.proname,pg_get_function_identity_arguments(p.oid)),',')
             INTO functions FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
             WHERE n.nspname=current_schema() AND left(p.proname,11)='mst2_route_';
           IF functions IS NULL THEN RAISE EXCEPTION 'route upgrade fixture has no route functions'; END IF;
           EXECUTE 'DROP FUNCTION '||functions;
         END $$;
         DELETE FROM seaql_migrations WHERE version IN (
           'm20261007_000600_add_mst2_storage_routes','m20261008_000200_add_mst2_rooted_qualified_family')",
    )
    .await
    .unwrap();
    assert_eq!(trigger_modes(&txn).await, untouched);
    txn.commit().await.unwrap();
}

pub(super) async fn reject_half_lease_commit_for_test(
    db: &DatabaseConnection,
    lease_id: &str,
    insert_sql: &str,
) {
    let before = trigger_modes(db).await;
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared("SELECT mst2_route_enter(current_schema())")
        .await
        .unwrap();
    // Simulate an omitted derived write; all statement, identity and deferred
    // completeness guards remain active. Failed commit rolls back the trigger
    // change; pending deferred events forbid ALTER TABLE before commit.
    txn.execute_unprepared(
        "ALTER TABLE mst2_snapshot_lease DISABLE TRIGGER mst2_route_lease_insert",
    )
    .await
    .unwrap();
    assert_eq!(
        txn.execute_unprepared(insert_sql)
            .await
            .unwrap()
            .rows_affected(),
        1
    );
    let half = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT (SELECT count(*) FROM mst2_snapshot_lease WHERE lease_id=$1)::bigint AS actual,
         (SELECT count(*) FROM mst2_lease_storage_route WHERE lease_id=$1)::bigint AS routes",
            [lease_id.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(half.try_get::<i64>("", "actual").unwrap(), 1);
    assert_eq!(half.try_get::<i64>("", "routes").unwrap(), 0);
    let paused = before.replacen(
        "mst2_snapshot_lease:mst2_route_lease_insert:O",
        "mst2_snapshot_lease:mst2_route_lease_insert:D",
        1,
    );
    assert_ne!(paused, before);
    assert_eq!(trigger_modes(&txn).await, paused);
    let complete = txn
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT t.tgenabled::text AS mode,t.tgdeferrable AS deferrable,t.tginitdeferred AS deferred
         FROM pg_catalog.pg_trigger t WHERE t.tgrelid='mst2_snapshot_lease'::regclass
           AND t.tgname='mst2_route_complete' AND NOT t.tgisinternal",
        ))
        .await
        .unwrap()
        .unwrap();
    let mode: String = complete.try_get("", "mode").unwrap();
    assert!(matches!(mode.as_str(), "O" | "A"));
    assert!(before.contains(&format!("mst2_snapshot_lease:mst2_route_complete:{mode}")));
    assert!(complete.try_get::<bool>("", "deferrable").unwrap());
    assert!(complete.try_get::<bool>("", "deferred").unwrap());
    let rejected = txn.commit().await.unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("storage route lease committed without exact routing"),
        "{rejected}"
    );
    assert_eq!(trigger_modes(db).await, before);
}

pub(super) async fn overwrite_lease_route_incarnation_for_test(
    db: &DatabaseConnection,
    lease_id: &str,
) {
    let sql = "UPDATE mst2_lease_storage_route SET session_incarnation=$2::uuid WHERE lease_id=$1";
    let incarnation = uuid::Uuid::new_v4().to_string();
    let mutation = || {
        Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            [lease_id.into(), incarnation.clone().into()],
        )
    };
    assert!(
        db.execute_raw(mutation())
            .await
            .unwrap_err()
            .to_string()
            .contains("immutable")
    );
    let before = trigger_modes(db).await;
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared("SELECT mst2_route_enter(current_schema())")
        .await
        .unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_lease_storage_route DISABLE TRIGGER mst2_route_immutable",
    )
    .await
    .unwrap();
    assert_eq!(
        txn.execute_raw(mutation()).await.unwrap().rows_affected(),
        1
    );
    txn.execute_unprepared(
        "ALTER TABLE mst2_lease_storage_route ENABLE TRIGGER mst2_route_immutable",
    )
    .await
    .unwrap();
    assert_eq!(trigger_modes(&txn).await, before);
    txn.commit().await.unwrap();
    assert_eq!(trigger_modes(db).await, before);
    let forbidden = uuid::Uuid::new_v4().to_string();
    assert_ne!(forbidden, incarnation);
    assert!(
        db.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            sql,
            [lease_id.into(), forbidden.into()],
        ))
        .await
        .unwrap_err()
        .to_string()
        .contains("immutable")
    );
}
