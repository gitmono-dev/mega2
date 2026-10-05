use std::sync::{Arc, atomic::Ordering};

use git_internal::{
    hash::set_hash_kind_for_test,
    internal::object::tree::{TreeItem, TreeItemMode},
};
use sea_orm::{Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    callisto::{mega_refs, push_queue, sea_orm_active_enums::PushQueueKindEnum},
    common::utils::MEGA_BRANCH_NAME,
    jupiter::{
        migration::Migrator,
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            mst2_publication_storage::{PublicationPreparation, PublicationRequest},
            native_metadata_install::tests::{
                Fault, PgCommitFaultProxy, fixture as database_fixture,
            },
            push_queue_storage::{ClaimOutcome, EnqueueOutcome, EnqueueParams},
        },
        tests::{TestSchemaGuard, test_db_config},
    },
};

const INSTANCE: &str = "6ab219b0-4275-45ba-9d7b-7b0b633018cd";
const OTHER_INSTANCE: &str = "e8bba4be-7f4f-41be-80a5-3be77e874c92";

struct Fixture {
    _schema: TestSchemaGuard,
    mono: MonoStorage,
    queue: PushQueueStorage,
    url: String,
    root: Commit,
    child: Tree,
    tip: Commit,
}

fn storage(connection: DatabaseConnection) -> MonoStorage {
    MonoStorage {
        base: BaseStorage::new(Arc::new(connection)),
    }
}

async fn fixture(ready: bool) -> Fixture {
    let (first, _second, schema, url) = database_fixture().await;
    let mono = storage(first);
    // Git's empty tree is a real object; no placeholder blob or absent child
    // object is needed to make a complete root-object fixture.
    let child = Tree {
        id: ObjectHash::from_type_and_data_for_kind(HashKind::Sha1, ObjectType::Tree, &[]).unwrap(),
        tree_items: vec![],
    };
    let tree = Tree::from_tree_items_with_kind(
        HashKind::Sha1,
        vec![TreeItem::new(
            TreeItemMode::Tree,
            child.id,
            "project".to_owned(),
        )],
    )
    .unwrap();
    let root = Commit::from_tree_id_with_kind(HashKind::Sha1, tree.id, vec![], "\nroot").unwrap();
    let tip =
        Commit::from_tree_id_with_kind(HashKind::Sha1, child.id, vec![], "\npath tip").unwrap();
    mono.save_mega_trees(vec![child.clone(), tree.clone()], root.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![root.clone(), tip.clone()], None)
        .await
        .unwrap();
    for (path, commit) in [("/", &root), ("/project", &tip)] {
        mono.save_refs(
            mega_refs::Model::new(
                path,
                MEGA_BRANCH_NAME.to_owned(),
                commit.id.to_string(),
                commit.tree_id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
    }
    mono.initialize_native_publication(INSTANCE).await.unwrap();
    let queue = PushQueueStorage::new(mono.base.clone()).with_native_publication(true);
    let mut fixture = Fixture {
        _schema: schema,
        mono,
        queue,
        url,
        root,
        child,
        tip,
    };
    if ready {
        publish(&mut fixture, None).await;
    }
    fixture
}

async fn publish(fixture: &mut Fixture, next_root: Option<&Commit>) {
    let next = Commit::from_tree_id_with_kind(
        HashKind::Sha1,
        fixture.child.id,
        vec![fixture.tip.id],
        &format!("\npath publication {}", uuid::Uuid::new_v4()),
    )
    .unwrap();
    fixture
        .mono
        .save_mega_commits(vec![next.clone()], None)
        .await
        .unwrap();
    let operation = uuid::Uuid::new_v4().to_string();
    let old = fixture.tip.id.to_string();
    let new = next.id.to_string();
    let EnqueueOutcome::Inserted { id } = fixture
        .queue
        .enqueue_atomic(EnqueueParams {
            kind: PushQueueKindEnum::Push,
            operation_id: &operation,
            path: "/project",
            old_id: &old,
            new_id: &new,
            requester: Some("alice"),
            payload: serde_json::json!({"n":1,"commits":[new]}),
        })
        .await
        .unwrap()
    else {
        panic!("fresh queue operation")
    };
    assert_eq!(
        fixture.queue.claim_for_execution(id).await.unwrap(),
        ClaimOutcome::Claimed
    );
    let row: push_queue::Model = fixture.queue.get_by_id(id).await.unwrap().unwrap();
    let txn = fixture.mono.get_connection().begin().await.unwrap();
    let PublicationPreparation::Prepared(origin) = fixture
        .mono
        .begin_publication_in_txn(&txn, PublicationRequest::from_trunk_queue(&row).unwrap())
        .await
        .unwrap()
    else {
        panic!("fresh publication")
    };
    let native = fixture
        .mono
        .reserve_native_publication_in_txn(&txn, &row, INSTANCE)
        .await
        .unwrap();
    let root = next_root.unwrap_or(&fixture.root);
    assert!(
        fixture
            .mono
            .cas_update_root_main_ref_in_txn(
                &txn,
                row.expected_commit_hash.as_deref(),
                row.expected_tree_hash.as_deref(),
                &root.id.to_string(),
                &root.tree_id.to_string(),
            )
            .await
            .unwrap()
    );
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "UPDATE mega_refs SET ref_commit_hash=$1 WHERE path='/project' AND ref_name=$2 AND is_cl=false",
        [new.clone().into(), MEGA_BRANCH_NAME.into()],
    )).await.unwrap();
    let committed = fixture
        .mono
        .record_publication_in_txn(
            &txn,
            origin,
            row.expected_commit_hash.as_deref().unwrap(),
            &new,
        )
        .await
        .unwrap();
    fixture
        .mono
        .record_native_publication_in_txn(&txn, native, &committed)
        .await
        .unwrap();
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE push_queue SET status='Done',landed_commit_id=$1 WHERE id=$2",
        [new.into(), id.into()],
    ))
    .await
    .unwrap();
    txn.commit().await.unwrap();
    fixture.tip = next;
    if let Some(root) = next_root {
        fixture.root = root.clone();
    }
}

async fn source_count(connection: &DatabaseConnection) -> i64 {
    connection
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*) FROM mst2_native_source_identity",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

#[tokio::test]
async fn native_identity_requires_explicit_bootstrap_and_actual_ready_publication() {
    let mut fixture = fixture(false).await;
    assert!(matches!(
        fixture
            .mono
            .attest_native_source_identity(INSTANCE)
            .await
            .unwrap_err(),
        NativeSourceIdentityError::Publication(_)
    ));
    assert!(
        fixture
            .mono
            .bootstrap_native_source_identity(INSTANCE)
            .await
            .is_err()
    );
    assert_eq!(source_count(fixture.mono.get_connection()).await, 0);
    publish(&mut fixture, None).await;
    assert!(matches!(
        fixture
            .mono
            .attest_native_source_identity(INSTANCE)
            .await
            .unwrap_err(),
        NativeSourceIdentityError::Integrity(_)
    ));
    assert_eq!(source_count(fixture.mono.get_connection()).await, 0);
    assert!(
        fixture
            .mono
            .bootstrap_native_source_identity(OTHER_INSTANCE)
            .await
            .is_err()
    );
    assert_eq!(source_count(fixture.mono.get_connection()).await, 0);
    fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    assert!(
        fixture
            .mono
            .attest_native_source_identity(OTHER_INSTANCE)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn native_source_bootstrap_race_and_connection_restart_keep_one_persisted_uuid() {
    let fixture = fixture(true).await;
    let reopened = storage(Database::connect(fixture.url.clone()).await.unwrap());
    let (first, second, third) = tokio::join!(
        fixture.mono.bootstrap_native_source_identity(INSTANCE),
        reopened.bootstrap_native_source_identity(INSTANCE),
        fixture.mono.bootstrap_native_source_identity(INSTANCE),
    );
    let first = first.unwrap();
    assert_eq!(second.unwrap(), first);
    assert_eq!(third.unwrap(), first);
    assert_ne!(first, INSTANCE);
    let uuid = uuid::Uuid::parse_str(&first).unwrap();
    assert_eq!(uuid.get_version_num(), 4);
    assert_eq!(source_count(fixture.mono.get_connection()).await, 1);
    drop(reopened);
    let reopened = storage(Database::connect(fixture.url.clone()).await.unwrap());
    assert_eq!(
        reopened
            .bootstrap_native_source_identity(INSTANCE)
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        reopened
            .attest_native_source_identity(INSTANCE)
            .await
            .unwrap()
            .source()
            .source_id(),
        first
    );
}

#[tokio::test]
async fn native_identity_uses_codec_canonical_source_and_empty_namespace_binding_root() {
    let fixture = fixture(true).await;
    let source_id = fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    let identity = fixture
        .mono
        .attest_native_source_identity(INSTANCE)
        .await
        .unwrap();
    let expected = SourceSnapshot::new(
        source_id,
        "/".into(),
        fixture.root.id.to_string(),
        fixture.root.tree_id.to_string(),
    )
    .unwrap();
    assert_eq!(identity.source(), &expected);
    assert_eq!(
        SourceSnapshot::decode(&identity.source().encode()).unwrap(),
        expected
    );
    assert_eq!(
        identity.namespace(),
        &NamespaceView::new(INSTANCE.into(), expected, radix::empty_root(), None).unwrap()
    );
    assert_eq!(
        NamespaceView::decode(&identity.namespace().encode()).unwrap(),
        *identity.namespace()
    );
    assert_eq!(identity.namespace().native().id(), identity.source().id());
    assert_eq!(identity.publication_sequence(), 1);
    assert_eq!(identity.publication_epoch(), 1);
    assert!(identity.publication_certificate() > 0);
}

#[tokio::test]
async fn native_identity_publication_advance_keeps_source_uuid_and_fixed_old_identity() {
    let mut fixture = fixture(true).await;
    fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    let first = fixture
        .mono
        .attest_native_source_identity(INSTANCE)
        .await
        .unwrap();
    publish(&mut fixture, None).await;
    let same = fixture
        .mono
        .attest_native_source_identity(INSTANCE)
        .await
        .unwrap();
    assert_eq!(same.source(), first.source());
    assert_eq!(same.namespace().id(), first.namespace().id());
    assert_eq!(
        same.publication_sequence(),
        first.publication_sequence() + 1
    );
    assert_ne!(
        same.publication_certificate(),
        first.publication_certificate()
    );
    let tree = Tree::from_tree_items_with_kind(
        HashKind::Sha1,
        vec![
            TreeItem::new(TreeItemMode::Tree, fixture.child.id, "another".into()),
            TreeItem::new(TreeItemMode::Tree, fixture.child.id, "project".into()),
        ],
    )
    .unwrap();
    let root = Commit::from_tree_id_with_kind(
        HashKind::Sha1,
        tree.id,
        vec![fixture.root.id],
        "\nnew native root",
    )
    .unwrap();
    fixture
        .mono
        .save_mega_trees(vec![tree], root.id, None)
        .await
        .unwrap();
    fixture
        .mono
        .save_mega_commits(vec![root.clone()], None)
        .await
        .unwrap();
    publish(&mut fixture, Some(&root)).await;
    let changed = fixture
        .mono
        .attest_native_source_identity(INSTANCE)
        .await
        .unwrap();
    assert_eq!(changed.source().source_id(), first.source().source_id());
    assert_ne!(changed.source().id(), first.source().id());
    assert_ne!(changed.namespace().id(), first.namespace().id());
    assert_eq!(first.source().commit_oid(), same.source().commit_oid());
    assert_eq!(first.publication_sequence(), 1);
}

#[tokio::test]
async fn native_identity_actual_repeatable_snapshot_cannot_mix_publication_generations() {
    let mut fixture = fixture(true).await;
    fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    let txn = fixture
        .mono
        .get_connection()
        .begin_with_config(
            Some(IsolationLevel::RepeatableRead),
            Some(AccessMode::ReadOnly),
        )
        .await
        .unwrap();
    let first = fixture
        .mono
        .attest_native_source_in_snapshot(&txn, INSTANCE)
        .await
        .unwrap();
    publish(&mut fixture, None).await;
    let repeated = fixture
        .mono
        .attest_native_source_in_snapshot(&txn, INSTANCE)
        .await
        .unwrap();
    assert_eq!(
        repeated.publication_sequence(),
        first.publication_sequence()
    );
    assert_eq!(
        repeated.publication_certificate(),
        first.publication_certificate()
    );
    assert_eq!(repeated.source(), first.source());
    txn.commit().await.unwrap();
    let current = fixture
        .mono
        .attest_native_source_identity(INSTANCE)
        .await
        .unwrap();
    assert_eq!(
        current.publication_sequence(),
        first.publication_sequence() + 1
    );
}

#[tokio::test]
async fn native_identity_actual_commit_corruption_and_malformed_fields_fail_closed() {
    let fixture = fixture(true).await;
    fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    fixture
        .mono
        .attest_native_source_identity(INSTANCE)
        .await
        .unwrap();
    for modification in [
        "content=content || 'corruption'",
        "author=NULL",
        "committer=NULL",
        "content=NULL",
        "author='author invalid signature'",
        "committer='author wrong role'",
        "parents_id='{}'::jsonb",
        "parents_id='[7]'::jsonb",
        "parents_id='[\"invalid\"]'::jsonb",
        "author=author || chr(10)",
        "content=repeat('x',16777217)",
        "parents_id=(SELECT jsonb_agg('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'::text) FROM generate_series(1,4097))",
    ] {
        let txn = fixture.mono.get_connection().begin().await.unwrap();
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!("UPDATE mega_commit SET {modification} WHERE commit_id=$1"),
            [fixture.root.id.to_string().into()],
        ))
        .await
        .unwrap();
        assert!(
            fixture
                .mono
                .attest_native_source_in_snapshot(&txn, INSTANCE)
                .await
                .is_err(),
            "accepted malformed stored commit: {modification}"
        );
        txn.rollback().await.unwrap();
    }
    assert!(
        fixture
            .mono
            .attest_native_source_identity(INSTANCE)
            .await
            .is_ok()
    );
    let txn = fixture.mono.get_connection().begin().await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM mega_commit WHERE commit_id=$1",
        [fixture.root.id.to_string().into()],
    ))
    .await
    .unwrap();
    assert!(
        fixture
            .mono
            .attest_native_source_in_snapshot(&txn, INSTANCE)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn native_identity_actual_root_tree_missing_corrupt_or_mismatched_is_rejected() {
    let fixture = fixture(true).await;
    fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    let stored = mega_tree::Entity::find()
        .filter(mega_tree::Column::TreeId.eq(fixture.root.tree_id.to_string()))
        .one(fixture.mono.get_connection())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.size, 0);
    assert!(!stored.sub_trees.is_empty());
    let baseline = fixture
        .mono
        .attest_native_source_identity(INSTANCE)
        .await
        .unwrap();
    // Tree::into_mega_model intentionally leaves this metadata at zero.
    // Changing it cannot serve as evidence for different Git object bytes.
    for size in [1, stored.sub_trees.len() as i32, i32::MAX] {
        let txn = fixture.mono.get_connection().begin().await.unwrap();
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE mega_tree SET size=$1 WHERE tree_id=$2",
            [size.into(), fixture.root.tree_id.to_string().into()],
        ))
        .await
        .unwrap();
        let actual = fixture
            .mono
            .attest_native_source_in_snapshot(&txn, INSTANCE)
            .await
            .unwrap();
        assert_eq!(actual.source(), baseline.source());
        assert_eq!(actual.namespace(), baseline.namespace());
        assert_eq!(
            actual.publication_sequence(),
            baseline.publication_sequence()
        );
        assert_eq!(actual.publication_epoch(), baseline.publication_epoch());
        assert_eq!(
            actual.publication_certificate(),
            baseline.publication_certificate()
        );
        txn.rollback().await.unwrap();
    }
    for modification in [
        "sub_trees=set_byte(sub_trees,0,49)",
        "sub_trees=decode(repeat('41',16777217),'hex')",
    ] {
        let txn = fixture.mono.get_connection().begin().await.unwrap();
        txn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!("UPDATE mega_tree SET {modification} WHERE tree_id=$1"),
            [fixture.root.tree_id.to_string().into()],
        ))
        .await
        .unwrap();
        assert!(
            fixture
                .mono
                .attest_native_source_in_snapshot(&txn, INSTANCE)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
    }
    let txn = fixture.mono.get_connection().begin().await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM mega_tree WHERE tree_id=$1",
        [fixture.root.tree_id.to_string().into()],
    ))
    .await
    .unwrap();
    assert!(
        fixture
            .mono
            .attest_native_source_in_snapshot(&txn, INSTANCE)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    // An actual publication can be structurally complete while its root ref's
    // tree does not belong to the persisted commit. Attestation must reject it.
    let mut inconsistent = fixture;
    let wrong = Commit {
        tree_id: inconsistent.child.id,
        ..inconsistent.root.clone()
    };
    publish(&mut inconsistent, Some(&wrong)).await;
    assert!(matches!(
        inconsistent
            .mono
            .attest_native_source_identity(INSTANCE)
            .await
            .unwrap_err(),
        NativeSourceIdentityError::Integrity(_)
    ));
}

#[tokio::test]
async fn native_identity_malformed_tree_with_matching_typed_hash_is_not_attested() {
    let mut fixture = fixture(true).await;
    fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    let bytes = b"40000 missing-nul-and-object-id";
    let id =
        ObjectHash::from_type_and_data_for_kind(HashKind::Sha1, ObjectType::Tree, bytes).unwrap();
    let root = Commit::from_tree_id_with_kind(
        HashKind::Sha1,
        id,
        vec![fixture.root.id],
        "\nmalformed native tree",
    )
    .unwrap();
    fixture
        .mono
        .save_mega_commits(vec![root.clone()], None)
        .await
        .unwrap();
    fixture.mono.get_connection().execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mega_tree(id,tree_id,sub_trees,size,created_at,pack_id,pack_offset,commit_id)
         VALUES((SELECT COALESCE(max(id),0)+1 FROM mega_tree),$1,$2,$3,now(),'',0,$4)",
        [id.to_string().into(), bytes.to_vec().into(), (bytes.len() as i32).into(), root.id.to_string().into()],
    )).await.unwrap();
    publish(&mut fixture, Some(&root)).await;
    assert!(matches!(
        fixture
            .mono
            .attest_native_source_identity(INSTANCE)
            .await
            .unwrap_err(),
        NativeSourceIdentityError::Integrity(_)
    ));
}

#[tokio::test]
async fn native_source_identity_ddl_is_immutable_and_rejects_invalid_bindings() {
    let fixture = fixture(true).await;
    for sql in [
        "INSERT INTO mst2_native_source_identity(singleton,source_id,scope_path) VALUES(2,gen_random_uuid(),'/')",
        "INSERT INTO mst2_native_source_identity(singleton,source_id,scope_path) VALUES(1,'00000000-0000-0000-0000-000000000000','/')",
        "INSERT INTO mst2_native_source_identity(singleton,source_id,scope_path) VALUES(1,gen_random_uuid(),'/project')",
    ] {
        assert!(
            fixture
                .mono
                .get_connection()
                .execute_unprepared(sql)
                .await
                .is_err()
        );
    }
    assert_eq!(source_count(fixture.mono.get_connection()).await, 0);
    let id = fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    for sql in [
        "UPDATE mst2_native_source_identity SET source_id=gen_random_uuid()",
        "UPDATE mst2_native_source_identity SET scope_path='/'",
        "DELETE FROM mst2_native_source_identity",
        "TRUNCATE mst2_native_source_identity",
        "INSERT INTO mst2_native_source_identity(singleton,source_id,scope_path) VALUES(1,gen_random_uuid(),'/')",
    ] {
        assert!(
            fixture
                .mono
                .get_connection()
                .execute_unprepared(sql)
                .await
                .is_err()
        );
        assert_eq!(source_count(fixture.mono.get_connection()).await, 1);
    }
    assert_eq!(
        fixture
            .mono
            .bootstrap_native_source_identity(INSTANCE)
            .await
            .unwrap(),
        id
    );
}

#[tokio::test]
async fn native_source_identity_forward_migration_never_invents_source_uuid() {
    let temp = tempfile::tempdir().unwrap();
    let (config, _schema) = test_db_config(temp.path()).await;
    let db = Database::connect(config.db_url).await.unwrap();
    let total = Migrator::migrations().len() as u32;
    Migrator::up(&db, Some(total - 1)).await.unwrap();
    assert!(
        db.query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT * FROM mst2_native_source_identity"
        ))
        .await
        .is_err()
    );
    Migrator::up(&db, None).await.unwrap();
    assert_eq!(source_count(&db).await, 0);
}

async fn bootstrap_fault(fault: Fault, committed: bool) {
    let fixture = fixture(true).await;
    let proxy = PgCommitFaultProxy::start(&fixture.url, fault).await;
    let mut options = sea_orm::ConnectOptions::new(proxy.url.clone());
    options.min_connections(1).max_connections(1);
    let proxied = storage(Database::connect(options).await.unwrap());
    proxy.armed.store(true, Ordering::SeqCst);
    assert!(matches!(
        proxied
            .bootstrap_native_source_identity(INSTANCE)
            .await
            .unwrap_err(),
        NativeSourceIdentityError::BootstrapUncertain(_)
    ));
    proxy.wait_for_fault().await;
    assert_eq!(proxy.commit_observed.load(Ordering::SeqCst), committed);
    assert_eq!(
        source_count(fixture.mono.get_connection()).await,
        i64::from(committed)
    );
    let durable = if committed {
        Some(
            fixture
                .mono
                .attest_native_source_identity(INSTANCE)
                .await
                .unwrap()
                .source()
                .source_id()
                .to_owned(),
        )
    } else {
        None
    };
    let retried = fixture
        .mono
        .bootstrap_native_source_identity(INSTANCE)
        .await
        .unwrap();
    if let Some(durable) = durable {
        assert_eq!(retried, durable);
    }
    assert_eq!(source_count(fixture.mono.get_connection()).await, 1);
    assert_eq!(
        fixture
            .mono
            .bootstrap_native_source_identity(INSTANCE)
            .await
            .unwrap(),
        retried
    );
}

#[tokio::test]
async fn native_source_bootstrap_actual_disconnect_before_commit_has_no_false_identity() {
    bootstrap_fault(Fault::BeforeCommit, false).await;
}

#[tokio::test]
async fn native_source_bootstrap_actual_commit_reply_loss_reuses_one_durable_identity() {
    bootstrap_fault(Fault::AfterCommit, true).await;
}

#[tokio::test]
async fn native_identity_sha256_backend_is_rejected_before_bootstrap_or_attestation() {
    let fixture = fixture(true).await;
    let _kind = set_hash_kind_for_test(HashKind::Sha256);
    assert!(matches!(
        fixture
            .mono
            .bootstrap_native_source_identity(INSTANCE)
            .await
            .unwrap_err(),
        NativeSourceIdentityError::UnsupportedObjectFormat
    ));
    assert!(matches!(
        fixture
            .mono
            .attest_native_source_identity(INSTANCE)
            .await
            .unwrap_err(),
        NativeSourceIdentityError::UnsupportedObjectFormat
    ));
    assert_eq!(source_count(fixture.mono.get_connection()).await, 0);
}
