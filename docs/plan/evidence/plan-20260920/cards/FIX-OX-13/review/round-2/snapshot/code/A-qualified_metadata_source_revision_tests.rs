use super::{canonical_tests::seeded_rooted_plan, *};
use crate::ceres::snapshot::rooted_metadata_projection::RootedReuseLookup;

async fn revision(core: &DatabaseConnection, oid: &str) -> String {
    core.query_one_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT revision::text FROM mst2_rooted_source_tree_revision WHERE tree_id=$1",
        [oid.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get_by_index(0)
    .unwrap()
}

#[tokio::test]
async fn source_revision_rejects_forgery_preserves_unchanged_writes_and_requires_fresh_attestation()
{
    let (config, core, namespace, q, _guard) = fixture().await;
    let (plan, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let intent = writer
        .begin_intent("source-revision-first", &plan)
        .await
        .unwrap();
    writer
        .install_pages(&intent, std::slice::from_ref(&payload))
        .await
        .unwrap();
    writer.finalize(&intent).await.unwrap();
    let oid = "a".repeat(40);
    let original = revision(&core, &oid).await;
    for statement in [
        "UPDATE mst2_rooted_source_tree_revision SET revision=gen_random_uuid()",
        "UPDATE mst2_rooted_source_tree_revision SET body_digest=decode(repeat('11',32),'hex')",
        "UPDATE mst2_rooted_source_tree_revision SET valid=false",
        "DELETE FROM mst2_rooted_source_tree_revision",
        "TRUNCATE mst2_rooted_source_tree_revision",
    ] {
        assert!(
            core.execute_unprepared(statement).await.is_err(),
            "{statement}"
        );
    }
    assert_eq!(revision(&core, &oid).await, original);
    core.execute_unprepared(
        "CREATE TABLE source_revision_spoof(id integer);
        CREATE FUNCTION source_revision_spoof_trigger() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN UPDATE mst2_rooted_source_tree_revision SET valid=false; RETURN NEW; END $$;
        CREATE TRIGGER source_revision_spoof AFTER INSERT ON source_revision_spoof
          FOR EACH ROW EXECUTE FUNCTION source_revision_spoof_trigger()",
    )
    .await
    .unwrap();
    assert!(
        core.execute_unprepared("INSERT INTO source_revision_spoof VALUES(1)")
            .await
            .is_err()
    );
    assert_eq!(revision(&core, &oid).await, original);
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mega_tree SET sub_trees=sub_trees,pack_offset=pack_offset WHERE tree_id=$1",
        [oid.clone().into()],
    ))
    .await
    .unwrap();
    assert_eq!(revision(&core, &oid).await, original);
    assert!(
        writer
            .lookup_reuse(&plan.identity.tagged_root_tree_oid, &plan.identity)
            .await
            .unwrap()
            .is_some()
    );
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mega_tree SET sub_trees=set_byte(sub_trees,7,120) WHERE tree_id=$1",
        [oid.clone().into()],
    ))
    .await
    .unwrap();
    assert_ne!(revision(&core, &oid).await, original);
    assert_eq!(
        count(
            &core,
            "SELECT count(*) FROM mst2_rooted_source_tree_revision WHERE valid"
        )
        .await,
        0
    );
    assert!(
        writer
            .lookup_reuse(&plan.identity.tagged_root_tree_oid, &plan.identity)
            .await
            .unwrap()
            .is_none()
    );
    let rejected=core.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT mst2_route_capture_source_tree($1,(SELECT source_body_digest FROM {q}.mst2_metadata_source_root_attestation LIMIT 1))"
            .replace("{q}",&identifier(&namespace.schema)),[oid.clone().into()])).await;
    assert!(rejected.is_err());
    // Restoring the same old bytes does not silently resurrect the old revision.
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mega_tree SET sub_trees=set_byte(sub_trees,7,102) WHERE tree_id=$1",
        [oid.clone().into()],
    ))
    .await
    .unwrap();
    assert_eq!(
        count(
            &core,
            "SELECT count(*) FROM mst2_rooted_source_tree_revision WHERE valid"
        )
        .await,
        0
    );
    let fresh = writer
        .begin_intent("source-revision-reattest", &plan)
        .await
        .unwrap();
    writer.install_pages(&fresh, &[payload]).await.unwrap();
    writer.finalize(&fresh).await.unwrap();
    assert_eq!(
        count(
            &core,
            "SELECT count(*) FROM mst2_rooted_source_tree_revision WHERE valid"
        )
        .await,
        1
    );
    assert_ne!(revision(&core, &oid).await, original);
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_source_root_attestation"
        )
        .await,
        2
    );
    assert_eq!(count(&q,&format!("SELECT count(*) FROM mst2_metadata_source_root_attestation a WHERE NOT {}.mst2_route_source_tree_matches(
        split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest)",identifier(&namespace.core_schema))).await,1);
}

#[tokio::test]
async fn deleted_and_reinserted_source_oid_requires_new_independent_revision() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let (plan, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let intent = writer
        .begin_intent("source-delete-original", &plan)
        .await
        .unwrap();
    writer
        .install_pages(&intent, std::slice::from_ref(&payload))
        .await
        .unwrap();
    writer.finalize(&intent).await.unwrap();
    let oid = "a".repeat(40);
    let original = revision(&core, &oid).await;
    let saved: String = core
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT row_to_json(t)::text FROM mega_tree t WHERE tree_id=$1",
            [oid.clone().into()],
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM mega_tree WHERE tree_id=$1",
        [oid.clone().into()],
    ))
    .await
    .unwrap();
    let deleted = revision(&core, &oid).await;
    assert_ne!(deleted, original);
    assert_eq!(
        count(
            &core,
            "SELECT count(*) FROM mst2_rooted_source_tree_revision WHERE valid"
        )
        .await,
        0
    );
    assert!(
        writer
            .lookup_reuse(&plan.identity.tagged_root_tree_oid, &plan.identity)
            .await
            .unwrap()
            .is_none()
    );
    // Even an identical OID, row ID, and byte body cannot revive the old stamp.
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO mega_tree SELECT (json_populate_record(NULL::mega_tree,$1::json)).*",
        [saved.into()],
    ))
    .await
    .unwrap();
    assert_eq!(revision(&core, &oid).await, deleted);
    assert_eq!(
        count(
            &core,
            "SELECT count(*) FROM mst2_rooted_source_tree_revision WHERE valid"
        )
        .await,
        0
    );
    assert!(
        writer
            .lookup_reuse(&plan.identity.tagged_root_tree_oid, &plan.identity)
            .await
            .unwrap()
            .is_none()
    );
    let fresh = writer
        .begin_intent("source-delete-reattest", &plan)
        .await
        .unwrap();
    writer.install_pages(&fresh, &[payload]).await.unwrap();
    writer.finalize(&fresh).await.unwrap();
    assert_ne!(revision(&core, &oid).await, deleted);
    assert_eq!(
        count(
            &core,
            "SELECT count(*) FROM mst2_rooted_source_tree_revision WHERE valid"
        )
        .await,
        1
    );
    assert_eq!(count(&q,&format!("SELECT count(*) FROM mst2_metadata_source_root_attestation a WHERE NOT {}.mst2_route_source_tree_matches(
        split_part(a.tagged_tree_oid,':',2),a.source_revision,a.source_body_digest)",identifier(&namespace.core_schema))).await,1);
}

#[tokio::test]
async fn current_source_revision_uses_captured_core_relations_under_temp_shadow() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let (plan, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let intent = writer
        .begin_intent("source-temp-shadow", &plan)
        .await
        .unwrap();
    writer.install_pages(&intent, &[payload]).await.unwrap();
    writer.finalize(&intent).await.unwrap();
    q.execute_unprepared("CREATE TEMP TABLE mega_tree(id bigint,tree_id text,sub_trees bytea);
        CREATE TEMP TABLE mst2_rooted_source_tree_revision(tree_id text,tree_row_id bigint,revision uuid,body_digest bytea,valid boolean)")
        .await.unwrap();
    assert_eq!(count(&q,&format!("SELECT {}.mst2_route_source_tree_matches(split_part(tagged_tree_oid,':',2),source_revision,source_body_digest)::bigint
        FROM mst2_metadata_source_root_attestation",identifier(&namespace.core_schema))).await,1);
}

#[tokio::test]
async fn current_source_revision_share_fence_orders_real_source_update_after_read() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let (plan, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let intent = writer
        .begin_intent("source-share-fence", &plan)
        .await
        .unwrap();
    writer.install_pages(&intent, &[payload]).await.unwrap();
    writer.finalize(&intent).await.unwrap();
    let read = q.begin().await.unwrap();
    assert_eq!(count(&read,&format!("SELECT {}.mst2_route_source_tree_matches(split_part(tagged_tree_oid,':',2),source_revision,source_body_digest)::bigint
        FROM mst2_metadata_source_root_attestation",identifier(&namespace.core_schema))).await,1);
    let core_writer = core.clone();
    let (ready, received) = tokio::sync::oneshot::channel();
    let pending = tokio::spawn(async move {
        let txn = core_writer.begin().await.unwrap();
        let pid: i32 = txn
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT pg_backend_pid()",
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get_by_index(0)
            .unwrap();
        ready.send(pid).unwrap();
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mega_tree SET sub_trees=set_byte(sub_trees,7,120) WHERE tree_id=$1",
            ["a".repeat(40).into()],
        ))
        .await
        .unwrap();
        txn.commit().await
    });
    let pid = received.await.unwrap();
    tokio::time::timeout(Duration::from_secs(4),async {
        loop {
            let waiting:bool=core.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
                "SELECT coalesce(wait_event_type='Lock',false) FROM pg_stat_activity WHERE pid=$1",[pid.into()]))
                .await.unwrap().unwrap().try_get_by_index(0).unwrap();
            if waiting {break;} tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert!(!pending.is_finished());
    read.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(4), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        count(
            &core,
            "SELECT count(*) FROM mst2_rooted_source_tree_revision WHERE valid"
        )
        .await,
        0
    );
}
