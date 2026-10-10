use std::collections::{BTreeMap, BTreeSet};

use super::{canonical_tests::seeded_rooted_plan, *};
use crate::ceres::snapshot::{
    rooted_metadata_install::RootedMetadataInstallPlan,
    rooted_metadata_projection::RootedReuseLookup,
};

async fn write_rooted(
    writer: &RootedQualifiedMetadataRepository,
    operation: &str,
    plan: &RootedMetadataInstallPlan,
    payload: MetadataPagePayload,
) -> RootedPrepareIntent {
    let intent = writer.begin_intent(operation, plan).await.unwrap();
    writer.install_pages(&intent, &[payload]).await.unwrap();
    writer.finalize(&intent).await.unwrap();
    intent
}

// Fault injection only: expire database evidence in the isolated test schema,
// restore the original guards/catalog, then use normal production maintenance.
async fn age_orphan(q: &DatabaseConnection, namespace: &VerifiedQualifiedNamespace, prepare: &str) {
    let txn = q.begin().await.unwrap();
    for trigger in [
        "mst2_00_family_barrier",
        "mst2_metadata_prepare_guard",
        "mst2_metadata_prepare_00_expiry_guard",
    ] {
        txn.execute_unprepared(&format!(
            "ALTER TABLE mst2_metadata_prepare DISABLE TRIGGER {trigger}"
        ))
        .await
        .unwrap();
    }
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mst2_metadata_prepare SET created_at=clock_timestamp()-interval '3601 seconds',
         orphan_expires_at=clock_timestamp()-interval '1 second' WHERE prepare_id=$1",
        [prepare.into()],
    ))
    .await
    .unwrap();
    // clock_timestamp() calls differ; preserve the exact database TTL relation.
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE mst2_metadata_prepare SET orphan_expires_at=created_at+interval '3600 seconds' WHERE prepare_id=$1",
        [prepare.into()])).await.unwrap();
    for trigger in [
        "mst2_00_family_barrier",
        "mst2_metadata_prepare_guard",
        "mst2_metadata_prepare_00_expiry_guard",
    ] {
        txn.execute_unprepared(&format!(
            "ALTER TABLE mst2_metadata_prepare ENABLE TRIGGER {trigger}"
        ))
        .await
        .unwrap();
    }
    txn.commit().await.unwrap();
    assert_eq!(
        catalog(q, namespace.core_oid, namespace.schema_oid)
            .await
            .unwrap(),
        namespace.catalog_fingerprint
    );
}

async fn claim(
    q: &DatabaseConnection,
    intent: &RootedPrepareIntent,
) -> Result<String, sea_orm::DbErr> {
    let operation = uuid::Uuid::new_v4().to_string();
    q.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT mst2_metadata_gc_claim($1,$2,(SELECT primary_scope FROM mst2_metadata_prepare WHERE prepare_id=$3),$4::uuid)",
        [intent.metadata_root().to_vec().into(),intent.root_generation().into(),intent.prepare_id().into(),operation.clone().into()]
    )).await?;
    Ok(operation)
}

#[tokio::test]
async fn admitted_orphan_gc_preserves_replay_identity_and_advances_exact_generation() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let (plan, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let first = write_rooted(&writer, "gc-generation-one", &plan, payload.clone()).await;
    assert_eq!(
        count(&q, "SELECT mst2_metadata_gc_enabled()::bigint").await,
        1
    );
    assert!(
        claim(&q, &first)
            .await
            .unwrap_err()
            .to_string()
            .contains("owned coverage")
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_gc_op").await,
        0
    );
    age_orphan(&q, &namespace, first.prepare_id()).await;
    let work = writer.maintenance_tick(64).await.unwrap();
    assert!(work.collector_enabled);
    assert!(work.examined <= 64);
    assert_eq!(work.orphans_retired, 1);
    assert_eq!(work.payload_pages_removed, 1);
    assert_eq!(work.payload_bytes_removed, payload.size);
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_graph_node").await,
        0
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_page_certificate").await,
        1
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_source_root_attestation"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_gc_op WHERE state='APPLIED'"
        )
        .await,
        1
    );
    let operation: String = q
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT operation_id::text FROM mst2_metadata_gc_op",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    let fresh = write_rooted(&writer, "gc-generation-two", &plan, payload.clone()).await;
    assert_eq!(fresh.root_generation(), first.root_generation() + 1);
    assert!(writer.recover("gc-generation-one", &plan).await.is_err());
    for _ in 0..2 {
        q.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT mst2_metadata_gc_apply($1::uuid)",
            [operation.clone().into()],
        ))
        .await
        .unwrap();
    }
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation=2"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_lifetime WHERE generation=1 AND state='REMOVED'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_lifetime WHERE generation=2 AND state='LIVE'"
        )
        .await,
        1
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_page_certificate").await,
        2
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_current WHERE generation=2"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn pending_gc_survives_rebuild_and_payload_stamp_cannot_commit_alone() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let (plan, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let intent = write_rooted(&writer, "gc-pending", &plan, payload).await;
    age_orphan(&q, &namespace, intent.prepare_id()).await;
    assert_eq!(writer.maintenance_tick(1).await.unwrap().orphans_retired, 1);
    let operation = claim(&q, &intent).await.unwrap();
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_gc_op WHERE state='PENDING'"
        )
        .await,
        1
    );
    let txn = q.begin().await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE mst2_metadata_gc_op SET payload_delete_xid=txid_current() WHERE operation_id=$1::uuid",
        [operation.into()])).await.unwrap();
    let error = txn.commit().await.unwrap_err();
    assert!(
        error.to_string().contains("cannot escape its atomic apply"),
        "{error}"
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_payload").await,
        1
    );
    assert_eq!(count(&q,"SELECT count(*) FROM mst2_metadata_gc_op WHERE state='PENDING' AND payload_delete_xid IS NULL").await,1);
    drop(writer);
    let rebuilt = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let work = rebuilt.maintenance_tick(1).await.unwrap();
    assert_eq!(work.examined, 1);
    assert_eq!(work.pending_replayed, 1);
    assert_eq!(work.gc_applied, 1);
    assert_eq!(work.payload_pages_removed, 1);
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_gc_op WHERE state='APPLIED'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn actual_incoming_edge_blocks_child_gc_until_parent_removal() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let (child, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let child_intent = write_rooted(&writer, "gc-edge-child", &child, payload).await;
    let hint = writer
        .lookup_reuse(&child.identity.tagged_root_tree_oid, &child.identity)
        .await
        .unwrap()
        .unwrap();
    let entries = [
        Entry::dir(b"one", child.root),
        Entry::dir(b"two", child.root),
    ];
    let bytes = Page::build(&entries).unwrap();
    let root = page_id(&bytes);
    let mut body = b"40000 one\0".to_vec();
    body.extend_from_slice(&[0xaa; 20]);
    body.extend_from_slice(b"40000 two\0");
    body.extend_from_slice(&[0xaa; 20]);
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO mega_tree(id,tree_id,sub_trees,size,created_at,pack_id,pack_offset,commit_id)
         VALUES(2,$1,$2,0,now(),'fixture',0,'fixture')",
        ["d".repeat(40).into(), body.into()],
    ))
    .await
    .unwrap();
    let mut identity = child.identity.clone();
    identity.tagged_root_tree_oid = format!("sha1:{}", "d".repeat(40));
    let parent = RootedMetadataInstallPlan::new(
        identity.clone(),
        root,
        BTreeMap::from([(root, bytes.len() as u64)]),
        BTreeSet::from([(root, child.root)]),
        BTreeMap::from([(child.root, hint.proof)]),
        BTreeMap::from([
            (identity.tagged_root_tree_oid, root),
            (child.identity.tagged_root_tree_oid.clone(), child.root),
        ]),
    )
    .unwrap();
    let parent_intent = write_rooted(
        &writer,
        "gc-edge-parent",
        &parent,
        MetadataPagePayload {
            id: root,
            size: bytes.len() as u64,
            bytes,
        },
    )
    .await;
    age_orphan(&q, &namespace, child_intent.prepare_id()).await;
    age_orphan(&q, &namespace, parent_intent.prepare_id()).await;
    for _ in 0..2 {
        assert_eq!(writer.maintenance_tick(1).await.unwrap().orphans_retired, 1);
    }
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_root_anchor").await,
        0
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_graph_edge").await,
        1
    );
    assert_eq!(
        count(
            &q,
            "SELECT sum(incoming_refs)::bigint FROM mst2_metadata_graph_node"
        )
        .await,
        1
    );
    assert!(
        claim(&q, &child_intent)
            .await
            .unwrap_err()
            .to_string()
            .contains("incoming edges")
    );
    let operation = claim(&q, &parent_intent).await.unwrap();
    q.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT mst2_metadata_gc_apply($1::uuid)",
        [operation.into()],
    ))
    .await
    .unwrap();
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_graph_edge").await,
        0
    );
    assert_eq!(
        count(
            &q,
            "SELECT sum(incoming_refs)::bigint FROM mst2_metadata_graph_node"
        )
        .await,
        0
    );
    let operation = claim(&q, &child_intent).await.unwrap();
    q.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT mst2_metadata_gc_apply($1::uuid)",
        [operation.into()],
    ))
    .await
    .unwrap();
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_verified_ref").await,
        2
    );
}

#[tokio::test]
async fn restored_guards_refuse_corrupt_payload_size_counter_and_current_bindings() {
    for case in 0..4 {
        let (config, core, namespace, q, _guard) = fixture().await;
        let (plan, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
        let writer = RootedQualifiedMetadataRepository::open(&core, &config)
            .await
            .unwrap();
        let intent = write_rooted(&writer, "gc-corruption", &plan, payload).await;
        age_orphan(&q, &namespace, intent.prepare_id()).await;
        assert_eq!(writer.maintenance_tick(1).await.unwrap().orphans_retired, 1);
        let (table, guard, statement) = match case {
            0 => (
                "mst2_metadata_payload",
                "mst2_metadata_payload_fenced",
                "UPDATE mst2_metadata_payload SET payload=set_byte(payload,20,255)",
            ),
            1 => (
                "mst2_metadata_graph_node",
                "mst2_metadata_graph_node_guard",
                "UPDATE mst2_metadata_graph_node SET bytes=bytes+1",
            ),
            2 => (
                "mst2_metadata_graph_node",
                "mst2_metadata_graph_node_guard",
                "UPDATE mst2_metadata_graph_node SET incoming_refs=1",
            ),
            _ => (
                "mst2_metadata_current",
                "mst2_metadata_current_guard",
                "DELETE FROM mst2_metadata_current",
            ),
        };
        let txn = q.begin().await.unwrap();
        for trigger in ["mst2_00_family_barrier", guard] {
            txn.execute_unprepared(&format!("ALTER TABLE {table} DISABLE TRIGGER {trigger}"))
                .await
                .unwrap();
        }
        txn.execute_unprepared(statement).await.unwrap();
        for trigger in ["mst2_00_family_barrier", guard] {
            txn.execute_unprepared(&format!("ALTER TABLE {table} ENABLE TRIGGER {trigger}"))
                .await
                .unwrap();
        }
        txn.commit().await.unwrap();
        assert_eq!(
            catalog(&q, namespace.core_oid, namespace.schema_oid)
                .await
                .unwrap(),
            namespace.catalog_fingerprint
        );
        let error = claim(&q, &intent).await.unwrap_err();
        assert!(
            error.to_string().contains(if case == 0 {
                "durable bytes"
            } else if case < 3 {
                "actual counter"
            } else {
                "current generation"
            }),
            "{error}"
        );
        assert_eq!(
            count(&q, "SELECT count(*) FROM mst2_metadata_gc_op").await,
            0
        );
        assert_eq!(
            count(&q, "SELECT count(*) FROM mst2_metadata_payload").await,
            1
        );
        assert_eq!(
            count(
                &q,
                "SELECT count(*) FROM mst2_metadata_lifetime WHERE state='LIVE'"
            )
            .await,
            1
        );
    }
}
