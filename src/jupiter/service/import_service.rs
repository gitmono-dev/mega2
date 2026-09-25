use std::time::Duration;

use futures::{StreamExt, stream};
use git_internal::internal::{
    metadata::{EntryMeta, MetaAttached},
    pack::entry::Entry,
};
use sea_orm::{ActiveModelTrait, DatabaseTransaction, IntoActiveModel, TransactionTrait};

use crate::{
    callisto::{git_blob, git_commit, git_tag, git_tree},
    common::errors::MegaError,
    jupiter::{
        service::git_service::GitService,
        storage::{
            base_storage::{BaseStorage, InsertRetry, StorageConnector, next_insert_retry},
            git_db_storage::GitDbStorage,
        },
        utils::converter::{GitObjectModel, process_entry},
    },
};

#[derive(Clone)]
pub struct ImportService {
    pub git_db_storage: GitDbStorage,
    pub git_service: GitService,
}

/// The rows of one unpacked batch, each kind in the order of its unique key
/// (a fixed lock order for concurrent batches of the same repository).
#[derive(Debug, Default)]
pub struct GitObjects {
    commits: Vec<git_commit::Model>,
    trees: Vec<git_tree::Model>,
    blobs: Vec<git_blob::Model>,
    tags: Vec<git_tag::Model>,
}

impl GitObjects {
    fn is_empty(&self) -> bool {
        self.commits.is_empty()
            && self.trees.is_empty()
            && self.blobs.is_empty()
            && self.tags.is_empty()
    }
}

impl ImportService {
    pub fn mock() -> Self {
        let mock = BaseStorage::mock();
        let git_db_storage = GitDbStorage { base: mock.clone() };
        let git_service = GitService::mock();

        Self {
            git_db_storage,
            git_service,
        }
    }

    /// Save one unpacked batch of the ImportRepo `(repo_id, repo_path)`
    /// (plan-20260923 ADR-FU-09 item 5). Blob bytes go to the object store
    /// first, outside any transaction; the rows then land in one transaction
    /// that takes the repository's liveness lock first, so a repository
    /// removed meanwhile gets `IMPORT_REPO_REMOVED` and no row. A deadlock or
    /// serialization failure retries the whole transaction.
    pub async fn save_entry(
        &self,
        repo_id: i64,
        repo_path: &str,
        entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<(), MegaError> {
        let objects = self.prepare_objects(repo_id, entry_list).await?;
        if objects.is_empty() {
            return Ok(());
        }
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let txn = self.git_db_storage.get_connection().begin().await?;
            let error = match self
                .insert_objects_in_txn(&txn, repo_id, repo_path, &objects)
                .await
            {
                Ok(()) => match txn.commit().await {
                    Ok(()) => return Ok(()),
                    Err(error) => MegaError::Db(error),
                },
                Err(error) => {
                    if let Err(rollback) = txn.rollback().await {
                        tracing::warn!(error = %rollback, "rolling back an import batch failed");
                    }
                    error
                }
            };
            // A deadlock or serialization failure, at a statement or at
            // commit, retries the whole transaction.
            let Some(backoff_ms) = batch_retry_backoff(attempt, &error) else {
                return Err(error);
            };
            tracing::warn!(
                attempt,
                backoff_ms,
                error_kind = "deadlock_or_serialization",
                "retrying an import batch transaction"
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }
    }

    /// Convert a batch to rows of `repo_id`, uploading blob bytes on the way.
    pub(crate) async fn prepare_objects(
        &self,
        repo_id: i64,
        entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<GitObjects, MegaError> {
        let models: Vec<Result<GitObjectModel, MegaError>> = stream::iter(entry_list)
            .map(|entry| async move {
                let raw_obj = process_entry(entry.inner);
                Ok(match raw_obj.convert_to_git_model(entry.meta) {
                    GitObjectModel::Commit(mut commit) => {
                        commit.repo_id = repo_id;
                        GitObjectModel::Commit(commit)
                    }
                    GitObjectModel::Tree(mut tree) => {
                        tree.repo_id = repo_id;
                        GitObjectModel::Tree(tree)
                    }
                    GitObjectModel::Blob(mut blob, raw) => {
                        blob.repo_id = repo_id;
                        self.git_service
                            .save_object_from_model(raw, &blob.blob_id)
                            .await?;
                        GitObjectModel::Blob(blob, Vec::new())
                    }
                    GitObjectModel::Tag(mut tag) => {
                        tag.repo_id = repo_id;
                        GitObjectModel::Tag(tag)
                    }
                })
            })
            .buffer_unordered(16)
            .collect()
            .await;

        let mut objects = GitObjects::default();
        for model in models {
            match model? {
                GitObjectModel::Commit(commit) => objects.commits.push(commit),
                GitObjectModel::Tree(tree) => objects.trees.push(tree),
                GitObjectModel::Blob(blob, _) => objects.blobs.push(blob),
                GitObjectModel::Tag(tag) => objects.tags.push(tag),
            }
        }
        objects
            .commits
            .sort_by(|a, b| a.commit_id.cmp(&b.commit_id));
        objects.trees.sort_by(|a, b| a.tree_id.cmp(&b.tree_id));
        objects.blobs.sort_by(|a, b| a.blob_id.cmp(&b.blob_id));
        objects.tags.sort_by(|a, b| a.tag_id.cmp(&b.tag_id));
        Ok(objects)
    }

    /// The row writes of `save_entry` inside `txn`: the liveness lock, then
    /// the four tables in a fixed order.
    pub(crate) async fn insert_objects_in_txn(
        &self,
        txn: &DatabaseTransaction,
        repo_id: i64,
        repo_path: &str,
        objects: &GitObjects,
    ) -> Result<(), MegaError> {
        let git_db = &self.git_db_storage;
        git_db
            .lock_live_import_repo(txn, repo_id, repo_path)
            .await?;
        git_db
            .insert_import_objects_in_txn(active(&objects.commits), txn)
            .await?;
        git_db
            .insert_import_objects_in_txn(active(&objects.trees), txn)
            .await?;
        git_db
            .insert_import_objects_in_txn(active(&objects.blobs), txn)
            .await?;
        git_db
            .insert_import_objects_in_txn(active(&objects.tags), txn)
            .await?;
        Ok(())
    }
}

/// The wait before retrying a failed batch transaction: only a database
/// deadlock or serialization failure, within `next_insert_retry`'s attempt
/// budget. A typed refusal is never retried, whatever its text.
fn batch_retry_backoff(attempt: u32, error: &MegaError) -> Option<u64> {
    let MegaError::Db(db_error) = error else {
        return None;
    };
    match next_insert_retry(attempt, db_error) {
        InsertRetry::Sleep(backoff_ms) => Some(backoff_ms),
        InsertRetry::Ok | InsertRetry::Fail => None,
    }
}

fn active<M: IntoActiveModel<A> + Clone, A: ActiveModelTrait>(models: &[M]) -> Vec<A> {
    models
        .iter()
        .cloned()
        .map(IntoActiveModel::into_active_model)
        .collect()
}

#[cfg(test)]
mod tests {
    use git_internal::internal::object::{
        blob::Blob,
        commit::Commit,
        signature::{Signature, SignatureType},
        tag::Tag,
        tree::{Tree, TreeItem, TreeItemMode},
        types::ObjectType,
    };
    use sea_orm::TransactionTrait;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{git_repo, import_refs, sea_orm_active_enums::RefTypeEnum},
        ceres::{
            pack::import_repo::{RemoveOutcome, detach_import_repo, remove_import_repo},
            protocol::repo::Repo,
        },
        common::errors::ImportRepoError,
        jupiter::storage::git_db_storage::fu18_support::{
            blocked_by_me, object_counts, park_detach, single_connection, test_cache, wired_storage,
        },
    };

    /// One of each object kind (blob, tree, commit, annotated tag).
    fn entries(seed: &str) -> Vec<MetaAttached<Entry, EntryMeta>> {
        let blob = Blob::from_content(&format!("fu18 {seed}"));
        let tree = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: blob.id,
            name: "README.md".to_string(),
        }])
        .unwrap();
        let commit = Commit::from_tree_id(tree.id, vec![], &format!("fu18 {seed}"));
        let tag = Tag::new(
            commit.id,
            ObjectType::Commit,
            format!("v-{seed}"),
            Signature::new(SignatureType::Tagger, "fu18".to_string(), String::new()),
            format!("fu18 {seed}"),
        );
        [
            Entry::from(blob),
            Entry::from(tree),
            Entry::from(commit),
            Entry::from(tag),
        ]
        .into_iter()
        .map(|inner| MetaAttached {
            inner,
            meta: EntryMeta::new(),
        })
        .collect()
    }

    async fn register(storage: &crate::jupiter::storage::Storage, path: &str) -> git_repo::Model {
        let model: git_repo::Model = Repo::new(std::path::PathBuf::from(path), false)
            .unwrap()
            .into();
        storage
            .git_db_storage()
            .register_import_repo(model.clone())
            .await
            .unwrap();
        model
    }

    fn is_removed(result: &Result<(), MegaError>, path: &str) -> bool {
        matches!(
            result,
            Err(MegaError::ImportRepo(ImportRepoError::Removed { path: p })) if p == path
        )
    }

    #[tokio::test]
    async fn fu18_object_writes_fenced_after_detach() {
        let temp = TempDir::new().unwrap();
        let storage = wired_storage(temp.path()).await;
        let import = storage.import_service.clone();
        let conn = storage.git_db_storage().get_connection().clone();

        // (A) After detach and sweep, a stale batch lands no row.
        let p = "/third-party/fu18-objects";
        let r1 = register(&storage, p).await;
        import.save_entry(r1.id, p, entries("one")).await.unwrap();
        assert_eq!(object_counts(&conn, r1.id).await, [1, 1, 1, 1]);
        assert!(matches!(
            remove_import_repo(&storage, test_cache().await, p, None, None)
                .await
                .unwrap(),
            RemoveOutcome::Removed { repo_id, .. } if repo_id == r1.id
        ));
        assert_eq!(object_counts(&conn, r1.id).await, [0, 0, 0, 0]);
        let stale = import.save_entry(r1.id, p, entries("two")).await;
        assert!(is_removed(&stale, p), "{stale:?}");
        assert_eq!(object_counts(&conn, r1.id).await, [0, 0, 0, 0]);

        // (B) A batch holding the lock first: the removal waits for it, then
        // sweeps its committed rows.
        let p2 = "/third-party/fu18-objects-b";
        let r2 = register(&storage, p2).await;
        let objects = import.prepare_objects(r2.id, entries("one")).await.unwrap();
        let batch = conn.begin().await.unwrap();
        import
            .insert_objects_in_txn(&batch, r2.id, p2, &objects)
            .await
            .unwrap();
        let removing_storage = storage.clone();
        let removing = tokio::spawn(async move {
            remove_import_repo(
                &removing_storage,
                test_cache().await,
                "/third-party/fu18-objects-b",
                None,
                None,
            )
            .await
        });
        assert!(
            blocked_by_me(&batch, false).await,
            "the removal waits on the batch"
        );
        batch.commit().await.unwrap();
        assert!(matches!(
            removing.await.unwrap().unwrap(),
            RemoveOutcome::Removed { repo_id, .. } if repo_id == r2.id
        ));
        assert_eq!(object_counts(&conn, r2.id).await, [0, 0, 0, 0]);

        // (C) The detach holding the row first (parked at its import_refs
        // delete): the batch waits on it, then lands no row.
        let p3 = "/third-party/fu18-objects-c";
        let r3 = register(&storage, p3).await;
        storage
            .git_db_storage()
            .save_ref(
                r3.id,
                import_refs::Model {
                    id: crate::common::utils::generate_id(),
                    repo_id: r3.id,
                    ref_name: "refs/heads/main".to_string(),
                    ref_git_id: "c".repeat(40),
                    ref_type: RefTypeEnum::Branch,
                    default_branch: true,
                    created_at: chrono::Utc::now().naive_utc(),
                    updated_at: chrono::Utc::now().naive_utc(),
                },
            )
            .await
            .unwrap();
        let before = object_counts(&conn, r3.id).await;
        let single = single_connection(&conn).await;
        let parked = park_detach(&single, r3.id).await;
        let detach_storage = storage.clone();
        let r3_id = r3.id;
        let detaching = tokio::spawn(async move {
            detach_import_repo(
                &detach_storage,
                test_cache().await,
                r3_id,
                "/third-party/fu18-objects-c",
                None,
            )
            .await
        });
        assert!(
            blocked_by_me(&parked, false).await,
            "the detach parks at its import_refs delete"
        );
        let saving_import = import.clone();
        let saving = tokio::spawn(async move {
            saving_import
                .save_entry(r3_id, "/third-party/fu18-objects-c", entries("one"))
                .await
        });
        assert!(
            blocked_by_me(&parked, true).await,
            "the batch waits on the detach"
        );
        parked.commit().await.unwrap();
        assert!(detaching.await.unwrap().unwrap().is_some());
        let refused = saving.await.unwrap();
        assert!(is_removed(&refused, p3), "{refused:?}");
        assert_eq!(object_counts(&conn, r3.id).await, before);

        // (E) A re-import at the first path writes as usual.
        let r4 = register(&storage, p).await;
        import.save_entry(r4.id, p, entries("one")).await.unwrap();
        assert_eq!(object_counts(&conn, r4.id).await, [1, 1, 1, 1]);
    }

    #[test]
    fn fu18_batch_retry_only_on_database_conflicts() {
        let deadlock = || {
            MegaError::Db(sea_orm::DbErr::Custom(
                "ERROR: 40P01 deadlock detected".into(),
            ))
        };
        for attempt in 1..=4 {
            assert_eq!(
                batch_retry_backoff(attempt, &deadlock()),
                Some(10 << (attempt - 1)),
                "attempt {attempt}"
            );
        }
        assert_eq!(
            batch_retry_backoff(5, &deadlock()),
            None,
            "five attempts at most"
        );
        let removed = MegaError::ImportRepo(ImportRepoError::Removed {
            path: "/third-party/40001-deadlock".into(),
        });
        assert_eq!(
            batch_retry_backoff(1, &removed),
            None,
            "a refusal is never retried"
        );
        assert_eq!(
            batch_retry_backoff(1, &MegaError::Other("deadlock detected".into())),
            None
        );
        assert_eq!(
            batch_retry_backoff(
                1,
                &MegaError::Db(sea_orm::DbErr::Custom(
                    "duplicate key value violates unique constraint".into()
                ))
            ),
            None
        );
    }
}
