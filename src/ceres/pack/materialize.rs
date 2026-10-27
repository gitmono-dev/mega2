#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    future::Future,
    path::{Component, PathBuf},
    time::Duration,
};

use git_internal::{
    hash::HashKind,
    internal::{
        metadata::EntryMeta,
        object::{commit::Commit, signature::Signature, tree::Tree},
    },
};
use sea_orm::{IntoActiveModel, TransactionTrait};

use crate::{
    callisto::mega_refs,
    common::{
        errors::MegaError,
        utils::{format_commit_msg, split_commit_message},
    },
    jupiter::{
        storage::{
            Storage, base_storage::StorageConnector, mono_storage::MonoStorage,
            push_queue_storage::PushQueueStorage,
        },
        utils::converter::{
            IntoMegaModel, commit_from_mega_model_with_kind, tree_from_mega_model_with_kind,
        },
    },
};

/// Bounded retries after the initial attempt (ADR-TP-20 item 1: K=2 + backoff).
pub const MATERIALIZE_RETRY_LIMIT: u32 = 2;
const BACKOFF_BASE_MS: u64 = 10;

#[cfg(test)]
pub(crate) static WALK_COUNT: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static ABANDON_COUNT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_materialize_test_counters() {
    WALK_COUNT.store(0, Ordering::SeqCst);
    ABANDON_COUNT.store(0, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) async fn lock_materialize_tests() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

struct WalkedRoot {
    ref_name: String,
    root_commit_hash: String,
    root_tree_hash: String,
    subtree: Tree,
    author: Signature,
    committer: Signature,
    message: String,
}

enum OnceOutcome {
    Done(Vec<mega_refs::Model>),
    PathAbsent,
    Abandoned,
}

/// Lazy-materialize path refs under ADR-TP-20 item 1.
///
/// Off-lock tree walk records the starting root identity pair
/// (`ref_commit_hash` + `ref_tree_hash`). A short `MONO_WRITE_LOCK` txn then
/// re-reads the root, inserts with `WHERE NOT EXISTS`, and deletes a
/// continued tombstone. Identity mismatch abandons; callers retry with
/// `heads_exist` bypassed so a concurrent insert cannot pin a stale row.
pub async fn materialize_path_refs(
    storage: &Storage,
    path: &str,
) -> Result<Vec<mega_refs::Model>, MegaError> {
    materialize_loop(storage, path, false, || async {}).await
}

#[cfg(test)]
pub(crate) async fn materialize_path_refs_with_hook<F, Fut>(
    storage: &Storage,
    path: &str,
    bypass_heads_exist: bool,
    after_walk: F,
) -> Result<Vec<mega_refs::Model>, MegaError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    materialize_loop(storage, path, bypass_heads_exist, after_walk).await
}

async fn materialize_loop<F, Fut>(
    storage: &Storage,
    path: &str,
    mut bypass_heads_exist: bool,
    mut after_walk: F,
) -> Result<Vec<mega_refs::Model>, MegaError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    for attempt in 0..=MATERIALIZE_RETRY_LIMIT {
        match materialize_once(storage, path, bypass_heads_exist, &mut after_walk).await? {
            OnceOutcome::Done(refs) => return Ok(refs),
            OnceOutcome::PathAbsent => return Ok(Vec::new()),
            OnceOutcome::Abandoned => {
                bypass_heads_exist = true;
                if attempt < MATERIALIZE_RETRY_LIMIT {
                    let backoff_ms = BACKOFF_BASE_MS.saturating_mul(1u64 << attempt);
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }
    Err(MegaError::MaterializeAborted)
}

async fn materialize_once<F, Fut>(
    storage: &Storage,
    path: &str,
    bypass_heads_exist: bool,
    after_walk: &mut F,
) -> Result<OnceOutcome, MegaError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let hash_kind = storage.config().monorepo.object_hash_kind()?;
    let mono = storage.mono_storage();
    if !bypass_heads_exist {
        let path_refs = mono.get_all_refs(path, false).await?;
        let heads_exist = path_refs
            .iter()
            .any(|x| x.ref_name == crate::common::utils::MEGA_BRANCH_NAME);
        if heads_exist {
            return Ok(OnceOutcome::Done(path_refs));
        }
    }

    let walked = match walk_root_refs(&mono, path, hash_kind).await? {
        Some(w) => w,
        None => return Ok(OnceOutcome::PathAbsent),
    };
    after_walk().await;

    let txn = mono.get_connection().begin().await?;
    PushQueueStorage::acquire_mono_write_lock(&txn).await?;
    match persist_walked_refs(&mono, &txn, path, &walked).await {
        Ok(inserted) => {
            txn.commit().await?;
            let mut refs = mono.get_all_refs(path, false).await?;
            if refs.is_empty() {
                refs = inserted;
            }
            Ok(OnceOutcome::Done(refs))
        }
        Err(PersistError::Abandoned) => {
            txn.rollback().await?;
            #[cfg(test)]
            ABANDON_COUNT.fetch_add(1, Ordering::SeqCst);
            tracing::warn!(event = "materialize_abandoned", "root identity mismatch");
            Ok(OnceOutcome::Abandoned)
        }
        Err(PersistError::Other(e)) => {
            let _ = txn.rollback().await;
            Err(e)
        }
    }
}

async fn walk_root_refs(
    mono: &MonoStorage,
    path: &str,
    hash_kind: HashKind,
) -> Result<Option<Vec<WalkedRoot>>, MegaError> {
    #[cfg(test)]
    WALK_COUNT.fetch_add(1, Ordering::SeqCst);

    let target_path = PathBuf::from(path);
    let root_refs = mono.get_all_refs("/", true).await?;
    let mut walked = Vec::with_capacity(root_refs.len());
    for root_ref in root_refs {
        let mut tree = tree_from_mega_model_with_kind(
            mono.get_tree_by_hash(&root_ref.ref_tree_hash)
                .await?
                .ok_or_else(|| MegaError::Other("root tree missing during materialize".into()))?,
            hash_kind,
        )?;
        let commit = commit_from_mega_model_with_kind(
            mono.get_commit_by_hash(&root_ref.ref_commit_hash)
                .await?
                .ok_or_else(|| MegaError::Other("root commit missing during materialize".into()))?,
            hash_kind,
        )?;
        for component in target_path.components() {
            if component == Component::RootDir {
                continue;
            }
            let path_compo_name = component
                .as_os_str()
                .to_str()
                .ok_or_else(|| MegaError::Other("path component is not valid UTF-8".into()))?;
            let Some(hash) = tree
                .tree_items
                .iter()
                .find(|x| x.name == path_compo_name)
                .map(|x| x.id)
            else {
                return Ok(None);
            };
            tree = tree_from_mega_model_with_kind(
                mono.get_tree_by_hash(&hash.to_string())
                    .await?
                    .ok_or_else(|| MegaError::Other("subtree missing during materialize".into()))?,
                hash_kind,
            )?;
        }
        walked.push(WalkedRoot {
            ref_name: root_ref.ref_name,
            root_commit_hash: root_ref.ref_commit_hash,
            root_tree_hash: root_ref.ref_tree_hash,
            subtree: tree,
            author: commit.author,
            committer: commit.committer,
            message: commit.message,
        });
    }
    Ok(Some(walked))
}

enum PersistError {
    Abandoned,
    Other(MegaError),
}

impl From<MegaError> for PersistError {
    fn from(value: MegaError) -> Self {
        PersistError::Other(value)
    }
}

async fn persist_walked_refs(
    mono: &MonoStorage,
    txn: &sea_orm::DatabaseTransaction,
    path: &str,
    walked: &[WalkedRoot],
) -> Result<Vec<mega_refs::Model>, PersistError> {
    for w in walked {
        let Some(current) = mono.get_ref_in_txn("/", &w.ref_name, txn).await? else {
            return Err(PersistError::Abandoned);
        };
        if current.ref_commit_hash != w.root_commit_hash
            || current.ref_tree_hash != w.root_tree_hash
        {
            return Err(PersistError::Abandoned);
        }
    }

    let mut refs = Vec::with_capacity(walked.len());
    for w in walked {
        let hash_kind = w.subtree.id.kind();
        let parents = mono
            .materialize_parents_in_txn_for_kind(path, &w.ref_name, txn, hash_kind)
            .await?;
        let continued = !parents.is_empty();
        // The root commit's raw message may carry its (server) `gpgsig`
        // header; a materialized path commit is unsigned, so it takes only
        // the body, framed with the header/body blank line (FU-02).
        let message = format_commit_msg(split_commit_message(&w.message).body, None);
        let c = Commit::new_with_kind(
            hash_kind,
            w.author.clone(),
            w.committer.clone(),
            w.subtree.id,
            parents,
            &message,
        )
        .map_err(MegaError::from)?;
        let commit_id = c.id.to_string();
        let tree_id = c.tree_id.to_string();
        let commit_model: crate::callisto::mega_commit::Model =
            c.into_mega_model(EntryMeta::default());
        mono.save_commit_in_txn(txn, commit_model.into_active_model())
            .await?;
        let computed = mega_refs::Model::new(path, w.ref_name.clone(), commit_id, tree_id, false);
        let persisted = mono.insert_ref_if_not_exists_in_txn(txn, computed).await?;
        if continued {
            mono.delete_tombstone_in_txn(path, &w.ref_name, txn).await?;
        }
        refs.push(persisted);
    }
    Ok(refs)
}

#[cfg(test)]
mod native_hash_tests {
    use git_internal::{
        hash::{HashKind, ObjectHash, get_hash_kind, set_hash_kind_for_test},
        internal::object::{ObjectTrait, types::ObjectType},
    };

    use super::*;
    use crate::{
        common::utils::MEGA_BRANCH_NAME,
        config::{MonoObjectFormat, testing::isolated_config},
        jupiter::{tests::test_storage_with_config, utils::converter::MegaModelConverter},
    };

    #[tokio::test]
    async fn native_hash_materialized_commit_id() {
        let _thread_kind = set_hash_kind_for_test(HashKind::Sha1);
        let temp = tempfile::tempdir().expect("private fixture");
        let mut config = isolated_config(temp.path().join("config"));
        config.monorepo.object_format = MonoObjectFormat::Sha256;
        config.monorepo.root_dirs = vec!["project".to_owned()];
        let graph = MegaModelConverter::init(&config.monorepo).expect("explicit SHA256 graph");
        assert_eq!(get_hash_kind(), HashKind::Sha1);
        let subtree_id = graph
            .root_tree
            .tree_items
            .iter()
            .find(|item| item.name == "project")
            .expect("project entry")
            .id;
        let subtree = graph
            .tree_maps
            .get(&subtree_id)
            .expect("project tree")
            .clone();
        assert_eq!(subtree.id.kind(), HashKind::Sha256);
        let root_commit = graph.commit.clone();
        let storage = test_storage_with_config(temp.path(), config).await;
        let mono = storage.mono_storage();
        let mut trees: Vec<_> = graph.tree_maps.into_values().collect();
        trees.push(graph.root_tree);
        mono.save_mega_trees(trees, root_commit.id, None)
            .await
            .expect("seed trees");
        mono.save_mega_commits(vec![root_commit.clone()], None)
            .await
            .expect("seed root commit");
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                root_commit.id.to_string(),
                root_commit.tree_id.to_string(),
                false,
            ),
            None,
        )
        .await
        .expect("seed root ref");
        let walked = WalkedRoot {
            ref_name: MEGA_BRANCH_NAME.to_owned(),
            root_commit_hash: root_commit.id.to_string(),
            root_tree_hash: root_commit.tree_id.to_string(),
            subtree,
            author: root_commit.author.clone(),
            committer: root_commit.committer.clone(),
            message: root_commit.message.clone(),
        };
        let txn = mono
            .get_connection()
            .begin()
            .await
            .expect("materialize transaction");
        PushQueueStorage::acquire_mono_write_lock(&txn)
            .await
            .expect("write lock");
        let refs = match persist_walked_refs(&mono, &txn, "/project", &[walked]).await {
            Ok(refs) => refs,
            Err(PersistError::Abandoned) => panic!("unexpected fixture root change"),
            Err(PersistError::Other(error)) => panic!("materialize fixture error: {error}"),
        };
        txn.commit().await.expect("persist materialized ref");
        assert_eq!(refs.len(), 1);
        let stored = mono
            .get_commit_by_hash(&refs[0].ref_commit_hash)
            .await
            .expect("read materialized commit")
            .expect("materialized commit");
        let canonical = Commit {
            id: ObjectHash::zero_for_kind(HashKind::Sha256),
            tree_id: subtree_id,
            parent_commit_ids: Vec::new(),
            author: root_commit.author,
            committer: root_commit.committer,
            message: stored.content.expect("materialized message"),
        };
        let expected = ObjectHash::from_type_and_data_for_kind(
            HashKind::Sha256,
            ObjectType::Commit,
            &canonical.to_data().expect("canonical materialized bytes"),
        )
        .expect("explicit commit hash");
        assert_eq!(
            stored.commit_id,
            expected.to_string(),
            "FIX15_MATERIALIZED_COMMIT_ID"
        );
        assert_eq!(
            get_hash_kind(),
            HashKind::Sha1,
            "materialize must preserve caller TLS"
        );
    }

    async fn native_hash_materialize_lifecycle(kind: HashKind, format: MonoObjectFormat) {
        use git_internal::internal::object::tree::{TreeItem, TreeItemMode};

        use crate::jupiter::utils::converter::commit_from_mega_model_with_kind;
        let _counter_lock = lock_materialize_tests().await;
        let other = if kind == HashKind::Sha1 {
            HashKind::Sha256
        } else {
            HashKind::Sha1
        };
        let _thread_kind = set_hash_kind_for_test(other);
        let temp = tempfile::tempdir().expect("private fixture");
        let mut config = isolated_config(temp.path().join("config"));
        config.monorepo.object_format = format;
        let blob = git_internal::internal::object::blob::Blob::from_content_bytes_with_kind(
            kind,
            b"native materialize fixture".to_vec(),
        )
        .expect("explicit blob");
        let subtree = Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(
                TreeItemMode::Blob,
                blob.id,
                "fixture.txt".to_owned(),
            )],
        )
        .expect("explicit subtree");
        let root_tree = Tree::from_tree_items_with_kind(
            kind,
            vec![TreeItem::new(
                TreeItemMode::Tree,
                subtree.id,
                "project".to_owned(),
            )],
        )
        .expect("explicit root");
        let author =
            Signature::from_data(b"author Fixture <native@example.invalid> 1 +0000".to_vec())
                .expect("author");
        let committer =
            Signature::from_data(b"committer Fixture <native@example.invalid> 1 +0000".to_vec())
                .expect("committer");
        let root = Commit::new_with_kind(
            kind,
            author,
            committer,
            root_tree.id,
            Vec::new(),
            "\nnative materialize fixture\n",
        )
        .expect("explicit commit");
        let storage = test_storage_with_config(temp.path(), config).await;
        let mono = storage.mono_storage();
        mono.save_mega_trees(vec![root_tree, subtree.clone()], root.id, None)
            .await
            .expect("seed trees");
        mono.save_mega_commits(vec![root.clone()], None)
            .await
            .expect("seed root");
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                root.id.to_string(),
                root.tree_id.to_string(),
                false,
            ),
            None,
        )
        .await
        .expect("seed root ref");
        let mut previous = None;
        for continuation in [false, true] {
            let refs = materialize_path_refs(&storage, "/project")
                .await
                .expect("real materialize entry");
            assert_eq!(refs.len(), 1);
            assert_eq!(refs[0].ref_tree_hash, subtree.id.to_string());
            let model = mono
                .get_commit_by_hash(&refs[0].ref_commit_hash)
                .await
                .expect("query commit")
                .expect("stored commit");
            let commit = commit_from_mega_model_with_kind(model, kind).expect("explicit decode");
            assert_eq!(commit.id.kind(), kind);
            assert_eq!(commit.tree_id, subtree.id);
            assert_eq!(
                commit.parent_commit_ids,
                previous.into_iter().collect::<Vec<_>>()
            );
            assert_eq!(
                commit.id,
                ObjectHash::from_type_and_data_for_kind(
                    kind,
                    ObjectType::Commit,
                    &commit.to_data().expect("canonical bytes")
                )
                .expect("canonical OID")
            );
            assert!(
                mono.get_tombstone("/project", MEGA_BRANCH_NAME)
                    .await
                    .expect("tombstone read")
                    .is_none()
            );
            assert_eq!(get_hash_kind(), other, "materialize preserves TLS");
            if !continuation {
                previous = Some(commit.id);
                let txn = mono
                    .get_connection()
                    .begin()
                    .await
                    .expect("tombstone transaction");
                PushQueueStorage::acquire_mono_write_lock(&txn)
                    .await
                    .expect("write lock");
                mono.tombstone_and_delete_main_ref_in_txn("/project", &txn)
                    .await
                    .expect("delete into tombstone");
                txn.commit().await.expect("commit tombstone");
                assert_eq!(
                    mono.get_tombstone("/project", MEGA_BRANCH_NAME)
                        .await
                        .expect("read tombstone")
                        .expect("tombstone")
                        .last_commit_hash,
                    commit.id.to_string()
                );
            } else {
                mono.remove_ref(refs[0].clone())
                    .await
                    .expect("remove own fixture ref");
            }
        }
        mono.upsert_tombstone(
            "/project",
            MEGA_BRANCH_NAME,
            "invalid-native-parent",
            &subtree.id.to_string(),
        )
        .await
        .expect("malformed fixture tombstone");
        let error = materialize_path_refs(&storage, "/project")
            .await
            .expect_err("invalid parent must fail closed");
        assert!(
            error
                .to_string()
                .contains("invalid native tombstone parent identity")
        );
        assert!(
            mono.get_all_refs("/project", false)
                .await
                .expect("query rejected ref")
                .is_empty()
        );
        assert_eq!(
            mono.get_tombstone("/project", MEGA_BRANCH_NAME)
                .await
                .expect("query preserved tombstone")
                .expect("preserved tombstone")
                .last_commit_hash,
            "invalid-native-parent"
        );
        assert_eq!(
            mono.get_main_ref("/")
                .await
                .expect("root ref")
                .expect("root")
                .ref_commit_hash,
            root.id.to_string()
        );
        assert_eq!(get_hash_kind(), other);
    }

    #[tokio::test]
    async fn native_hash_sha1_materialize_and_tombstone() {
        native_hash_materialize_lifecycle(HashKind::Sha1, MonoObjectFormat::Sha1).await;
    }

    #[tokio::test]
    async fn native_hash_sha256_materialize_and_tombstone() {
        native_hash_materialize_lifecycle(HashKind::Sha256, MonoObjectFormat::Sha256).await;
    }

    #[tokio::test]
    async fn native_hash_blake3_materialize_and_tombstone() {
        native_hash_materialize_lifecycle(HashKind::Blake3, MonoObjectFormat::Blake3).await;
    }
}
