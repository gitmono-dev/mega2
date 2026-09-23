#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    future::Future,
    path::{Component, PathBuf},
    time::Duration,
};

use git_internal::internal::{
    metadata::EntryMeta,
    object::{commit::Commit, signature::Signature, tree::Tree},
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
        utils::converter::{FromMegaModel, IntoMegaModel},
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

    let walked = match walk_root_refs(&mono, path).await? {
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
) -> Result<Option<Vec<WalkedRoot>>, MegaError> {
    #[cfg(test)]
    WALK_COUNT.fetch_add(1, Ordering::SeqCst);

    let target_path = PathBuf::from(path);
    let root_refs = mono.get_all_refs("/", true).await?;
    let mut walked = Vec::with_capacity(root_refs.len());
    for root_ref in root_refs {
        let mut tree: Tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&root_ref.ref_tree_hash)
                .await?
                .ok_or_else(|| MegaError::Other("root tree missing during materialize".into()))?,
        );
        let commit: Commit = Commit::from_mega_model(
            mono.get_commit_by_hash(&root_ref.ref_commit_hash)
                .await?
                .ok_or_else(|| MegaError::Other("root commit missing during materialize".into()))?,
        );
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
            tree =
                Tree::from_mega_model(mono.get_tree_by_hash(&hash.to_string()).await?.ok_or_else(
                    || MegaError::Other("subtree missing during materialize".into()),
                )?);
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
        let parents = mono
            .materialize_parents_in_txn(path, &w.ref_name, txn)
            .await?;
        let continued = !parents.is_empty();
        // The root commit's raw message may carry its (server) `gpgsig`
        // header; a materialized path commit is unsigned, so it takes only
        // the body, framed with the header/body blank line (FU-02).
        let message = format_commit_msg(split_commit_message(&w.message).body, None);
        let c = Commit::new(
            w.author.clone(),
            w.committer.clone(),
            w.subtree.id,
            parents,
            &message,
        );
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
