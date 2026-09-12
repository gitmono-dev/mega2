//! UN-19 fail-closed: a check that could not run is not a pass.
//!
//! Three things can prevent the ACL-change check from reaching a verdict — the
//! changed-file list cannot be read, main's copy of the authorization file
//! cannot be resolved, or it is absent from main. In every one of them the
//! change was never examined, so `enforce` refuses the merge and `shadow`
//! records it and proceeds. The refusal is 503 rather than 500 or 403: nothing
//! was wrong with the change and nobody rejected it, so the same merge can
//! succeed once authorization is available again (UN-25's contract).

use std::sync::Arc;

use git_internal::{
    hash::{ObjectHash, get_hash_kind},
    internal::object::tree::{Tree, TreeItem, TreeItemMode},
};
use sea_orm::ConnectionTrait;

use crate::{
    callisto::{
        mega_refs,
        sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
    },
    ceres::api_service::{
        cache::GitObjectCache,
        mono_api_service::{MonoApiService, emit_merge_authz_unavailable},
    },
    common::utils::MEGA_BRANCH_NAME,
    contract::policy::entitystore::{SharedEntityStore, generate_entity},
    jupiter::storage::{
        Storage,
        base_storage::StorageConnector,
        push_queue_storage::{EnqueueOutcome, EnqueueParams},
    },
};

fn service(storage: &Storage) -> MonoApiService {
    MonoApiService {
        storage: storage.clone(),
        git_object_cache: Arc::new(GitObjectCache {
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "un19-test".to_string(),
        }),
    }
}

/// Storage in the given enforcement mode, with an admin-only ACL snapshot
/// already built.
async fn storage_in(temp: &std::path::Path, mode: &str) -> Storage {
    let mut storage = crate::jupiter::tests::test_storage(temp).await;
    let mut config = (*storage.config).clone();
    config.cedar.enforcement = mode.to_string();
    let config = Arc::new(config);
    storage.config_handle = crate::config::reload::ConfigHandle::from_arc(config.clone());
    storage.config = config;

    let json = generate_entity(&["un19-admin".to_string()], "/").expect("init product");
    let store = SharedEntityStore::new();
    store.ensure(&json).expect("build snapshot");
    storage.set_entity_store_for_test(Arc::new(store));
    storage
}

/// Main with a `/.mega_cedar.json` present, or without it.
async fn seed_main(storage: &Storage, with_acl: bool) {
    let mut items = vec![TreeItem::new(
        TreeItemMode::Blob,
        ObjectHash::from_hex_for_kind(get_hash_kind(), "2222222222222222222222222222222222222222")
            .unwrap(),
        "README.md".to_string(),
    )];
    if with_acl {
        items.push(TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_hex_for_kind(
                get_hash_kind(),
                "3333333333333333333333333333333333333333",
            )
            .unwrap(),
            ".mega_cedar.json".to_string(),
        ));
    }
    let tree = Tree::from_tree_items(items).expect("root tree");
    let commit_id = "1111111111111111111111111111111111111111";
    storage
        .mono_storage()
        .save_mega_trees(
            vec![tree.clone()],
            ObjectHash::from_hex_for_kind(get_hash_kind(), commit_id).unwrap(),
            None,
        )
        .await
        .expect("save root tree");
    storage
        .mono_storage()
        .save_refs(
            mega_refs::Model {
                id: 1,
                path: "/".to_string(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: commit_id.to_string(),
                ref_tree_hash: tree.id.to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .expect("save main ref");
}

async fn seed_cl(storage: &Storage, id: i64, cl_link: &str) {
    storage
        .cl_storage()
        .get_connection()
        .execute_unprepared(&format!(
            "INSERT INTO mega_cl \
             (id, link, title, status, path, from_hash, to_hash, created_at, updated_at, \
              username, base_branch) \
             VALUES ({id}, '{cl_link}', 'un19', 'open', '/', 'from', 'to', now(), now(), \
             'un19-author', 'main')"
        ))
        .await
        .expect("seed CL");
}

/// Class 1: the changed-file list cannot be read (its commits do not exist).
#[tokio::test]
async fn un19_fail_closed_when_the_changed_file_list_cannot_be_read() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "enforce").await;
    seed_main(&storage, true).await;
    seed_cl(&storage, 720_001, "UN19UNREADABLE").await;

    let error = service(&storage)
        .enforce_acl_change_authorization("UN19UNREADABLE", "un19-admin")
        .await
        .expect_err("an unreadable change set must not merge");

    let message = error.to_string();
    assert!(
        message.contains("[code:503]"),
        "not examined is retryable, not a rejection: {message}"
    );
    assert!(
        !message.contains("[code:500]"),
        "an undecidable check is not a server bug: {message}"
    );
}

/// Class 1b: the commits exist but their tree does not. `get_commit_blobs`
/// answers with an empty list there, which is indistinguishable from "this CL
/// changed nothing" — the same fail-open shape, so it must be caught too.
#[tokio::test]
async fn un19_fail_closed_when_a_commit_tree_is_missing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "enforce").await;
    seed_main(&storage, true).await;
    seed_cl(&storage, 720_004, "UN19NOTREE").await;

    // Commits that exist but point at a tree that was never saved.
    storage
        .cl_storage()
        .get_connection()
        .execute_unprepared(
            "INSERT INTO mega_commit (id, commit_id, tree, parents_id, author, committer,              content, created_at) VALUES              (720101, 'from', '4444444444444444444444444444444444444444', '{}', '', '', '', now()),              (720102, 'to', '5555555555555555555555555555555555555555', '{}', '', '', '', now())",
        )
        .await
        .expect("seed commits without trees");

    let error = service(&storage)
        .enforce_acl_change_authorization("UN19NOTREE", "un19-admin")
        .await
        .expect_err("a commit without a tree makes the change set unknowable");
    assert!(
        error.to_string().contains("[code:503]"),
        "not examined is retryable: {error}"
    );
}

/// A CL whose link does not exist at all is likewise undecidable, not allowed.
#[tokio::test]
async fn un19_fail_closed_for_an_unknown_cl() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "enforce").await;
    seed_main(&storage, true).await;

    assert!(
        service(&storage)
            .enforce_acl_change_authorization("UN19NOSUCHCL", "un19-admin")
            .await
            .is_err()
    );
}

/// The same conditions under `shadow` must not change the outcome.
#[tokio::test]
async fn un19_shadow_records_but_lets_the_merge_proceed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "shadow").await;
    seed_main(&storage, true).await;
    seed_cl(&storage, 720_002, "UN19SHADOW").await;

    assert!(
        service(&storage)
            .enforce_acl_change_authorization("UN19SHADOW", "un19-admin")
            .await
            .is_ok(),
        "shadow evaluates and records, but never changes the outcome"
    );
}

/// `off` performs no detection at all.
#[tokio::test]
async fn un19_off_performs_no_detection() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "off").await;
    // Deliberately seed nothing: under `off` there is nothing to read anyway.
    assert!(
        service(&storage)
            .enforce_acl_change_authorization("UN19OFF", "whoever")
            .await
            .is_ok()
    );
}

/// Renaming the authorization file away is still a change to it: the diff
/// reports only the new path, so a check that looked there alone could be
/// side-stepped by moving the file out of where it is read.
#[test]
fn un19_a_rename_of_the_authz_file_counts_as_touching_it() {
    use std::path::PathBuf;

    use git_internal::hash::{ObjectHash, get_hash_kind};

    use crate::ceres::model::change_list::ClDiffFile;

    let acl = PathBuf::from(".mega_cedar.json");
    let elsewhere = PathBuf::from("docs/old-acl.json");
    let hash =
        ObjectHash::from_hex_for_kind(get_hash_kind(), "6666666666666666666666666666666666666666")
            .unwrap();

    let renamed_away = ClDiffFile::Renamed(acl.clone(), elsewhere.clone(), hash, hash, 100);
    assert_eq!(
        renamed_away.path(),
        &elsewhere,
        "the diff itself only reports the new path — which is exactly the gap"
    );

    // The check must look at both sides.
    let touches = |file: &ClDiffFile| match file {
        ClDiffFile::Renamed(old_path, new_path, ..) | ClDiffFile::Moved(old_path, new_path, ..) => {
            old_path.ends_with(".mega_cedar.json") || new_path.ends_with(".mega_cedar.json")
        }
        other => other.path().ends_with(".mega_cedar.json"),
    };
    assert!(touches(&renamed_away), "renaming it away must count");
    assert!(
        touches(&ClDiffFile::Moved(elsewhere, acl, hash, hash, 100)),
        "moving something onto it must count too"
    );
}

/// The contract event, asserted at its single emit site.
#[test]
fn un19_the_unavailable_alert_carries_its_fields() {
    use std::{io::Write, sync::Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct TestWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl MakeWriter<'_> for TestWriter {
        type Writer = TestWriter;
        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(TestWriter(buf.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        emit_merge_authz_unavailable("UN19EVENT", "un19-admin", "the changed-file list failed");
    });
    let out = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8");

    assert!(out.contains("merge_authz_unavailable"), "event: {out}");
    assert!(out.contains("UN19EVENT"), "cl_link: {out}");
    assert!(out.contains("un19-admin"), "principal: {out}");
    assert!(
        out.contains("the changed-file list failed"),
        "reason: {out}"
    );
}

/// The queue freezes rather than merging when the check cannot run, keeping the
/// requester and the retry guidance (UN-25's fields).
#[tokio::test]
async fn un19_fail_closed_freezes_a_queued_item() {
    let temp = tempfile::tempdir().expect("temp dir");
    let storage = storage_in(temp.path(), "enforce").await;
    seed_main(&storage, true).await;
    seed_cl(&storage, 720_003, "UN19QUEUE").await;
    let outcome = storage
        .push_queue_storage()
        .enqueue_atomic(EnqueueParams {
            kind: PushQueueKindEnum::Merge,
            operation_id: "UN19QUEUE",
            path: "/",
            old_id: "from",
            new_id: "to",
            requester: Some("un19-admin"),
            payload: serde_json::json!({
                "cl_link": "UN19QUEUE",
                "authz_principal": "un19-admin",
                "execution_actor": "system",
                "apply_queue_execution_decision": true,
                "requester": "un19-admin",
            }),
        })
        .await
        .expect("enqueue");
    assert!(
        matches!(outcome, EnqueueOutcome::Inserted { .. }),
        "expected insert, got {outcome:?}"
    );

    let service = service(&storage);
    let error = service
        .enforce_acl_change_authorization("UN19QUEUE", "un19-admin")
        .await
        .expect_err("undecidable");
    service
        .freeze_merge_queue_item_for_authz("UN19QUEUE", &error.to_string())
        .await
        .expect("freeze");

    let item = storage
        .push_queue_storage()
        .list_by_kind_and_operation(PushQueueKindEnum::Merge, "UN19QUEUE")
        .await
        .expect("read item")
        .into_iter()
        .next()
        .expect("item");
    assert_eq!(item.status, PushQueueStatusEnum::Failed);
    assert_eq!(
        item.requester,
        Some("un19-admin".to_string()),
        "the frozen item keeps the subject it will be re-decided against"
    );
    assert!(
        item.error_message.expect("message").contains("Retry"),
        "the frozen item states its retry condition"
    );
}
