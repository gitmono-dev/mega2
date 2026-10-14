use std::str::FromStr;

use git_internal::{
    hash::ObjectHash,
    internal::object::{
        commit::Commit,
        tree::{Tree, TreeItem, TreeItemMode},
    },
};

use super::{test_support::*, *};
use crate::{
    callisto::sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
    ceres::pack::materialize,
    config::{PushPolicy, testing::isolated_config},
    jupiter::{
        service::push_queue_service::{
            EnqueueRequest, ExecuteOutcome, ExecuteRequest, PushExecContext, PushPayload,
            push_operation_id,
        },
        storage::push_queue_storage::{ClaimOutcome, EnqueueOutcome},
        tests::test_storage_with_config,
    },
};

async fn tp11_storage(temp: &std::path::Path) -> Storage {
    let mut config = isolated_config(temp.join("config"));
    config.monorepo.push_policy = PushPolicy::Trunk;
    let storage = test_storage_with_config(temp, config).await;
    crate::jupiter::tests::with_test_vault(storage, temp).await
}

async fn nested_queue_fixture(
    path: &str,
    link: &str,
    dir: &str,
) -> (
    tempfile::TempDir,
    Storage,
    MonoApiService,
    mega_cl::Model,
    String,
) {
    let temp = tempfile::tempdir().unwrap();
    let storage = tp11_storage(temp.path()).await;
    let service = test_service(&storage);
    let mono = storage.mono_storage();
    let child = Tree::from_tree_items(vec![blob_item(
        "x.txt",
        "dddddddddddddddddddddddddddddddddddddddd",
    )])
    .expect("child");
    let path_tree_hash = child.id.to_string();
    mono.save_mega_trees(
        vec![child.clone()],
        ObjectHash::from_str(&"1".repeat(40)).unwrap(),
        None,
    )
    .await
    .unwrap();
    let old_tree = Tree::from_tree_items(vec![
        blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        TreeItem::new(TreeItemMode::Tree, child.id, dir.to_string()),
    ])
    .expect("old tree");
    let new_tree = Tree::from_tree_items(vec![blob_item(
        "queued.txt",
        "cccccccccccccccccccccccccccccccccccccccc",
    )])
    .expect("new tree");
    let old_commit = Commit::from_tree_id(old_tree.id, vec![], "base");
    let new_commit = Commit::from_tree_id(new_tree.id, vec![old_commit.id], "cl tip");
    mono.save_mega_trees(
        vec![old_tree.clone(), new_tree.clone()],
        old_commit.id,
        None,
    )
    .await
    .unwrap();
    mono.save_mega_commits(vec![old_commit.clone(), new_commit.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &old_tree, &old_commit.id.to_string()).await;
    mono.save_refs(
        mega_refs::Model {
            id: crate::callisto::entity_ext::generate_id(),
            path: path.to_string(),
            ref_name: MEGA_BRANCH_NAME.to_string(),
            ref_commit_hash: old_commit.id.to_string(),
            ref_tree_hash: path_tree_hash,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            is_cl: false,
        },
        None,
    )
    .await
    .unwrap();
    mono.save_or_update_cl_ref(
        path,
        &format!("refs/cl/{link}"),
        &new_commit.id.to_string(),
        &new_tree.id.to_string(),
    )
    .await
    .unwrap();
    let cl = storage
        .cl_storage()
        .new_cl_model(
            path,
            link,
            "queue merge",
            "main",
            &old_commit.id.to_string(),
            &new_commit.id.to_string(),
            "gate-tester",
        )
        .await
        .unwrap();
    (temp, storage, service, cl, old_commit.id.to_string())
}

async fn poison_main_tree(storage: &Storage, path: &str) -> String {
    let mut row = storage
        .mono_storage()
        .get_main_ref(path)
        .await
        .unwrap()
        .unwrap();
    let tip = row.ref_commit_hash.clone();
    row.ref_tree_hash = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into();
    storage.mono_storage().update_ref(row, None).await.unwrap();
    tip
}

#[tokio::test]
async fn tp11_merge_stale_tree_hash_refuses_and_tombstones() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, service, cl, stale_tip) =
        nested_queue_fixture("/p11", "TP11M1", "p11").await;
    poison_main_tree(&storage, "/p11").await;
    let err = service
        .merge_cl("gate-tester", "gate-tester", cl)
        .await
        .expect_err("stale tree hash must refuse merge");
    assert!(err.to_string().contains("stale materialized"), "{err}");
    assert!(
        storage
            .mono_storage()
            .get_main_ref("/p11")
            .await
            .unwrap()
            .is_none()
    );
    let tomb = storage
        .mono_storage()
        .get_tombstone("/p11", MEGA_BRANCH_NAME)
        .await
        .unwrap()
        .expect("tombstone");
    assert_eq!(tomb.last_commit_hash, stale_tip);
    assert!(
        storage
            .push_queue_service
            .metrics()
            .tree_hash_assert_failures
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    );
}

#[tokio::test]
async fn tp11_push_stale_tree_hash_refuses_and_tombstones() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _service, _cl, stale_tip) =
        nested_queue_fixture("/p11p", "TP11P1", "p11p").await;
    poison_main_tree(&storage, "/p11p").await;
    let outcome = storage
        .push_queue_service
        .enqueue(EnqueueRequest {
            kind: PushQueueKindEnum::Push,
            operation_id: push_operation_id(&stale_tip, &"c".repeat(40)),
            path: "/p11p".into(),
            old_id: stale_tip.clone(),
            new_id: "c".repeat(40),
            requester: None,
            payload: serde_json::json!({}),
            ref_name: Some(MEGA_BRANCH_NAME.into()),
            is_delete: false,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Inserted { id } = outcome else {
        panic!("insert push {outcome:?}");
    };
    assert_eq!(
        storage
            .push_queue_service
            .storage()
            .claim_for_execution(id)
            .await
            .unwrap(),
        ClaimOutcome::Claimed
    );
    let exec = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id,
                ..Default::default()
            },
            None,
            None,
            None,
        )
        .await
        .unwrap();
    match exec {
        ExecuteOutcome::Failed {
            failure, message, ..
        } => {
            assert_eq!(failure, "Conflict");
            assert!(message.contains("advertise"), "{message}");
            assert!(message.contains("fetch"), "{message}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        storage
            .mono_storage()
            .get_main_ref("/p11p")
            .await
            .unwrap()
            .is_none()
    );
    let tomb = storage
        .mono_storage()
        .get_tombstone("/p11p", MEGA_BRANCH_NAME)
        .await
        .unwrap()
        .expect("tombstone");
    assert_eq!(tomb.last_commit_hash, stale_tip);
    let row = storage
        .push_queue_service
        .storage()
        .get_by_id(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, PushQueueStatusEnum::Failed);
    assert!(row.pending_action.is_none());
}

#[tokio::test]
async fn tp11_merge_repair_advertise_continues_from_tombstone() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, service, cl, stale_tip) =
        nested_queue_fixture("/p11a", "TP11AD", "p11a").await;
    poison_main_tree(&storage, "/p11a").await;
    service
        .merge_cl("gate-tester", "gate-tester", cl)
        .await
        .expect_err("stale");
    let head = crate::ceres::code_edit::utils::create_repo_commit(&storage, "/p11a")
        .await
        .unwrap();
    assert_ne!(head, ZERO_ID);
    let commit = storage
        .mono_storage()
        .get_commit_by_hash(&head)
        .await
        .unwrap()
        .expect("revived commit");
    let parents: Vec<String> = serde_json::from_value(commit.parents_id).unwrap();
    assert_eq!(parents, vec![stale_tip]);
}

#[tokio::test]
async fn tp11_assertion_precedes_conflict_recheck() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, service, cl, stale_tip) =
        nested_queue_fixture("/p11c", "TP11CR", "p11c").await;
    poison_main_tree(&storage, "/p11c").await;
    let txn = storage.begin_db_transaction().await.unwrap();
    let current = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    assert!(
        storage
            .cl_storage()
            .cas_update_cl_hashes_in_txn(&current, &"f".repeat(40), &current.to_hash, &txn)
            .await
            .unwrap()
    );
    txn.commit().await.unwrap();
    let err = service
        .merge_cl("gate-tester", "gate-tester", cl)
        .await
        .expect_err("stale tree must refuse before from_hash conflict");
    assert!(err.to_string().contains("stale materialized"), "got {err}");
    assert!(
        storage
            .mono_storage()
            .get_tombstone("/p11c", MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .is_some()
    );
    let rows = storage
        .push_queue_service
        .storage()
        .list_recent_finished(20)
        .await
        .unwrap();
    let row = rows
        .iter()
        .find(|r| r.operation_id == "TP11CR")
        .expect("queued merge row");
    assert_eq!(row.status, PushQueueStatusEnum::Failed);
    assert!(row.pending_action.is_none());
    assert_eq!(
        storage
            .mono_storage()
            .get_tombstone("/p11c", MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .unwrap()
            .last_commit_hash,
        stale_tip
    );
}

#[tokio::test]
async fn tp11_cl_ref_is_not_asserted() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, service, cl, _tip) =
        nested_queue_fixture("/p11cl", "TP11CL", "p11cl").await;
    let mut cl_ref = storage
        .mono_storage()
        .get_ref_by_name(&format!("refs/cl/{}", cl.link))
        .await
        .unwrap()
        .unwrap();
    cl_ref.ref_tree_hash = "ffffffffffffffffffffffffffffffffffffffff".into();
    storage
        .mono_storage()
        .update_ref(cl_ref, None)
        .await
        .unwrap();
    service
        .merge_cl("gate-tester", "gate-tester", cl.clone())
        .await
        .expect("poisoned CL ref must not trip main tree-hash assertion");
    let merged = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.status, MergeStatusEnum::Merged);
    assert!(
        storage
            .mono_storage()
            .get_tombstone("/p11cl", MEGA_BRANCH_NAME)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn tp11_reconcile_and_inspect_tombstone_stale_rows() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _service, _cl, stale_tip) =
        nested_queue_fixture("/p11r", "TP11RC", "p11r").await;
    poison_main_tree(&storage, "/p11r").await;
    let audit = storage.push_queue_service.audit().with_batch_size(8);
    let rec = audit.reconcile_once().await.unwrap();
    assert!(rec.lock_acquired);
    assert!(rec.tombstoned >= 1);
    assert!(
        storage
            .mono_storage()
            .get_main_ref("/p11r")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        storage
            .push_queue_service
            .metrics()
            .reconcile_tombstoned
            .load(std::sync::atomic::Ordering::Relaxed),
        rec.tombstoned
    );
    storage
        .mono_storage()
        .save_refs(
            mega_refs::Model::new(
                "/p11r",
                MEGA_BRANCH_NAME.to_owned(),
                stale_tip.clone(),
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
    let ins = audit.inspect_once().await.unwrap();
    assert!(ins.lock_acquired);
    assert!(ins.tombstoned >= 1);
    assert!(
        storage
            .mono_storage()
            .get_main_ref("/p11r")
            .await
            .unwrap()
            .is_none()
    );
    let snap = storage.push_queue_service.metrics_snapshot().await.unwrap();
    assert!(snap.inspect_tombstoned >= 1);
    assert!(snap.reconcile_tombstoned >= 1);
}

#[tokio::test]
async fn tp11_inspect_skips_root_and_cl_refs() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _service, cl, _) = nested_queue_fixture("/p11s", "TP11SK", "p11s").await;
    let mut cl_ref = storage
        .mono_storage()
        .get_ref_by_name(&format!("refs/cl/{}", cl.link))
        .await
        .unwrap()
        .unwrap();
    cl_ref.ref_tree_hash = "ffffffffffffffffffffffffffffffffffffffff".into();
    storage
        .mono_storage()
        .update_ref(cl_ref, None)
        .await
        .unwrap();
    let root_before = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    let audit = storage.push_queue_service.audit();
    let rec = audit.reconcile_once().await.unwrap();
    assert_eq!(rec.tombstoned, 0);
    let root_after = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root_before.ref_tree_hash, root_after.ref_tree_hash);
    assert!(
        storage
            .mono_storage()
            .get_ref_by_name(&format!("refs/cl/{}", cl.link))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        storage
            .mono_storage()
            .get_main_ref("/p11s")
            .await
            .unwrap()
            .is_some()
    );
}

async fn tp12_ctx(storage: &Storage) -> PushExecContext {
    PushExecContext {
        git_object_cache: Arc::new(GitObjectCache {
            connection: crate::jupiter::tests::test_redis_manager().await,
            prefix: String::new(),
        }),
        storage: storage.clone(),
        pre_apply_enter_barrier: None,
        pre_apply_release_barrier: None,
    }
}

async fn tp12_enqueue_claim(
    storage: &Storage,
    path: &str,
    old_id: &str,
    new_id: &str,
    payload: &PushPayload,
) -> i64 {
    let outcome = storage
        .push_queue_service
        .enqueue(EnqueueRequest {
            kind: PushQueueKindEnum::Push,
            operation_id: push_operation_id(old_id, new_id),
            path: path.into(),
            old_id: old_id.into(),
            new_id: new_id.into(),
            requester: None,
            payload: payload.to_json(),
            ref_name: Some(MEGA_BRANCH_NAME.into()),
            is_delete: false,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Inserted { id } = outcome else {
        panic!("insert push {outcome:?}");
    };
    assert_eq!(
        storage
            .push_queue_service
            .storage()
            .claim_for_execution(id)
            .await
            .unwrap(),
        ClaimOutcome::Claimed
    );
    id
}

async fn tp12_exec(storage: &Storage, id: i64) -> ExecuteOutcome {
    let ctx = tp12_ctx(storage).await;
    storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id,
                ..Default::default()
            },
            None,
            None,
            Some(&ctx),
        )
        .await
        .unwrap()
}

/// Root + `main@/{dir}` whose tree is `child` and tip is `path_commit`.
async fn tp12_path_fixture(
    dir: &str,
    path_commit_msg: &str,
) -> (
    tempfile::TempDir,
    Storage,
    Tree,
    Commit,
    Tree,
    Commit,
    String,
) {
    let temp = tempfile::tempdir().unwrap();
    let storage = tp11_storage(temp.path()).await;
    let mono = storage.mono_storage();
    let child = Tree::from_tree_items(vec![blob_item(
        "x.txt",
        "dddddddddddddddddddddddddddddddddddddddd",
    )])
    .expect("child");
    let root_tree = Tree::from_tree_items(vec![
        blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        TreeItem::new(TreeItemMode::Tree, child.id, dir.to_string()),
    ])
    .expect("root");
    let root_commit = Commit::from_tree_id(root_tree.id, vec![], "root");
    let path_commit = Commit::from_tree_id(child.id, vec![], path_commit_msg);
    mono.save_mega_trees(vec![child.clone(), root_tree.clone()], root_commit.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![root_commit.clone(), path_commit.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &root_tree, &root_commit.id.to_string()).await;
    let path = format!("/{dir}");
    mono.save_refs(
        mega_refs::Model {
            id: crate::callisto::entity_ext::generate_id(),
            path: path.clone(),
            ref_name: MEGA_BRANCH_NAME.to_string(),
            ref_commit_hash: path_commit.id.to_string(),
            ref_tree_hash: child.id.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            is_cl: false,
        },
        None,
    )
    .await
    .unwrap();
    (
        temp,
        storage,
        root_tree,
        root_commit,
        child,
        path_commit,
        path,
    )
}

#[tokio::test]
async fn tp12_n1_fast_forward_lands_client_tip() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
        tp12_path_fixture("p12n1", "path tip").await;
    let new_child = Tree::from_tree_items(vec![blob_item(
        "y.txt",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
    )])
    .unwrap();
    let new_commit = Commit::from_tree_id(new_child.id, vec![path_commit.id], "n1");
    storage
        .mono_storage()
        .save_mega_trees(vec![new_child], new_commit.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![new_commit.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![new_commit.id.to_string()],
        fork_base: Some(path_commit.id.to_string()),
        n: 1,
    };
    let id = tp12_enqueue_claim(
        &storage,
        &path,
        &path_commit.id.to_string(),
        &new_commit.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage, id).await;
    match exec {
        ExecuteOutcome::Done {
            landed_commit_id,
            root_cas_writes,
            ..
        } => {
            assert_eq!(landed_commit_id, new_commit.id.to_string());
            assert_eq!(root_cas_writes, 1);
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let pref = storage
        .mono_storage()
        .get_main_ref(&path)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pref.ref_commit_hash, new_commit.id.to_string());
}

#[tokio::test]
async fn tp14_push_continues_changed_descendant_and_skips_unchanged() {
    let _lock = materialize::lock_materialize_tests().await;
    let temp = tempfile::tempdir().unwrap();
    let storage = tp11_storage(temp.path()).await;
    let mono = storage.mono_storage();
    let foo_old = Tree::from_tree_items(vec![blob_item(
        "a.txt",
        "dddddddddddddddddddddddddddddddddddddddd",
    )])
    .unwrap();
    let foo_new = Tree::from_tree_items(vec![blob_item(
        "a.txt",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
    )])
    .unwrap();
    let keep = Tree::from_tree_items(vec![blob_item(
        "k.txt",
        "ffffffffffffffffffffffffffffffffffffffff",
    )])
    .unwrap();
    let p_old = Tree::from_tree_items(vec![
        TreeItem::new(TreeItemMode::Tree, foo_old.id, "foo".into()),
        TreeItem::new(TreeItemMode::Tree, keep.id, "keep".into()),
    ])
    .unwrap();
    let p_new = Tree::from_tree_items(vec![
        TreeItem::new(TreeItemMode::Tree, foo_new.id, "foo".into()),
        TreeItem::new(TreeItemMode::Tree, keep.id, "keep".into()),
    ])
    .unwrap();
    let root = Tree::from_tree_items(vec![
        blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        TreeItem::new(TreeItemMode::Tree, p_old.id, "p14".into()),
    ])
    .unwrap();
    let root_c = Commit::from_tree_id(root.id, vec![], "root");
    let p_c = Commit::from_tree_id(p_old.id, vec![], "p14");
    let foo_c = Commit::from_tree_id(foo_old.id, vec![], "foo");
    let keep_c = Commit::from_tree_id(keep.id, vec![], "keep");
    let push_c = Commit::from_tree_id(p_new.id, vec![p_c.id], "push p14");
    mono.save_mega_trees(
        vec![
            foo_old.clone(),
            foo_new.clone(),
            keep.clone(),
            p_old.clone(),
            p_new.clone(),
            root.clone(),
        ],
        root_c.id,
        None,
    )
    .await
    .unwrap();
    mono.save_mega_commits(
        vec![
            root_c.clone(),
            p_c.clone(),
            foo_c.clone(),
            keep_c.clone(),
            push_c.clone(),
        ],
        None,
    )
    .await
    .unwrap();
    setup_main_ref(&storage, &root, &root_c.id.to_string()).await;
    for (path, commit, tree) in [
        ("/p14", &p_c, &p_old),
        ("/p14/foo", &foo_c, &foo_old),
        ("/p14/keep", &keep_c, &keep),
    ] {
        mono.save_refs(
            mega_refs::Model {
                id: crate::callisto::entity_ext::generate_id(),
                path: path.into(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: commit.id.to_string(),
                ref_tree_hash: tree.id.to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .unwrap();
    }
    let payload = PushPayload {
        commits: vec![push_c.id.to_string()],
        fork_base: Some(p_c.id.to_string()),
        n: 1,
    };
    let id = tp12_enqueue_claim(
        &storage,
        "/p14",
        &p_c.id.to_string(),
        &push_c.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage, id).await;
    assert!(
        matches!(exec, ExecuteOutcome::Done { .. }),
        "push B3: {exec:?}"
    );

    let foo = mono.get_main_ref("/p14/foo").await.unwrap().unwrap();
    assert_eq!(foo.ref_tree_hash, foo_new.id.to_string());
    let cont = mono
        .get_commit_by_hash(&foo.ref_commit_hash)
        .await
        .unwrap()
        .unwrap();
    let parents: Vec<String> = serde_json::from_value(cont.parents_id).unwrap();
    assert_eq!(parents, vec![foo_c.id.to_string()]);
    let cont_msg = cont.content.as_deref().unwrap_or("");
    assert!(
        cont_msg.contains("push p14") || cont_msg.contains("gpgsig"),
        "N=1 descendant keeps the client message: {cont_msg}"
    );
    assert_has_gpgsig(cont_msg);

    let keep_row = mono.get_main_ref("/p14/keep").await.unwrap().unwrap();
    assert_eq!(keep_row.ref_commit_hash, keep_c.id.to_string());
}

#[tokio::test]
async fn tp12_n2_known_objects_still_squash_and_done_replay() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
        tp12_path_fixture("p12n2", "path tip").await;
    let mid_tree = Tree::from_tree_items(vec![blob_item(
        "m.txt",
        "1111111111111111111111111111111111111111",
    )])
    .unwrap();
    let tip_tree = Tree::from_tree_items(vec![blob_item(
        "t.txt",
        "2222222222222222222222222222222222222222",
    )])
    .unwrap();
    let mid = Commit::from_tree_id(mid_tree.id, vec![path_commit.id], "mid");
    let tip = Commit::from_tree_id(tip_tree.id, vec![mid.id], "tip");
    storage
        .mono_storage()
        .save_mega_trees(vec![mid_tree, tip_tree], tip.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![mid.clone(), tip.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![tip.id.to_string(), mid.id.to_string()],
        fork_base: Some(path_commit.id.to_string()),
        n: 2,
    };
    let old_id = path_commit.id.to_string();
    let new_id = tip.id.to_string();
    let id = tp12_enqueue_claim(&storage, &path, &old_id, &new_id, &payload).await;
    let exec = tp12_exec(&storage, id).await;
    let ExecuteOutcome::Done {
        landed_commit_id, ..
    } = exec
    else {
        panic!("expected Done, got {exec:?}");
    };
    assert_ne!(landed_commit_id, new_id, "N>1 must squash, not FF");
    let squash = storage
        .mono_storage()
        .get_commit_by_hash(&landed_commit_id)
        .await
        .unwrap()
        .expect("squash");
    let parents: Vec<String> = serde_json::from_value(squash.parents_id).unwrap();
    assert_eq!(parents, vec![old_id.clone()]);
    let pref = storage
        .mono_storage()
        .get_main_ref(&path)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pref.ref_commit_hash, landed_commit_id);
    for cid in &payload.commits {
        assert!(
            storage
                .mono_storage()
                .get_commit_by_hash(cid)
                .await
                .unwrap()
                .is_some(),
            "payload must reconstruct the original chain"
        );
    }

    let replay = storage
        .push_queue_service
        .enqueue(EnqueueRequest {
            kind: PushQueueKindEnum::Push,
            operation_id: push_operation_id(&old_id, &new_id),
            path: path.clone(),
            old_id: old_id.clone(),
            new_id: new_id.clone(),
            requester: None,
            payload: payload.to_json(),
            ref_name: Some(MEGA_BRANCH_NAME.into()),
            is_delete: false,
        })
        .await
        .unwrap();
    match replay {
        EnqueueOutcome::Replay {
            id: rid,
            landed_commit_id: replayed,
        } => {
            let row = storage
                .push_queue_service
                .storage()
                .get_by_id(rid)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.status, PushQueueStatusEnum::Done);
            assert_eq!(replayed.as_deref(), Some(landed_commit_id.as_str()));
        }
        other => panic!("expected Done replay, got {other:?}"),
    }
}

#[tokio::test]
async fn tp12_n0_matching_tip_is_noop_done() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
        tp12_path_fixture("p12n0", "path tip").await;
    let tip = path_commit.id.to_string();
    let payload = PushPayload {
        commits: vec![tip.clone()],
        fork_base: Some(tip.clone()),
        n: 0,
    };
    let root_before = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    let id = tp12_enqueue_claim(&storage, &path, &tip, &tip, &payload).await;
    let exec = tp12_exec(&storage, id).await;
    match exec {
        ExecuteOutcome::Done {
            landed_commit_id,
            root_cas_writes,
            ..
        } => {
            assert_eq!(landed_commit_id, tip);
            assert_eq!(root_cas_writes, 1);
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let root_after = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root_before.ref_commit_hash, root_after.ref_commit_hash);
    assert_eq!(root_before.ref_tree_hash, root_after.ref_tree_hash);
    let pref = storage
        .mono_storage()
        .get_main_ref(&path)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pref.ref_commit_hash, tip);
}

#[tokio::test]
async fn tp12_net_zero_same_tree_cas_once() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _root_tree, _root_c, child, path_commit, path) =
        tp12_path_fixture("p12nz", "path tip").await;
    let new_commit = Commit::from_tree_id(child.id, vec![path_commit.id], "same tree n1");
    storage
        .mono_storage()
        .save_mega_commits(vec![new_commit.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![new_commit.id.to_string()],
        fork_base: Some(path_commit.id.to_string()),
        n: 1,
    };
    let root_before = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    let id = tp12_enqueue_claim(
        &storage,
        &path,
        &path_commit.id.to_string(),
        &new_commit.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage, id).await;
    match exec {
        ExecuteOutcome::Done {
            landed_commit_id,
            root_cas_writes,
            ..
        } => {
            assert_eq!(landed_commit_id, new_commit.id.to_string());
            assert_eq!(root_cas_writes, 1);
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let root_after = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root_before.ref_commit_hash, root_after.ref_commit_hash);
    assert_eq!(root_before.ref_tree_hash, root_after.ref_tree_hash);
    let pref = storage
        .mono_storage()
        .get_main_ref(&path)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pref.ref_commit_hash, new_commit.id.to_string());
    assert_eq!(pref.ref_tree_hash, child.id.to_string());
}

#[tokio::test]
async fn tp12_create_n1_and_n2_and_multilevel() {
    let _lock = materialize::lock_materialize_tests().await;
    let temp = tempfile::tempdir().unwrap();
    let storage = tp11_storage(temp.path()).await;
    // Creations stay under a `root_dirs` root: B3 re-classifies a true
    // creation (plan-20260923 FU-10), so a new top-level path is refused.
    let keep = Tree::from_tree_items(vec![blob_item(
        ".gitkeep",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )])
    .unwrap();
    let root_c = Commit::from_tree_id(keep.id, vec![], "root");
    storage
        .mono_storage()
        .save_mega_trees(vec![keep.clone()], root_c.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![root_c.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &keep, &root_c.id.to_string()).await;

    let leaf = Tree::from_tree_items(vec![blob_item(
        "f.txt",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )])
    .unwrap();
    let n1 = Commit::from_tree_id(leaf.id, vec![], "create n1");
    storage
        .mono_storage()
        .save_mega_trees(vec![leaf.clone()], n1.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![n1.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![n1.id.to_string()],
        fork_base: Some(ZERO_ID.to_string()),
        n: 1,
    };
    let id = tp12_enqueue_claim(
        &storage,
        "/project/p12c1",
        ZERO_ID,
        &n1.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage, id).await;
    let ExecuteOutcome::Done {
        landed_commit_id, ..
    } = exec
    else {
        panic!("create n1: {exec:?}");
    };
    assert_eq!(landed_commit_id, n1.id.to_string());
    let pref = storage
        .mono_storage()
        .get_main_ref("/project/p12c1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pref.ref_commit_hash, n1.id.to_string());

    let mid_t = Tree::from_tree_items(vec![blob_item(
        "m.txt",
        "cccccccccccccccccccccccccccccccccccccccc",
    )])
    .unwrap();
    let tip_t = Tree::from_tree_items(vec![blob_item(
        "t.txt",
        "dddddddddddddddddddddddddddddddddddddddd",
    )])
    .unwrap();
    let mid = Commit::from_tree_id(mid_t.id, vec![], "c2 mid");
    let tip = Commit::from_tree_id(tip_t.id, vec![mid.id], "c2 tip");
    storage
        .mono_storage()
        .save_mega_trees(vec![mid_t, tip_t], tip.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![mid.clone(), tip.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![tip.id.to_string(), mid.id.to_string()],
        fork_base: Some(ZERO_ID.to_string()),
        n: 2,
    };
    let id = tp12_enqueue_claim(
        &storage,
        "/project/p12c2",
        ZERO_ID,
        &tip.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage, id).await;
    let ExecuteOutcome::Done {
        landed_commit_id, ..
    } = exec
    else {
        panic!("create n2: {exec:?}");
    };
    assert_ne!(landed_commit_id, tip.id.to_string());
    let squash = storage
        .mono_storage()
        .get_commit_by_hash(&landed_commit_id)
        .await
        .unwrap()
        .unwrap();
    let parents: Vec<String> = serde_json::from_value(squash.parents_id).unwrap();
    assert!(parents.is_empty(), "create N>1 is parentless");
    for cid in &payload.commits {
        assert!(
            storage
                .mono_storage()
                .get_commit_by_hash(cid)
                .await
                .unwrap()
                .is_some()
        );
    }

    let deep = Tree::from_tree_items(vec![blob_item(
        "z.txt",
        "ffffffffffffffffffffffffffffffffffffffff",
    )])
    .unwrap();
    let deep_c = Commit::from_tree_id(deep.id, vec![], "deep");
    storage
        .mono_storage()
        .save_mega_trees(vec![deep.clone()], deep_c.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![deep_c.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![deep_c.id.to_string()],
        fork_base: Some(ZERO_ID.to_string()),
        n: 1,
    };
    let id = tp12_enqueue_claim(
        &storage,
        "/project/a/b/c",
        ZERO_ID,
        &deep_c.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage, id).await;
    let ExecuteOutcome::Done {
        landed_commit_id, ..
    } = exec
    else {
        panic!("multilevel: {exec:?}");
    };
    assert_eq!(landed_commit_id, deep_c.id.to_string());
    let pref = storage
        .mono_storage()
        .get_main_ref("/project/a/b/c")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pref.ref_commit_hash, deep_c.id.to_string());
}

#[tokio::test]
async fn tp12_tombstone_and_wait_materialize_and_assertions() {
    let _lock = materialize::lock_materialize_tests().await;
    let temp = tempfile::tempdir().unwrap();
    let storage = tp11_storage(temp.path()).await;
    let keep = Tree::from_tree_items(vec![blob_item(
        ".gitkeep",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )])
    .unwrap();
    let root_c = Commit::from_tree_id(keep.id, vec![], "root");
    storage
        .mono_storage()
        .save_mega_trees(vec![keep.clone()], root_c.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![root_c.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &keep, &root_c.id.to_string()).await;

    let leaf = Tree::from_tree_items(vec![blob_item(
        "f.txt",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )])
    .unwrap();
    let n1 = Commit::from_tree_id(leaf.id, vec![], "create");
    storage
        .mono_storage()
        .save_mega_trees(vec![leaf.clone()], n1.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![n1.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![n1.id.to_string()],
        fork_base: Some(ZERO_ID.to_string()),
        n: 1,
    };
    let id = tp12_enqueue_claim(&storage, "/p12tb", ZERO_ID, &n1.id.to_string(), &payload).await;
    storage
        .mono_storage()
        .upsert_tombstone("/p12tb", MEGA_BRANCH_NAME, &"c".repeat(40), &"d".repeat(40))
        .await
        .unwrap();
    let exec = tp12_exec(&storage, id).await;
    match exec {
        ExecuteOutcome::Failed {
            failure, message, ..
        } => {
            assert_eq!(failure, "Conflict");
            assert!(message.contains("advertise"), "{message}");
            assert!(message.contains("fetch"), "{message}");
        }
        other => panic!("tombstone race: {other:?}"),
    }

    let (_temp2, storage2, _rt, _rc, child, path_commit, path) =
        tp12_path_fixture("p12wm", "synth").await;
    let payload = PushPayload {
        commits: vec![n1.id.to_string()],
        fork_base: Some(ZERO_ID.to_string()),
        n: 1,
    };
    let id = tp12_enqueue_claim(&storage2, &path, ZERO_ID, &n1.id.to_string(), &payload).await;
    let exec = tp12_exec(&storage2, id).await;
    match exec {
        ExecuteOutcome::Failed { message, .. } => {
            assert!(message.contains("materialized while waiting"), "{message}");
        }
        other => panic!("wait materialize: {other:?}"),
    }
    let _ = (child, path_commit);

    let (_temp3, storage3, _rt, _rc, _child, path_commit, path) =
        tp12_path_fixture("p12as", "tip").await;
    let payload = PushPayload {
        commits: vec!["e".repeat(40)],
        fork_base: Some(path_commit.id.to_string()),
        n: 1,
    };
    let wrong_old = "f".repeat(40);
    let id = tp12_enqueue_claim(&storage3, &path, &wrong_old, &"e".repeat(40), &payload).await;
    let exec = tp12_exec(&storage3, id).await;
    match exec {
        ExecuteOutcome::Failed { message, .. } => {
            assert!(message.contains("non-fast-forward"), "{message}");
        }
        other => panic!("nff: {other:?}"),
    }

    let payload = PushPayload {
        commits: vec![path_commit.id.to_string()],
        fork_base: Some(path_commit.id.to_string()),
        n: 1,
    };
    let id = tp12_enqueue_claim(
        &storage3,
        "/missing-path",
        &path_commit.id.to_string(),
        &n1.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage3, id).await;
    match exec {
        ExecuteOutcome::Failed { message, .. } => {
            assert!(
                message.contains("missing path") || message.contains("ZERO_ID"),
                "{message}"
            );
        }
        other => panic!("missing row: {other:?}"),
    }

    let tip = path_commit.id.to_string();
    let bogus = "9".repeat(40);
    let payload = PushPayload {
        commits: vec![bogus.clone()],
        fork_base: Some(tip.clone()),
        n: 0,
    };
    let id = tp12_enqueue_claim(&storage3, &path, &bogus, &bogus, &payload).await;
    let exec = tp12_exec(&storage3, id).await;
    match exec {
        ExecuteOutcome::Failed { message, .. } => {
            assert!(
                message.contains("n=0")
                    || message.contains("new_id does not match")
                    || message.contains("invariant"),
                "{message}"
            );
        }
        other => panic!("n0 mismatch: {other:?}"),
    }
}

#[tokio::test]
async fn tp12_gap14_empty_pack_and_all_known_enqueue_b3() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _rt, _rc, _child, path_commit, path) =
        tp12_path_fixture("p12g14", "path tip").await;
    let new_child = Tree::from_tree_items(vec![blob_item(
        "g.txt",
        "3333333333333333333333333333333333333333",
    )])
    .unwrap();
    let new_commit = Commit::from_tree_id(new_child.id, vec![path_commit.id], "g14");
    storage
        .mono_storage()
        .save_mega_trees(vec![new_child], new_commit.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![new_commit.clone()], None)
        .await
        .unwrap();

    let cmd = crate::ceres::protocol::import_refs::RefCommand::new(
        path_commit.id.to_string(),
        new_commit.id.to_string(),
        MEGA_BRANCH_NAME.to_string(),
    );
    let chain = crate::ceres::pack::push_chain::PushChain::from_known_tip(
        &cmd,
        new_commit.clone(),
        &storage.mono_storage(),
        crate::ceres::merge_checker::MAX_CL_CHAIN_COMMITS,
    )
    .await
    .unwrap();
    assert_eq!(chain.ordered_commits.len(), 1);
    let payload = PushPayload::from_chain(
        &path_commit.id.to_string(),
        &new_commit.id.to_string(),
        &chain,
    );
    assert_eq!(payload.n, 1);

    let repo = tp12_monorepo(
        &storage,
        vec![cmd.clone()],
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
    );
    let built = repo.build_push_chain(&cmd).await.unwrap();
    assert!(built.is_some(), "trunk empty pack must not Noop");

    let id = tp12_enqueue_claim(
        &storage,
        &path,
        &path_commit.id.to_string(),
        &new_commit.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage, id).await;
    assert!(matches!(exec, ExecuteOutcome::Done { .. }), "{exec:?}");

    let mut pack = std::collections::HashSet::new();
    pack.insert(new_commit.id.to_string());
    let repo = tp12_monorepo(
        &storage,
        vec![cmd.clone()],
        pack,
        std::collections::HashSet::new(),
    );
    let built = repo.build_push_chain(&cmd).await.unwrap();
    assert!(
        built.is_some(),
        "all-known pack still yields a chain under trunk"
    );
}

#[tokio::test]
async fn tp12_review_keeps_noop() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = isolated_config(temp.path().join("config"));
    config.monorepo.push_policy = PushPolicy::Review;
    let storage = test_storage_with_config(temp.path(), config).await;
    let keep = Tree::from_tree_items(vec![blob_item(
        ".gitkeep",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )])
    .unwrap();
    let root_c = Commit::from_tree_id(keep.id, vec![], "root");
    storage
        .mono_storage()
        .save_mega_trees(vec![keep.clone()], root_c.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![root_c.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &keep, &root_c.id.to_string()).await;
    let cmd = crate::ceres::protocol::import_refs::RefCommand::new(
        root_c.id.to_string(),
        root_c.id.to_string(),
        MEGA_BRANCH_NAME.to_string(),
    );
    let repo = tp12_monorepo(
        &storage,
        vec![cmd.clone()],
        std::collections::HashSet::new(),
        std::collections::HashSet::new(),
    );
    let built = repo.build_push_chain(&cmd).await.unwrap();
    assert!(built.is_none(), "review empty pack stays Noop");
    assert!(crate::ceres::pack::RepoHandler::receive_pack_notice(&repo).is_some());
}

fn authored_commit(
    tree: ObjectHash,
    parents: Vec<ObjectHash>,
    name: &str,
    email: &str,
    ts: usize,
    message: &str,
) -> Commit {
    use git_internal::internal::object::signature::{Signature, SignatureType};
    let author = Signature {
        signature_type: SignatureType::Author,
        name: name.into(),
        email: email.into(),
        timestamp: ts,
        timezone: "+0800".into(),
    };
    let committer = Signature {
        signature_type: SignatureType::Committer,
        name: name.into(),
        email: email.into(),
        timestamp: ts,
        timezone: "+0800".into(),
    };
    Commit::new(author, committer, tree, parents, message)
}

fn assert_has_gpgsig(content: &str) {
    assert!(
        content.contains("gpgsig ") || content.contains("-----BEGIN PGP SIGNATURE-----"),
        "synthetic commit must carry a server GPG signature: {content}"
    );
}

#[tokio::test]
async fn tp16_n1_lands_client_tip_and_root_message_matches() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
        tp12_path_fixture("p16n1", "path tip").await;
    let new_child = Tree::from_tree_items(vec![blob_item(
        "y.txt",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
    )])
    .unwrap();
    let new_commit = authored_commit(
        new_child.id,
        vec![path_commit.id],
        "Alice",
        "alice@example.com",
        1_720_000_000,
        "feat: n1 client message",
    );
    storage
        .mono_storage()
        .save_mega_trees(vec![new_child], new_commit.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![new_commit.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![new_commit.id.to_string()],
        fork_base: Some(path_commit.id.to_string()),
        n: 1,
    };
    let id = tp12_enqueue_claim(
        &storage,
        &path,
        &path_commit.id.to_string(),
        &new_commit.id.to_string(),
        &payload,
    )
    .await;
    let exec = tp12_exec(&storage, id).await;
    let ExecuteOutcome::Done {
        landed_commit_id, ..
    } = exec
    else {
        panic!("expected Done, got {exec:?}");
    };
    assert_eq!(landed_commit_id, new_commit.id.to_string());
    let landed = storage
        .mono_storage()
        .get_commit_by_hash(&landed_commit_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        landed.content.as_deref(),
        Some("feat: n1 client message"),
        "N=1 must keep the client object"
    );
    let root = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    let root_c = storage
        .mono_storage()
        .get_commit_by_hash(&root.ref_commit_hash)
        .await
        .unwrap()
        .unwrap();
    let root_msg = root_c.content.as_deref().unwrap_or("");
    assert!(
        root_msg.contains("feat: n1 client message"),
        "root roll-up message must match the client commit: {root_msg}"
    );
    assert!(!root_msg.contains("Mono-Squash"));
    assert_has_gpgsig(root_msg);
    assert!(
        root_c
            .author
            .as_deref()
            .unwrap_or("")
            .contains("Alice <alice@example.com>")
    );
}

#[tokio::test]
async fn tp16_n3_squash_provenance_and_coauthors() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, _root_tree, _root_c, _child, path_commit, path) =
        tp12_path_fixture("p16n3", "path tip").await;
    let t1 = Tree::from_tree_items(vec![blob_item(
        "a.txt",
        "1111111111111111111111111111111111111111",
    )])
    .unwrap();
    let t2 = Tree::from_tree_items(vec![blob_item(
        "b.txt",
        "2222222222222222222222222222222222222222",
    )])
    .unwrap();
    let t3 = Tree::from_tree_items(vec![blob_item(
        "c.txt",
        "3333333333333333333333333333333333333333",
    )])
    .unwrap();
    let c1 = authored_commit(
        t1.id,
        vec![path_commit.id],
        "Alice",
        "alice@example.com",
        1_720_000_000,
        "feat: parser",
    );
    let c2 = authored_commit(
        t2.id,
        vec![c1.id],
        "Bob",
        "bob@example.com",
        1_720_000_000 + 200_000,
        "fix: empty input",
    );
    let c3 = authored_commit(
        t3.id,
        vec![c2.id],
        "Alice",
        "alice@example.com",
        1_720_000_000 + 200_001,
        "test: edge cases",
    );
    storage
        .mono_storage()
        .save_mega_trees(vec![t1, t2, t3.clone()], c3.id, None)
        .await
        .unwrap();
    storage
        .mono_storage()
        .save_mega_commits(vec![c1.clone(), c2.clone(), c3.clone()], None)
        .await
        .unwrap();
    let payload = PushPayload {
        commits: vec![c3.id.to_string(), c2.id.to_string(), c1.id.to_string()],
        fork_base: Some(path_commit.id.to_string()),
        n: 3,
    };
    let old_id = path_commit.id.to_string();
    let id = tp12_enqueue_claim(&storage, &path, &old_id, &c3.id.to_string(), &payload).await;
    let exec = tp12_exec(&storage, id).await;
    let ExecuteOutcome::Done {
        landed_commit_id, ..
    } = exec
    else {
        panic!("expected Done, got {exec:?}");
    };
    assert_ne!(landed_commit_id, c3.id.to_string());
    let squash = storage
        .mono_storage()
        .get_commit_by_hash(&landed_commit_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(squash.tree, t3.id.to_string());
    let parents: Vec<String> = serde_json::from_value(squash.parents_id).unwrap();
    assert_eq!(parents, vec![old_id.clone()]);
    let msg = squash.content.as_deref().unwrap_or("");
    assert!(
        msg.contains("Squash 3 commits at /p16n3"),
        "signed squash keeps the subject after gpgsig: {msg}"
    );
    assert!(msg.contains("This commit was created by mega2"));
    assert!(msg.contains("Mono-Squash-Count: 3"));
    assert!(msg.contains(&format!("Mono-Squash-Range: {old_id}..{}", c3.id)));
    assert!(!msg.contains("Mono-Commits"));
    let pos1 = msg.find(&c1.id.to_string()).expect("c1 listed");
    let pos2 = msg.find(&c2.id.to_string()).expect("c2 listed");
    let pos3 = msg.find(&c3.id.to_string()).expect("c3 listed");
    assert!(pos1 < pos2 && pos2 < pos3, "topo ascending listing");
    assert!(msg.contains("Co-authored-by: Bob <bob@example.com>"));
    assert!(msg.contains("Mono-Author-Date-Range:"));
    assert!(
        squash
            .author
            .as_deref()
            .unwrap_or("")
            .contains("Alice <alice@example.com>")
    );
    assert_has_gpgsig(msg);

    let root = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    let root_c = storage
        .mono_storage()
        .get_commit_by_hash(&root.ref_commit_hash)
        .await
        .unwrap()
        .unwrap();
    let root_msg = root_c.content.as_deref().unwrap_or("");
    assert!(root_msg.contains(&format!("Mono-Squash-Commit: {landed_commit_id}")));
    assert!(!root_msg.contains("feat: parser"));
    assert_has_gpgsig(root_msg);
    assert!(
        root_c
            .author
            .as_deref()
            .unwrap_or("")
            .contains("Alice <alice@example.com>")
    );
}

async fn fu07_storage(temp: &std::path::Path) -> Storage {
    use crate::jupiter::{
        service::{git_service::GitService, mono_service::MonoService},
        storage::object_storage::mock_object_storage,
    };

    let mut storage = crate::jupiter::tests::test_storage(temp).await;
    let git_service = GitService {
        obj_storage: mock_object_storage(),
    };
    storage.git_service = git_service.clone();
    storage.mono_service = MonoService {
        mono_storage: storage.mono_storage(),
        git_service,
    };
    storage
        .mono_service
        .init_monorepo(&storage.config().monorepo)
        .await
        .expect("init monorepo");
    storage
}

#[tokio::test]
async fn fu07_plan_highest_new_component() {
    let temp = tempfile::tempdir().unwrap();
    let storage = fu07_storage(temp.path()).await;
    let service = test_service(&storage);
    let plan = |highest: Option<&str>, path: &str| ProvisionPlan {
        path: path.to_owned(),
        highest_new: highest.map(str::to_owned),
    };
    assert_eq!(
        service.plan_provision("/project/a/b").await.unwrap(),
        plan(Some("/project/a"), "/project/a/b")
    );
    assert_eq!(
        service.plan_provision("/project").await.unwrap(),
        plan(None, "/project")
    );
    let conflict = service
        .plan_provision("/project/.gitkeep/x")
        .await
        .expect_err("file component");
    assert!(
        conflict
            .to_string()
            .contains("MONO_PATH_CONFLICT: \"/project/.gitkeep/x\": \"/project/.gitkeep\""),
        "{conflict}"
    );
    for (path, code) in [
        ("/fu07-vendor/x", "MONO_PATH_NOT_ALLOWED"),
        ("/third-party/x", "MONO_PATH_NOT_ALLOWED"),
        ("/", "MONO_PATH_INVALID"),
    ] {
        let err = service.plan_provision(path).await.expect_err(path);
        assert!(err.to_string().contains(code), "{path}: {err}");
    }
}

#[derive(Debug, PartialEq)]
struct Fu07Err(String);

impl From<GitError> for Fu07Err {
    fn from(err: GitError) -> Self {
        Fu07Err(err.to_string())
    }
}

/// Scripted provisioner: one plan per round, one apply result per apply.
struct Fu07Script {
    plans: std::sync::Mutex<Vec<Option<&'static str>>>,
    applies: std::sync::Mutex<Vec<Result<ApplyOutcome, String>>>,
    plan_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl PathProvisioner for Fu07Script {
    async fn plan_provision(&self, path: &str) -> Result<ProvisionPlan, GitError> {
        self.plan_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let highest = self.plans.lock().unwrap().remove(0);
        Ok(ProvisionPlan {
            path: path.to_owned(),
            highest_new: highest.map(str::to_owned),
        })
    }

    async fn apply_provision(
        &self,
        _plan: &ProvisionPlan,
        _requester: Option<String>,
    ) -> Result<ApplyOutcome, GitError> {
        self.applies
            .lock()
            .unwrap()
            .remove(0)
            .map_err(GitError::CustomError)
    }
}

fn fu07_script(
    plans: Vec<Option<&'static str>>,
    applies: Vec<Result<ApplyOutcome, String>>,
) -> Fu07Script {
    Fu07Script {
        plans: std::sync::Mutex::new(plans),
        applies: std::sync::Mutex::new(applies),
        plan_calls: std::sync::atomic::AtomicUsize::new(0),
    }
}

#[tokio::test]
async fn fu07_reprepare_reauthorizes() {
    // Round 1 creates /project/a but loses the queue race; round 2 finds
    // the path already provisioned and re-authorizes against the target.
    let script = fu07_script(
        vec![Some("/project/a"), None],
        vec![Ok(ApplyOutcome::Retry(
            "another push for this path is already Queued/Running (ADR-TP-10)".to_owned(),
        ))],
    );
    let mut scopes = Vec::new();
    let result = run_path_provision::<Fu07Err, _, _>(&script, "/project/a/b", |scope: &str| {
        scopes.push(scope.to_owned());
        Ok(Some("tester".to_owned()))
    })
    .await
    .expect("converges");
    assert_eq!(
        scopes,
        ["/project/a/b", "/project/a", "/project/a/b"],
        "target pre-check, then one authorization per prepare"
    );
    assert_eq!(
        result,
        PathProvisionResult {
            path: "/project/a/b".to_owned(),
            created: false,
            commit_id: None,
        }
    );

    // A narrower scope on a later round is refused even though an
    // earlier round was allowed (no widening between prepare rounds).
    let script = fu07_script(
        vec![Some("/project/a/b"), Some("/project/a")],
        vec![Ok(ApplyOutcome::Retry(
            "highest new component changed".to_owned(),
        ))],
    );
    let err = run_path_provision::<Fu07Err, _, _>(&script, "/project/a/b", |scope: &str| {
        if scope == "/project/a" {
            Err(Fu07Err("403".to_owned()))
        } else {
            Ok(None)
        }
    })
    .await
    .expect_err("second round needs a wider scope");
    assert_eq!(err, Fu07Err("403".to_owned()));
}

/// Plans once from a stale view (as if a container was deleted after the
/// plan), then delegates to the real service.
struct Fu07StaleFirst<'a> {
    inner: &'a MonoApiService,
    stale: std::sync::Mutex<Option<ProvisionPlan>>,
}

#[async_trait]
impl PathProvisioner for Fu07StaleFirst<'_> {
    async fn plan_provision(&self, path: &str) -> Result<ProvisionPlan, GitError> {
        let stale = self.stale.lock().unwrap().take();
        match stale {
            Some(plan) => Ok(plan),
            None => self.inner.plan_provision(path).await,
        }
    }

    async fn apply_provision(
        &self,
        plan: &ProvisionPlan,
        requester: Option<String>,
    ) -> Result<ApplyOutcome, GitError> {
        self.inner.apply_provision(plan, requester).await
    }
}

#[tokio::test]
async fn fu07_apply_bound_to_plan() {
    let temp = tempfile::tempdir().unwrap();
    let storage = fu07_storage(temp.path()).await;
    let service = test_service(&storage);
    let root_before = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();

    // /project/team does not exist: a plan that authorized only
    // /project/team/x is stale, so apply writes nothing and asks to re-plan.
    let stale = ProvisionPlan {
        path: "/project/team/x".to_owned(),
        highest_new: Some("/project/team/x".to_owned()),
    };
    assert!(matches!(
        service.apply_provision(&stale, None).await.unwrap(),
        ApplyOutcome::Retry(_)
    ));

    // The loop re-plans and re-authorizes: the narrow token that covers
    // only /project/team/x is refused for the container.
    let racing = Fu07StaleFirst {
        inner: &service,
        stale: std::sync::Mutex::new(Some(stale)),
    };
    let mut scopes = Vec::new();
    let err = run_path_provision::<Fu07Err, _, _>(&racing, "/project/team/x", |scope: &str| {
        scopes.push(scope.to_owned());
        if scope.starts_with("/project/team/x") {
            Ok(Some("narrow".to_owned()))
        } else {
            Err(Fu07Err("403".to_owned()))
        }
    })
    .await
    .expect_err("container needs a wider token");
    assert_eq!(err, Fu07Err("403".to_owned()));
    assert_eq!(
        scopes,
        ["/project/team/x", "/project/team/x", "/project/team"]
    );

    // A file where a directory must be created is a conflict, not a
    // second same-name tree entry.
    let file = ProvisionPlan {
        path: "/project/.gitkeep/x".to_owned(),
        highest_new: Some("/project/.gitkeep".to_owned()),
    };
    let conflict = service
        .apply_provision(&file, None)
        .await
        .expect_err("file component");
    assert!(
        conflict.to_string().contains("MONO_PATH_CONFLICT"),
        "{conflict}"
    );

    let root_after = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root_after.ref_commit_hash, root_before.ref_commit_hash);
    assert_eq!(root_after.ref_tree_hash, root_before.ref_tree_hash);
}

#[test]
fn fu07_retry_reasons_are_typed() {
    let retry = |text: &str| provision_retry_reason(&GitError::CustomError(text.to_owned()));
    assert!(retry(PROVISION_STALE_SNAPSHOT).is_some());
    assert!(
        retry("Other error: another push for this path is already Queued/Running (ADR-TP-10)")
            .is_some()
    );
    assert!(
            retry(
                "Other error: non-fast-forward: new_id does not match current tip; align with `git fetch && git reset --hard origin/main`"
            )
            .is_some()
        );
    assert!(
            retry("Other error: push rejected for push_queue id 17: cannot update missing path; create requires old_id=ZERO_ID")
                .is_some()
        );
    // Coded errors are final even when a user path spells a retry text.
    assert!(
            retry("[code:409] MONO_PATH_UNINITIALIZED: \"/third-party/non-fast-forward\" does not exist yet")
                .is_none()
        );
    assert!(retry("Other error: some unknown landing failure").is_none());
    assert!(
        retry("Other error: see non-fast-forward: new_id does not match current tip").is_none()
    );
}

async fn fu07_trunk_storage(temp: &std::path::Path) -> Storage {
    use crate::jupiter::{
        service::{git_service::GitService, mono_service::MonoService},
        storage::object_storage::mock_object_storage,
    };

    let mut storage = tp11_storage(temp).await;
    let git_service = GitService {
        obj_storage: mock_object_storage(),
    };
    storage.git_service = git_service.clone();
    storage.mono_service = MonoService {
        mono_storage: storage.mono_storage(),
        git_service,
    };
    storage
        .mono_service
        .init_monorepo(&storage.config().monorepo)
        .await
        .expect("init monorepo");
    storage
}

/// Two rounds planned from the same snapshot race to land: exactly one
/// lands, the other is told to re-plan (never a second `created:true`).
#[tokio::test]
async fn fu07_concurrent_apply_one_lands() {
    let _lock = materialize::lock_materialize_tests().await;
    let temp = tempfile::tempdir().unwrap();
    let storage = fu07_trunk_storage(temp.path()).await;
    let service = MonoApiService {
        storage: storage.clone(),
        git_object_cache: Arc::new(GitObjectCache {
            connection: crate::jupiter::tests::test_redis_manager().await,
            prefix: String::new(),
        }),
    };
    let plan = service.plan_provision("/project/fu07-race").await.unwrap();
    assert_eq!(plan.highest_new.as_deref(), Some("/project/fu07-race"));
    let (a, b) = tokio::join!(
        service.apply_provision(&plan, Some("a".to_owned())),
        service.apply_provision(&plan, Some("b".to_owned()))
    );
    let outcomes = [a.expect("apply a"), b.expect("apply b")];
    let landed = outcomes
        .iter()
        .filter(|o| matches!(o, ApplyOutcome::Landed(_)))
        .count();
    assert_eq!(landed, 1, "{outcomes:?}");
    assert!(
        outcomes.iter().any(|o| matches!(o, ApplyOutcome::Retry(_))),
        "{outcomes:?}"
    );
    assert_eq!(
        service
            .plan_provision("/project/fu07-race")
            .await
            .unwrap()
            .highest_new,
        None
    );
}

#[tokio::test]
async fn fu07_client_errors_are_not_retried() {
    let script = fu07_script(
        vec![Some("/project/a")],
        vec![Err("[code:409] MONO_PATH_UNINITIALIZED: \"/x\"".to_owned())],
    );
    let err = run_path_provision::<Fu07Err, _, _>(&script, "/project/a", |_: &str| Ok(None))
        .await
        .expect_err("client error");
    assert!(err.0.contains("MONO_PATH_UNINITIALIZED"), "{err:?}");
    assert_eq!(
        script.plan_calls.load(std::sync::atomic::Ordering::SeqCst),
        1
    );

    // An unauthorized caller is refused before any plan (no tree read).
    let script = fu07_script(Vec::new(), Vec::new());
    let err = run_path_provision::<Fu07Err, _, _>(&script, "/project/a", |_: &str| {
        Err(Fu07Err("401".to_owned()))
    })
    .await
    .expect_err("unauthorized");
    assert_eq!(err, Fu07Err("401".to_owned()));
    assert_eq!(
        script.plan_calls.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

/// FU-03 (ADR-FU-02): the review-morph merge constructor and the legacy
/// descendant continuation synthesize unsigned commits framed with the
/// header/body blank line, so `git fsck --strict` sees no
/// `unterminatedHeader`.
#[tokio::test]
async fn fu03_review_and_legacy_sites_framed() {
    use git_internal::internal::object::ObjectTrait;
    use sea_orm::TransactionTrait;

    use crate::jupiter::{storage::base_storage::StorageConnector, tests::test_storage};

    fn assert_framed(commit: &Commit, body: &str) {
        assert_eq!(commit.message, format!("\n{body}"));
        let raw = String::from_utf8(commit.to_data().unwrap()).unwrap();
        let (header, raw_body) = raw.split_once("\n\n").expect("header/body blank line");
        assert!(header.lines().last().unwrap().starts_with("committer "));
        assert_eq!(raw_body, body);
    }

    let tree_id = ObjectHash::from_str("27dd8d4cf39f3868c6eee38b601bc9e9939304f5").unwrap();
    let result = TreeUpdateResult {
        updated_trees: vec![],
        ref_updates: vec![RefUpdate {
            path: "/fu03".to_string(),
            tree_id,
        }],
    };
    let refs = vec![mega_refs::Model::new(
        "/fu03",
        MEGA_BRANCH_NAME.to_owned(),
        "0987654321098765432109876543210987654321".to_owned(),
        "1111111111111111111111111111111111111111".to_owned(),
        false,
    )];
    let (mut commits, mut updates, mut new_commit_id) = (Vec::new(), Vec::new(), String::new());
    MonoServiceLogic::process_ref_updates(
        &result,
        &refs,
        "cl merge generated commit",
        &mut commits,
        &mut updates,
        &mut new_commit_id,
    )
    .unwrap();
    assert_eq!(commits.len(), 1);
    assert_framed(&commits[0], "cl merge generated commit");
    assert_eq!(new_commit_id, commits[0].id.to_string());

    let temp = tempfile::tempdir().unwrap();
    let storage = test_storage(temp.path()).await;
    let mono = storage.mono_storage();
    let leaf_old = Tree::from_tree_items(vec![blob_item(
        "a.txt",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )])
    .unwrap();
    let leaf_new = Tree::from_tree_items(vec![blob_item(
        "a.txt",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )])
    .unwrap();
    let dir = |leaf: &Tree| {
        Tree::from_tree_items(vec![TreeItem::new(
            TreeItemMode::Tree,
            leaf.id,
            "chg".to_string(),
        )])
        .unwrap()
    };
    let (old_p, new_p) = (dir(&leaf_old), dir(&leaf_new));
    mono.save_mega_trees(
        vec![
            leaf_old.clone(),
            leaf_new.clone(),
            old_p.clone(),
            new_p.clone(),
        ],
        ObjectHash::from_str(&"9".repeat(40)).unwrap(),
        None,
    )
    .await
    .unwrap();
    let c_p = Commit::from_tree_id(old_p.id, vec![], "\np");
    let c_chg = Commit::from_tree_id(leaf_old.id, vec![], "\nchg");
    mono.save_mega_commits(vec![c_p.clone(), c_chg.clone()], None)
        .await
        .unwrap();
    for (path, commit, tree) in [("/fu03", &c_p, &old_p), ("/fu03/chg", &c_chg, &leaf_old)] {
        mono.save_refs(
            mega_refs::Model::new(
                path,
                MEGA_BRANCH_NAME.to_owned(),
                commit.id.to_string(),
                tree.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
    }
    let txn = mono.get_connection().begin().await.unwrap();
    mono.advance_descendant_refs(
        "/fu03",
        &new_p.id.to_string(),
        Some(&old_p.id.to_string()),
        &txn,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let chg = mono.get_main_ref("/fu03/chg").await.unwrap().unwrap();
    assert_eq!(chg.ref_tree_hash, leaf_new.id.to_string());
    let continuation = Commit::from_mega_model(
        mono.get_commit_by_hash(&chg.ref_commit_hash)
            .await
            .unwrap()
            .expect("continuation commit"),
    );
    assert_eq!(continuation.parent_commit_ids, vec![c_chg.id]);
    assert_framed(&continuation, "trunk descendant continuation");
}

#[tokio::test]
async fn tp16_review_merge_keeps_from_tree_id_shape() {
    let _lock = materialize::lock_materialize_tests().await;
    let (_temp, storage, service, cl, _stale) =
        nested_queue_fixture("/p16m", "TP16M", "p16m").await;
    service
        .merge_cl("gate-tester", "gate-tester", cl.clone())
        .await
        .expect("merge");
    let main = storage
        .mono_storage()
        .get_main_ref("/p16m")
        .await
        .unwrap()
        .unwrap();
    let landed = storage
        .mono_storage()
        .get_commit_by_hash(&main.ref_commit_hash)
        .await
        .unwrap()
        .unwrap();
    let msg = landed.content.as_deref().unwrap_or("");
    // Framed with the header/body blank line since FU-03 (ADR-FU-02).
    assert_eq!(msg, "\ncl merge generated commit");
    assert!(
        landed
            .author
            .as_deref()
            .unwrap_or("")
            .contains("mega <admin@mega.org>")
    );
    assert!(!msg.contains("Mono-Squash"));
}

fn tp12_monorepo(
    storage: &Storage,
    commands: Vec<crate::ceres::protocol::import_refs::RefCommand>,
    pack_commit_ids: std::collections::HashSet<String>,
    new_commit_ids: std::collections::HashSet<String>,
) -> crate::ceres::pack::monorepo::Monorepo {
    use std::{collections::HashMap, path::PathBuf, sync::Mutex};

    use tokio::sync::RwLock;

    crate::ceres::pack::monorepo::Monorepo {
        storage: storage.clone(),
        git_object_cache: test_service(storage).git_object_cache,
        path: PathBuf::from("/"),
        base_branch: "main".to_string(),
        pack_commit_ids: Mutex::new(pack_commit_ids),
        new_commit_ids: Mutex::new(new_commit_ids),
        no_op_notice: Mutex::new(None),
        push_chain_cache: Mutex::new(HashMap::new()),
        cl_link: std::sync::Arc::new(RwLock::new(None)),

        username: Some("tester".to_string()),
        command_list: Mutex::new(commands),
    }
}
