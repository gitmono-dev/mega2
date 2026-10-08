use std::sync::Arc;

use mst2_codec::metapage::{Entry, EntryKind};
use sea_orm::Database;
use sea_orm_migration::MigratorTrait;

use super::{
    super::history::{MetadataTerminalAction, MetadataTerminalError, MetadataTerminalObservation},
    *,
};
use crate::{
    ceres::snapshot::retention_dag::MetadataDagBuilder,
    jupiter::{
        migration::Migrator,
        tests::{TestSchemaGuard, test_db_config},
    },
};

fn prepared() -> PreparedNativeMetadataRetention {
    let entries = [Entry::file(EntryKind::Regular, b"file", 3, [42; 32])];
    let child = Page::build(&entries).unwrap();
    let roots = [
        Entry::dir(b"one", page_id(&child)),
        Entry::dir(b"two", page_id(&child)),
    ];
    let root = Page::build(&roots).unwrap();
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&child, &entries).unwrap();
    builder.add_directory(&root, &roots).unwrap();
    PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page_id(&root)).unwrap()),
        "/",
    )
}

async fn fixture() -> (DatabaseConnection, DatabaseConnection, TestSchemaGuard) {
    let temp = tempfile::tempdir().unwrap();
    let (config, schema) = test_db_config(temp.path()).await;
    let first = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&first, None).await.unwrap();
    let second = Database::connect(config.db_url).await.unwrap();
    (first, second, schema)
}

async fn scalar<C: ConnectionTrait>(db: &C, sql: &str) -> i64 {
    db.query_one_raw(statement(sql, []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn installed(
    db: &DatabaseConnection,
    operation: &str,
) -> (
    PostgresQualifiedMetadataRepository,
    PreparedNativeMetadataRetention,
    GenerationMetadataReceipt,
) {
    let repo = PostgresQualifiedMetadataRepository::new(db.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repo.begin_intent(operation, &pages).await.unwrap();
    repo.install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let receipt = repo.finalize(&intent).await.unwrap();
    (repo, pages, receipt)
}

async fn claim_root(
    repo: &PostgresQualifiedMetadataRepository,
    receipt: &GenerationMetadataReceipt,
) -> MetadataGcClaim {
    let lifetime = repo.lifetime(receipt.metadata_root()).await.unwrap();
    repo.claim(&uuid::Uuid::new_v4().to_string(), &lifetime)
        .await
        .unwrap()
}

async fn wait_for_waiter(txn: &DatabaseTransaction) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let waiting:bool=txn.query_one_raw(statement(
            "SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted
             AND classid=$1::integer::oid AND objid=hashtext(current_schema())::oid
             AND database=(SELECT oid FROM pg_database WHERE datname=current_database())) AS waiting",
            [RETENTION_LOCK_KEY.into()],
        )).await.unwrap().unwrap().try_get("","waiting").unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "real independent connection must wait on retention barrier"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn qualified_graph_exact_fks_unique_edges_and_permanent_domains() {
    let (first, second, _schema) = fixture().await;
    let (repo, pages, receipt) = installed(&first, "qualified-graph").await;
    repo.install_pages(receipt.intent(), pages.dag().payloads())
        .await
        .unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_graph_node").await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_graph_edge").await,
        1
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT sum(incoming_refs)::bigint FROM mst2_metadata_graph_node"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_node").await,
        0
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_lifetime WHERE graph_domain='qualified-v1'"
        )
        .await,
        2
    );
    repo.retire_prepare_coverage(&receipt).await.unwrap();
    let generic = PostgresMetadataGenerationRepository::new(second.clone())
        .await
        .unwrap();
    assert!(
        generic
            .begin_intent("cannot-adopt-retired-qualified", &pages)
            .await
            .is_err()
    );
    for sql in [
        "UPDATE mst2_metadata_lifetime SET graph_domain='generic-v1'",
        "UPDATE mst2_metadata_graph_node SET incoming_refs=incoming_refs+1",
        "UPDATE mst2_metadata_graph_edge SET child_generation=child_generation+1",
        "DELETE FROM mst2_metadata_current",
        "UPDATE mst2_metadata_payload SET payload=payload",
        "DELETE FROM mst2_metadata_storage_scope",
    ] {
        assert!(second.execute_unprepared(sql).await.is_err(), "{sql}");
    }
    let claim = claim_root(&repo, &receipt).await;
    let applied = repo.apply(&claim).await.unwrap();
    assert_eq!(applied.claim(), &claim);
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_gc_op WHERE state='APPLIED'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare_page WHERE generation=1"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        2
    );
}

#[tokio::test]
async fn qualified_shared_child_and_other_prepare_are_real_collection_vetoes() {
    let (first, second, _schema) = fixture().await;
    let (repo, pages, receipt) = installed(&first, "shared-owner-one").await;
    let other = PostgresQualifiedMetadataRepository::new(second)
        .await
        .unwrap();
    let intent = other
        .begin_intent("shared-owner-two", &pages)
        .await
        .unwrap();
    let shared = other.finalize(&intent).await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_graph_root").await,
        4
    );
    repo.retire_prepare_coverage(&receipt).await.unwrap();
    let root = repo.lifetime(receipt.metadata_root()).await.unwrap();
    assert!(
        repo.claim(&uuid::Uuid::new_v4().to_string(), &root)
            .await
            .is_err()
    );
    other.retire_prepare_coverage(&shared).await.unwrap();
    let child = pages
        .dag()
        .payloads()
        .iter()
        .find(|p| p.id != receipt.metadata_root())
        .unwrap();
    let child = repo.lifetime(child.id).await.unwrap();
    assert!(
        repo.claim(&uuid::Uuid::new_v4().to_string(), &child)
            .await
            .is_err()
    );
    let root_claim = repo
        .claim(&uuid::Uuid::new_v4().to_string(), &root)
        .await
        .unwrap();
    repo.apply(&root_claim).await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT incoming_refs FROM mst2_metadata_graph_node").await,
        0
    );
    let child_claim = repo
        .claim(&uuid::Uuid::new_v4().to_string(), &child)
        .await
        .unwrap();
    repo.apply(&child_claim).await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
}

#[tokio::test]
async fn qualified_pending_preparation_and_lost_roots_fail_closed() {
    let (first, second, _schema) = fixture().await;
    let (repo, pages, receipt) = installed(&first, "lost-roots").await;
    second
        .execute_raw(statement(
            "DELETE FROM mst2_metadata_graph_root WHERE prepare_id=$1",
            [receipt.intent().prepare_id().into()],
        ))
        .await
        .unwrap();
    assert!(repo.retire_prepare_coverage(&receipt).await.is_err());
    let lifetime = repo.lifetime(receipt.metadata_root()).await.unwrap();
    assert!(
        repo.claim(&uuid::Uuid::new_v4().to_string(), &lifetime)
            .await
            .is_err()
    );
    let preparing = repo.begin_intent("pending-owner", &pages).await.unwrap();
    assert!(
        repo.claim(&uuid::Uuid::new_v4().to_string(), &lifetime)
            .await
            .is_err()
    );
    repo.abort(&preparing).await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_gc_op").await,
        0
    );
}

#[tokio::test]
async fn qualified_explicit_aborted_orphans_with_and_without_payload_collect() {
    for partial in [false, true] {
        let (first, second, _schema) = fixture().await;
        let repo = PostgresQualifiedMetadataRepository::new(first.clone())
            .await
            .unwrap();
        let pages = prepared();
        let intent = repo.begin_intent("orphan", &pages).await.unwrap();
        if partial {
            repo.install_pages(&intent, pages.dag().payloads())
                .await
                .unwrap();
        }
        let lifetime = repo.lifetime(pages.dag().root()).await.unwrap();
        assert!(
            repo.claim(&uuid::Uuid::new_v4().to_string(), &lifetime)
                .await
                .is_err()
        );
        repo.abort(&intent).await.unwrap();
        let claim = repo
            .claim(&uuid::Uuid::new_v4().to_string(), &lifetime)
            .await
            .unwrap();
        assert!(!claim.graph_present);
        assert_eq!(claim.had_payload, partial);
        repo.apply(&claim).await.unwrap();
        assert_eq!(
            repo.inspect_gc(
                &second,
                claim.operation_id(),
                &lifetime,
                MetadataGcPhase::Apply
            )
            .await
            .unwrap(),
            MetadataGcObservation::Applied(Box::new(repo.apply(&claim).await.unwrap()))
        );
        assert_eq!(
            scalar(
                &first,
                "SELECT count(*) FROM mst2_metadata_lifetime WHERE state='REMOVED'"
            )
            .await,
            1
        );
    }
}

#[tokio::test]
async fn qualified_abort_and_collection_fence_a_late_installer_at_actual_pg_lock() {
    let (first, second, _schema) = fixture().await;
    let repo = PostgresQualifiedMetadataRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repo.begin_intent("late-installer", &pages).await.unwrap();
    repo.install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let lifetime = repo.lifetime(pages.dag().root()).await.unwrap();
    let held = first.begin().await.unwrap();
    repo.inner.inner.barrier(&held).await.unwrap();
    let late_intent = intent.clone();
    let late_pages = pages.dag().payloads().to_vec();
    let late = tokio::spawn(async move {
        let late_repo = PostgresQualifiedMetadataRepository::new(second)
            .await
            .unwrap();
        late_repo.install_pages(&late_intent, &late_pages).await
    });
    wait_for_waiter(&held).await;
    repo.terminate_in_txn(&held, &intent, MetadataTerminalAction::Abort, None)
        .await
        .unwrap();
    let claim = repo
        .claim_in_txn(&held, &uuid::Uuid::new_v4().to_string(), &lifetime)
        .await
        .unwrap();
    repo.apply_in_txn(&held, &claim).await.unwrap();
    held.commit().await.unwrap();
    assert!(late.await.unwrap().is_err());
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        1
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_gc_op WHERE state='APPLIED'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn qualified_statement_barrier_raw_writer_waits_before_row_lock_and_rechecks_domain() {
    for generic in [true, false] {
        let (first, second, _schema) = fixture().await;
        let (repo, _pages, receipt) = installed(&first, "raw-writer").await;
        repo.retire_prepare_coverage(&receipt).await.unwrap();
        let root = repo.lifetime(receipt.metadata_root()).await.unwrap();
        let held = first.begin().await.unwrap();
        repo.inner.inner.barrier(&held).await.unwrap();
        let sql = if generic {
            format!(
                "INSERT INTO mst2_retention_node(node_id,kind,bytes,state) VALUES('page:sha256:{}','page',{},'LIVE')",
                hex::encode(root.page_id),
                root.expected_size
            )
        } else {
            format!(
                "UPDATE mst2_metadata_graph_node SET state='LIVE' WHERE page_id=decode('{}','hex') AND generation={}",
                hex::encode(root.page_id),
                root.generation
            )
        };
        let writer = tokio::spawn(async move { second.execute_unprepared(&sql).await });
        wait_for_waiter(&held).await;
        held.query_one_raw(statement("SELECT page_id FROM mst2_metadata_graph_node WHERE page_id=$1 AND generation=$2 FOR UPDATE NOWAIT",
        [root.page_id.to_vec().into(),root.generation.into()])).await.unwrap().unwrap();
        let claim = repo
            .claim_in_txn(&held, &uuid::Uuid::new_v4().to_string(), &root)
            .await
            .unwrap();
        held.commit().await.unwrap();
        assert!(writer.await.unwrap().is_err());
        repo.apply(&claim).await.unwrap();
        assert_eq!(
            scalar(&first, "SELECT count(*) FROM mst2_retention_node").await,
            0
        );
    }
}

#[tokio::test]
async fn qualified_claim_outer_rollback_and_lost_commit_response_use_fresh_barrier() {
    let (first, second, _schema) = fixture().await;
    let (repo, _pages, receipt) = installed(&first, "claim-recovery").await;
    repo.retire_prepare_coverage(&receipt).await.unwrap();
    let lifetime = repo.lifetime(receipt.metadata_root()).await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let held = first.begin().await.unwrap();
    repo.claim_in_txn(&held, &id, &lifetime).await.unwrap();
    held.rollback().await.unwrap();
    assert_eq!(
        repo.inspect_gc(&second, &id, &lifetime, MetadataGcPhase::Claim)
            .await
            .unwrap(),
        MetadataGcObservation::Absent
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_graph_node WHERE state='LIVE'"
        )
        .await,
        2
    );
    let held = first.begin().await.unwrap();
    let claim = repo.claim_in_txn(&held, &id, &lifetime).await.unwrap();
    held.commit().await.unwrap(); // The caller deliberately discards the commit response.
    assert_eq!(
        repo.inspect_gc(&second, &id, &lifetime, MetadataGcPhase::Claim)
            .await
            .unwrap(),
        MetadataGcObservation::Pending(Box::new(claim.clone()))
    );
    assert_eq!(repo.pending_gc(1).await.unwrap(), vec![claim.clone()]);
    repo.apply(&claim).await.unwrap();
}

#[tokio::test]
async fn qualified_apply_faults_rollback_bytes_graph_counters_and_receipt_with_guards_enabled() {
    for (table, event, predicate) in [
        ("mst2_metadata_payload", "AFTER DELETE", "true"),
        ("mst2_metadata_graph_edge", "AFTER DELETE", "true"),
        (
            "mst2_metadata_gc_op",
            "BEFORE UPDATE",
            "NEW.state='APPLIED'",
        ),
    ] {
        let (first, second, _schema) = fixture().await;
        let (repo, _pages, receipt) = installed(&first, "apply-fault").await;
        repo.retire_prepare_coverage(&receipt).await.unwrap();
        let claim = claim_root(&repo, &receipt).await;
        let granularity = if table == "mst2_metadata_graph_edge" {
            "STATEMENT"
        } else {
            "ROW"
        };
        let sql = format!(
            "CREATE FUNCTION test_gc_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF {predicate} THEN RAISE EXCEPTION 'injected atomic GC failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER zzz_test_gc_fault {event} ON {table} FOR EACH {granularity} EXECUTE FUNCTION test_gc_fault()"
        );
        second.execute_unprepared(&sql).await.unwrap();
        assert!(repo.apply(&claim).await.is_err());
        assert_eq!(
            scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
            2
        );
        assert_eq!(
            scalar(&first, "SELECT count(*) FROM mst2_metadata_graph_edge").await,
            1
        );
        assert_eq!(
            scalar(
                &first,
                "SELECT sum(incoming_refs)::bigint FROM mst2_metadata_graph_node"
            )
            .await,
            1
        );
        assert_eq!(
            scalar(&first, "SELECT count(*) FROM mst2_metadata_graph_node").await,
            2
        );
        assert_eq!(scalar(&first,"SELECT count(*) FROM mst2_metadata_gc_op WHERE state='PENDING' AND payload_delete_xid IS NULL").await,1);
        assert_eq!(
            scalar(
                &first,
                "SELECT count(*) FROM mst2_metadata_lifetime WHERE state='DELETING'"
            )
            .await,
            1
        );
        assert_eq!(scalar(&first,"SELECT count(*) FROM pg_trigger WHERE tgname IN ('mst2_metadata_payload_fenced','mst2_metadata_payload_removed','mst2_metadata_graph_node_guard','mst2_metadata_gc_op_guard') AND tgenabled='O' AND tgrelid IN (SELECT oid FROM pg_class WHERE relnamespace=(SELECT oid FROM pg_namespace WHERE nspname=current_schema()))").await,4);
        second
            .execute_unprepared(&format!("DROP TRIGGER zzz_test_gc_fault ON {table}"))
            .await
            .unwrap();
        repo.apply(&claim).await.unwrap();
    }
}

#[tokio::test]
async fn qualified_direct_selected_delete_completes_all_bookkeeping_and_guc_is_not_authority() {
    let (first, second, _schema) = fixture().await;
    let (repo, _pages, receipt) = installed(&first, "direct-delete").await;
    repo.retire_prepare_coverage(&receipt).await.unwrap();
    let claim = claim_root(&repo, &receipt).await;
    for sql in [
        "UPDATE mst2_metadata_gc_op SET had_payload=false",
        "UPDATE mst2_metadata_gc_op SET graph_present=false",
        "UPDATE mst2_metadata_gc_op SET generation=generation+1",
    ] {
        assert!(second.execute_unprepared(sql).await.is_err(), "{sql}");
    }
    assert!(
        second
            .query_one_raw(statement(
                "SELECT mst2_metadata_gc_finish($1::uuid)",
                [claim.operation_id.clone().into()]
            ))
            .await
            .is_err()
    );
    second
        .execute_unprepared("UPDATE mst2_metadata_gc_op SET payload_delete_xid=1")
        .await
        .unwrap();
    let marked = second.begin().await.unwrap();
    marked
        .execute_raw(statement(
            "UPDATE mst2_metadata_gc_op SET payload_delete_xid=42 WHERE operation_id=$1::uuid",
            [claim.operation_id.clone().into()],
        ))
        .await
        .unwrap();
    let same_xid:bool=marked.query_one_raw(statement(
        "SELECT payload_delete_xid=txid_current() AS same_xid FROM mst2_metadata_gc_op WHERE operation_id=$1::uuid",
        [claim.operation_id.clone().into()],
    )).await.unwrap().unwrap().try_get("","same_xid").unwrap();
    assert!(
        same_xid,
        "database must replace a caller marker with the actual transaction identity"
    );
    let error = marked
        .query_one_raw(statement(
            "SELECT mst2_metadata_gc_finish($1::uuid)",
            [claim.operation_id.clone().into()],
        ))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("payload presence changed"),
        "{error}"
    );
    marked.rollback().await.unwrap();
    assert!(
        second
            .query_one_raw(statement(
                "SELECT mst2_metadata_gc_finish($1::uuid)",
                [claim.operation_id.clone().into()]
            ))
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    assert!(
        second
            .execute_unprepared("DELETE FROM mst2_metadata_payload")
            .await
            .is_err()
    );
    let txn = second.begin().await.unwrap();
    txn.execute_raw(statement(
        "SELECT set_config('mega2.metadata_gc_operation',$1,true)",
        [uuid::Uuid::new_v4().to_string().into()],
    ))
    .await
    .unwrap();
    assert!(
        txn.execute_raw(statement(
            "DELETE FROM mst2_metadata_payload WHERE page_id=$1 AND generation=$2",
            [
                claim.lifetime.page_id.to_vec().into(),
                claim.lifetime.generation.into()
            ]
        ))
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    let txn = second.begin().await.unwrap();
    txn.execute_raw(statement(
        "SELECT set_config('mega2.metadata_gc_operation',$1,true)",
        [claim.operation_id.clone().into()],
    ))
    .await
    .unwrap();
    txn.execute_raw(statement(
        "DELETE FROM mst2_metadata_payload WHERE page_id=$1 AND generation=$2",
        [
            claim.lifetime.page_id.to_vec().into(),
            claim.lifetime.generation.into(),
        ],
    ))
    .await
    .unwrap();
    assert_eq!(
        scalar(
            &txn,
            "SELECT count(*) FROM mst2_metadata_gc_op WHERE state='APPLIED'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&txn, "SELECT count(*) FROM mst2_metadata_graph_edge").await,
        0
    );
    assert_eq!(
        scalar(&txn, "SELECT incoming_refs FROM mst2_metadata_graph_node").await,
        0
    );
    txn.commit().await.unwrap();
    repo.apply(&claim).await.unwrap();
    assert!(
        second
            .execute_unprepared("DELETE FROM mst2_metadata_gc_op")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn qualified_fresh_aba_replays_old_receipts_without_deleting_g2_or_reviving_old_intents() {
    let (first, second, _schema) = fixture().await;
    let (repo, pages, receipt) = installed(&first, "aba-old").await;
    let observation = repo.observe_installed_dag(receipt.intent()).await.unwrap();
    let terminal = repo.retire_prepare_coverage(&receipt).await.unwrap();
    let claim = claim_root(&repo, &receipt).await;
    second.execute_unprepared("CREATE TABLE test_payload_deletes(n integer NOT NULL); CREATE FUNCTION test_record_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO test_payload_deletes VALUES(1); RETURN NULL; END $$; CREATE TRIGGER zzz_test_record_delete AFTER DELETE ON mst2_metadata_payload FOR EACH ROW EXECUTE FUNCTION test_record_delete()").await.unwrap();
    let held = first.begin().await.unwrap();
    let applied = repo.apply_in_txn(&held, &claim).await.unwrap();
    held.commit().await.unwrap(); // Deliberately lose delivery; inspect recovers the exact durable receipt.
    assert_eq!(
        repo.inspect_gc(
            &second,
            claim.operation_id(),
            claim.lifetime(),
            MetadataGcPhase::Apply
        )
        .await
        .unwrap(),
        MetadataGcObservation::Applied(Box::new(applied.clone()))
    );
    assert!(
        repo.begin_intent("ordinary-cannot-reopen", &pages)
            .await
            .is_err()
    );
    let fresh = repo
        .begin_fresh_intent("aba-fresh", &pages, std::slice::from_ref(&applied))
        .await
        .unwrap();
    assert_eq!(
        repo.lifetime(receipt.metadata_root())
            .await
            .unwrap()
            .generation(),
        2
    );
    repo.install_pages(&fresh, pages.dag().payloads())
        .await
        .unwrap();
    repo.finalize(&fresh).await.unwrap();
    assert!(
        repo.install_pages(receipt.intent(), pages.dag().payloads())
            .await
            .is_err()
    );
    assert!(
        repo.finalize_observation(receipt.intent(), &observation)
            .await
            .is_err()
    );
    let restarted = PostgresQualifiedMetadataRepository::new(second.clone())
        .await
        .unwrap();
    let old = restarted
        .historical_lifetime(claim.lifetime.page_id, 1)
        .await
        .unwrap();
    let recovered = restarted
        .inspect_gc(&second, claim.operation_id(), &old, MetadataGcPhase::Apply)
        .await
        .unwrap();
    let MetadataGcObservation::Applied(recovered) = recovered else {
        panic!("restart must recover historical APPLIED receipt");
    };
    assert_eq!(restarted.apply(recovered.claim()).await.unwrap(), applied);
    assert!(
        restarted
            .claim(&uuid::Uuid::new_v4().to_string(), &old)
            .await
            .is_err()
    );
    let terminal_intent = restarted
        .capture_terminal_intent(
            receipt.intent().operation_id(),
            receipt.intent().manifest_digest(),
        )
        .await
        .unwrap();
    assert_eq!(
        restarted
            .inspect_terminal(
                &second,
                &terminal_intent,
                MetadataTerminalAction::RetireCoverage
            )
            .await
            .unwrap(),
        MetadataTerminalObservation::Terminated(Box::new(terminal.clone()))
    );
    assert_eq!(
        repo.retire_prepare_coverage(&receipt).await.unwrap(),
        terminal
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM test_payload_deletes").await,
        1
    );
    let stale = second.begin().await.unwrap();
    stale
        .execute_raw(statement(
            "SELECT set_config('mega2.metadata_gc_operation',$1,true)",
            [claim.operation_id.clone().into()],
        ))
        .await
        .unwrap();
    assert!(
        stale
            .execute_raw(statement(
                "DELETE FROM mst2_metadata_payload WHERE page_id=$1 AND generation=2",
                [claim.lifetime.page_id.to_vec().into()]
            ))
            .await
            .is_err()
    );
    stale.rollback().await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        3
    );
    assert_eq!(
        repo.begin_fresh_intent("aba-fresh", &pages, std::slice::from_ref(&applied))
            .await
            .unwrap(),
        fresh
    );
    let mut forged = applied.clone();
    forged.created_at += chrono::Duration::seconds(1);
    assert!(
        repo.begin_fresh_intent("aba-fresh", &pages, &[forged])
            .await
            .is_err()
    );
    assert!(
        repo.begin_fresh_intent("stale-fresh-proof", &pages, &[applied])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn qualified_multi_page_fresh_failure_rolls_back_all_watermarks_and_new_history() {
    let (first, second, _schema) = fixture().await;
    let (repo, pages, receipt) = installed(&first, "multi-old").await;
    repo.retire_prepare_coverage(&receipt).await.unwrap();
    let root = claim_root(&repo, &receipt).await;
    let root = repo.apply(&root).await.unwrap();
    let child_id = pages
        .dag()
        .payloads()
        .iter()
        .find(|p| p.id != receipt.metadata_root())
        .unwrap()
        .id;
    let child = repo.lifetime(child_id).await.unwrap();
    let child = repo
        .claim(&uuid::Uuid::new_v4().to_string(), &child)
        .await
        .unwrap();
    let child = repo.apply(&child).await.unwrap();
    second.execute_unprepared("CREATE FUNCTION test_fresh_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.generation=2 AND NEW.page_id=(SELECT page_id FROM mst2_metadata_current ORDER BY page_id DESC LIMIT 1) THEN RAISE EXCEPTION 'injected multi-page fresh CAS failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER zzz_test_fresh_fault BEFORE UPDATE ON mst2_metadata_current FOR EACH ROW EXECUTE FUNCTION test_fresh_fault()").await.unwrap();
    assert!(
        repo.begin_fresh_intent("multi-fresh", &pages, &[root.clone(), child.clone()])
            .await
            .is_err()
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_current WHERE generation=1"
        )
        .await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        2
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE operation_id='multi-fresh'"
        )
        .await,
        0
    );
    second
        .execute_unprepared("DROP TRIGGER zzz_test_fresh_fault ON mst2_metadata_current")
        .await
        .unwrap();
    let intent = repo
        .begin_fresh_intent("multi-fresh", &pages, &[root, child])
        .await
        .unwrap();
    repo.install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    repo.finalize(&intent).await.unwrap();
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_current WHERE generation=2"
        )
        .await,
        2
    );
}

#[tokio::test]
async fn qualified_wrong_primary_and_repeatable_read_never_prove_absence_or_authorize_delete() {
    let (first, second, _schema) = fixture().await;
    let (other, _other_second, _other_schema) = fixture().await;
    let (repo, _pages, receipt) = installed(&first, "scope").await;
    repo.retire_prepare_coverage(&receipt).await.unwrap();
    let claim = claim_root(&repo, &receipt).await;
    assert!(matches!(
        repo.inspect_gc(
            &other,
            claim.operation_id(),
            claim.lifetime(),
            MetadataGcPhase::Apply
        )
        .await,
        Err(MetadataGcError::CommitUncertain { .. })
    ));
    let txn = second
        .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
        .await
        .unwrap();
    assert!(
        txn.execute_unprepared("DELETE FROM mst2_metadata_payload")
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    repo.apply(&claim).await.unwrap();
}

#[tokio::test]
async fn qualified_null_bytes_and_retired_generic_incarnations_cannot_be_adopted() {
    let (first, second, _schema) = fixture().await;
    let pages = prepared();
    let payload = &pages.dag().payloads()[0];
    first.execute_raw(statement("INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload) VALUES($1,1,$2,$3)",
        [payload.id.to_vec().into(),(payload.size as i32).into(),payload.bytes.clone().into()])).await.unwrap();
    let qualified = PostgresQualifiedMetadataRepository::new(second.clone())
        .await
        .unwrap();
    assert!(
        qualified
            .begin_intent("cannot-adopt-null", &pages)
            .await
            .is_err()
    );
    assert!(
        second
            .execute_unprepared("DELETE FROM mst2_metadata_payload")
            .await
            .is_err()
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NULL"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_current").await,
        0
    );
    let (first, second, _second_schema) = fixture().await;
    let generic = PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let intent = generic
        .begin_intent("generic-history", &pages)
        .await
        .unwrap();
    generic
        .install_pages(&intent, pages.dag().payloads())
        .await
        .unwrap();
    let receipt = generic.finalize(&intent).await.unwrap();
    generic.retire_prepare_coverage(&receipt).await.unwrap();
    let qualified = PostgresQualifiedMetadataRepository::new(second)
        .await
        .unwrap();
    assert!(
        qualified
            .begin_intent("cannot-adopt-retired-generic", &pages)
            .await
            .is_err()
    );
    assert!(qualified.lifetime(payload.id).await.is_err());
}

#[tokio::test]
async fn qualified_committed_partial_payload_is_rejected_without_repair() {
    let (first, second, _schema) = fixture().await;
    let repo = PostgresQualifiedMetadataRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repo
        .begin_intent("corrupt-partial-committed", &pages)
        .await
        .unwrap();
    repo.install_page(&intent, &pages.dag().payloads()[0])
        .await
        .unwrap();
    second.execute_raw(statement("UPDATE mst2_metadata_prepare SET state='COMMITTED',committed_at=clock_timestamp() WHERE prepare_id=$1",
        [intent.prepare_id().into()])).await.unwrap();
    assert!(
        repo.install_pages(&intent, pages.dag().payloads())
            .await
            .is_err()
    );
    let lifetime = repo.lifetime(pages.dag().root()).await.unwrap();
    assert!(
        repo.claim(&uuid::Uuid::new_v4().to_string(), &lifetime)
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        1
    );
}

#[tokio::test]
async fn qualified_cross_action_terminal_inspection_conflicts_and_production_adoption_stays_closed()
{
    let (first, second, _schema) = fixture().await;
    let (repo, pages, receipt) = installed(&first, "cross-action-committed").await;
    assert!(matches!(
        repo.inspect_terminal(&second, receipt.intent(), MetadataTerminalAction::Abort)
            .await,
        Err(MetadataTerminalError::Rejected(SnapshotError {
            code: SnapshotErrorCode::Conflict,
            ..
        }))
    ));
    let pending = repo
        .begin_intent("cross-action-aborted", &pages)
        .await
        .unwrap();
    repo.abort(&pending).await.unwrap();
    assert!(matches!(
        repo.inspect_terminal(&second, &pending, MetadataTerminalAction::RetireCoverage)
            .await,
        Err(MetadataTerminalError::Rejected(SnapshotError {
            code: SnapshotErrorCode::Conflict,
            ..
        }))
    ));
    assert!(repo.abort(receipt.intent()).await.is_err());
    let sql="INSERT INTO mst2_snapshot_context(snapshot_id,canonical_descriptor,instance_id,commit_oid,root_tree_oid,
        metadata_root,prepare_id,publication_sequence,writer_epoch,state) VALUES('blocked',$1,'i','c','r',$2,$3,0,1,'READY')";
    assert!(
        second
            .execute_raw(statement(
                sql,
                [
                    vec![1_u8].into(),
                    receipt.metadata_root().to_vec().into(),
                    receipt.intent().prepare_id().into()
                ]
            ))
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_snapshot_context").await,
        0
    );
    assert_eq!(
        repo.inspect_terminal(
            &second,
            receipt.intent(),
            MetadataTerminalAction::RetireCoverage
        )
        .await
        .unwrap(),
        MetadataTerminalObservation::Active
    );
}

#[tokio::test]
async fn qualified_deferred_current_guard_denies_unprotected_raw_fresh_commit() {
    let (first, second, _schema) = fixture().await;
    let (repo, _pages, receipt) = installed(&first, "raw-fresh").await;
    repo.retire_prepare_coverage(&receipt).await.unwrap();
    let claim = claim_root(&repo, &receipt).await;
    repo.apply(&claim).await.unwrap();
    let txn = second.begin().await.unwrap();
    txn.execute_raw(statement("INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size,graph_domain) VALUES($1,$2,2,'RESERVED',1,$3,'qualified-v1')",
        [claim.lifetime.page_id.to_vec().into(),node_id(&claim.lifetime.page_id).into(),claim.lifetime.expected_size.into()])).await.unwrap();
    txn.execute_raw(statement(
        "UPDATE mst2_metadata_current SET generation=2 WHERE page_id=$1",
        [claim.lifetime.page_id.to_vec().into()],
    ))
    .await
    .unwrap();
    assert!(txn.commit().await.is_err());
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_current WHERE generation=2"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_lifetime WHERE generation=2"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn qualified_statement_dag_audit_accepts_wide_and_chain_graphs_and_rejects_a_cycle() {
    for wide in [true, false] {
        let (first, second, _schema) = fixture().await;
        let repo = PostgresQualifiedMetadataRepository::new(first.clone())
            .await
            .unwrap();
        let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
        let mut root_entries = Vec::new();
        let mut last = [0_u8; 32];
        for i in 0..64_u8 {
            let entries = if wide || i == 0 {
                vec![Entry::file(EntryKind::Regular, b"file", 3, [i; 32])]
            } else {
                vec![Entry::dir(b"child", last)]
            };
            let page = Page::build(&entries).unwrap();
            builder.add_directory(&page, &entries).unwrap();
            last = page_id(&page);
            root_entries.push(Entry::dir(format!("d{i:03}").as_bytes(), last));
        }
        let root = if wide {
            let root = Page::build(&root_entries).unwrap();
            builder.add_directory(&root, &root_entries).unwrap();
            page_id(&root)
        } else {
            last
        };
        let pages = PreparedNativeMetadataRetention::test_installation(
            Arc::new(builder.finish(root).unwrap()),
            "/",
        );
        let intent = repo.begin_intent("statement-dag", &pages).await.unwrap();
        repo.install_pages(&intent, &pages.dag().payloads()[..64])
            .await
            .unwrap();
        if wide {
            repo.install_page(&intent, &pages.dag().payloads()[64])
                .await
                .unwrap();
        }
        let receipt = repo.finalize(&intent).await.unwrap();
        assert_eq!(
            scalar(&first, "SELECT count(*) FROM mst2_metadata_graph_node").await,
            if wide { 65 } else { 64 }
        );
        assert_eq!(
            scalar(
                &first,
                "SELECT sum(incoming_refs)::bigint FROM mst2_metadata_graph_node"
            )
            .await,
            if wide { 64 } else { 63 }
        );
        let pending = repo.begin_intent("cycle-attempt", &pages).await.unwrap();
        let leaf = pages
            .dag()
            .payloads()
            .iter()
            .find(|p| {
                !pages
                    .dag()
                    .edges()
                    .iter()
                    .any(|e| e.parent == node_id(&p.id))
            })
            .unwrap()
            .id;
        let error=second.execute_raw(statement(
            "INSERT INTO mst2_metadata_graph_edge(parent_page,parent_generation,child_page,child_generation) VALUES($1,1,$2,1)",
            [leaf.to_vec().into(),root.to_vec().into()],
        )).await.unwrap_err();
        assert!(error.to_string().contains("cycle"), "{error}");
        assert_eq!(
            scalar(
                &first,
                "SELECT sum(incoming_refs)::bigint FROM mst2_metadata_graph_node"
            )
            .await,
            if wide { 64 } else { 63 }
        );
        repo.abort(&pending).await.unwrap();
        repo.retire_prepare_coverage(&receipt).await.unwrap();
    }
}

#[tokio::test]
async fn qualified_statement_dag_overflow_rolls_back_without_disabling_production_guards() {
    let (first, second, _schema) = fixture().await;
    let repo = PostgresQualifiedMetadataRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared();
    let intent = repo.begin_intent("overflow-proof", &pages).await.unwrap();
    assert_eq!(MetadataDagLimits::default().nodes, 4096);
    assert_eq!(MetadataDagLimits::default().edges, 16384);
    let txn = second.begin().await.unwrap();
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size,graph_domain)
         SELECT sha256(convert_to('bound-node-'||i,'UTF8')),'page:sha256:'||encode(sha256(convert_to('bound-node-'||i,'UTF8')),'hex'),
           1,'RESERVED',1,$1,'qualified-v1' FROM generate_series(1,4097) x(i)",
        [(pages.dag().payloads()[0].size as i32).into()],
    )).await.unwrap();
    txn.execute_unprepared("INSERT INTO mst2_metadata_current(page_id,generation) SELECT l.page_id,l.generation FROM mst2_metadata_lifetime l WHERE NOT EXISTS(SELECT 1 FROM mst2_metadata_current c WHERE c.page_id=l.page_id)").await.unwrap();
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_prepare_page(prepare_id,page_id,generation,expected_size)
         SELECT $1,l.page_id,l.generation,l.expected_size FROM mst2_metadata_lifetime l
         WHERE NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m WHERE m.prepare_id=$1 AND m.page_id=l.page_id)",
        [intent.prepare_id().into()],
    )).await.unwrap();
    txn.execute_raw(statement(
        "INSERT INTO mst2_metadata_graph_node(page_id,generation,state,metadata_codec,bytes)
         SELECT page_id,generation,'LIVE',1,expected_size FROM mst2_metadata_prepare_page WHERE prepare_id=$1",
        [intent.prepare_id().into()],
    )).await.unwrap();
    let error=txn.execute_unprepared(
        "INSERT INTO mst2_metadata_graph_edge(parent_page,parent_generation,child_page,child_generation)
         SELECT p.page_id,1,c.page_id,1 FROM (SELECT page_id FROM mst2_metadata_graph_node ORDER BY page_id LIMIT 1) p
         CROSS JOIN mst2_metadata_graph_node c WHERE c.page_id<>p.page_id"
    ).await.unwrap_err();
    assert!(error.to_string().contains("overflow"), "{error}");
    txn.rollback().await.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_graph_node").await,
        0
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_current").await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        2
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_prepare_page").await,
        2
    );
    let captured = repo.scan_current_lifetimes(None, 1).await.unwrap();
    assert_eq!(captured.len(), 1);
    let rest = repo
        .scan_current_lifetimes(Some((captured[0].page_id(), captured[0].generation())), 1)
        .await
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_ne!(rest[0].page_id(), captured[0].page_id());
}

#[tokio::test]
async fn qualified_aborted_intent_is_recovered_from_history_after_repository_restart() {
    let (first, second, _schema) = fixture().await;
    let repo = PostgresQualifiedMetadataRepository::new(first)
        .await
        .unwrap();
    let pages = prepared();
    let intent = repo.begin_intent("restart-aborted", &pages).await.unwrap();
    let terminal = repo.abort(&intent).await.unwrap();
    drop(repo);
    let restarted = PostgresQualifiedMetadataRepository::new(second.clone())
        .await
        .unwrap();
    let recovered = restarted
        .capture_terminal_intent("restart-aborted", intent.manifest_digest())
        .await
        .unwrap();
    assert_eq!(
        restarted
            .inspect_terminal(&second, &recovered, MetadataTerminalAction::Abort)
            .await
            .unwrap(),
        MetadataTerminalObservation::Terminated(Box::new(terminal))
    );
    assert!(
        restarted
            .install_pages(&recovered, pages.dag().payloads())
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&second, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
}
