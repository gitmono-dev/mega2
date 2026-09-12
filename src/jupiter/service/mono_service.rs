use std::sync::Arc;

use futures::{StreamExt, stream};
use git_internal::internal::{
    metadata::{EntryMeta, MetaAttached},
    object::blob::Blob,
    pack::entry::Entry,
};
use sea_orm::{ActiveModelTrait, ConnectionTrait, IntoActiveModel, TransactionTrait};
use tokio::sync::Mutex;

use crate::{
    callisto::{mega_blob, mega_commit, mega_tag, mega_tree},
    common::{errors::MegaError, utils::is_full_hex_object_id},
    config::MonoConfig,
    jupiter::{
        service::git_service::GitService,
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            mono_storage::MonoStorage,
        },
        utils::converter::{IntoMegaModel, MegaModelConverter, MegaObjectModel, process_entry},
    },
};

#[derive(Clone)]
pub struct MonoService {
    pub mono_storage: MonoStorage,
    pub git_service: GitService,
}

#[derive(Debug, Default)]
pub struct GitMegaObjects {
    commits: Vec<mega_commit::ActiveModel>,
    trees: Vec<mega_tree::ActiveModel>,
    blobs: Vec<mega_blob::ActiveModel>,
    tags: Vec<mega_tag::ActiveModel>,
}

const MONOREPO_INITIALIZATION_LOCK_SQL: &str =
    "SELECT pg_advisory_xact_lock(1297043023, 1229867348)";

fn ensure_existing_root_ref_matches_config(
    mono_config: &MonoConfig,
    ref_commit_hash: &str,
    ref_tree_hash: &str,
) -> Result<(), MegaError> {
    let expected_hex_len = mono_config.object_hash_kind()?.hex_len();
    let matches_format = [ref_commit_hash, ref_tree_hash]
        .iter()
        .all(|object_id| object_id.len() == expected_hex_len && is_full_hex_object_id(object_id));

    if matches_format {
        return Ok(());
    }

    Err(MegaError::Other(format!(
        "existing Monorepo root ref is incompatible with monorepo.object_format={}; object-format changes do not convert existing repositories",
        mono_config.object_format.as_str()
    )))
}

async fn acquire_monorepo_initialization_lock(
    txn: &sea_orm::DatabaseTransaction,
) -> Result<(), MegaError> {
    // Monoengine accepts PostgreSQL only. The transaction-scoped lock releases
    // automatically on commit, rollback, or connection loss.
    txn.execute_unprepared(MONOREPO_INITIALIZATION_LOCK_SQL)
        .await?;
    Ok(())
}

impl MonoService {
    pub fn mock() -> Self {
        let mock = BaseStorage::mock();
        let mono_storage = MonoStorage { base: mock.clone() };
        let git_service = GitService::mock();

        Self {
            mono_storage,
            git_service,
        }
    }

    pub async fn init_monorepo(&self, mono_config: &MonoConfig) -> Result<(), MegaError> {
        mono_config.ensure_normal_service_object_format()?;
        self.initialize_monorepo(mono_config).await
    }

    /// Initializes an empty Monorepo for a controlled, one-shot bootstrap.
    ///
    /// Unlike [`Self::init_monorepo`], this accepts a configured SHA-256 or
    /// BLAKE3 initial object graph. It must not be followed by a normal Git
    /// service until protocol and pack paths carry an explicit repository hash
    /// context.
    pub(crate) async fn bootstrap_monorepo(
        &self,
        mono_config: &MonoConfig,
    ) -> Result<(), MegaError> {
        mono_config.object_hash_kind()?;
        self.initialize_monorepo(mono_config).await
    }

    async fn initialize_monorepo(&self, mono_config: &MonoConfig) -> Result<(), MegaError> {
        let txn = self.mono_storage.get_connection().begin().await?;
        acquire_monorepo_initialization_lock(&txn).await?;

        if let Some(root_ref) = self.mono_storage.get_main_ref_in_txn("/", &txn).await? {
            ensure_existing_root_ref_matches_config(
                mono_config,
                &root_ref.ref_commit_hash,
                &root_ref.ref_tree_hash,
            )?;
            txn.commit().await?;
            tracing::info!("Monorepo Directory Already Inited, skip init process!");
            return Ok(());
        }
        let converter = MegaModelConverter::init(mono_config)?;
        let commit = converter
            .commit
            .into_mega_model(EntryMeta::default())
            .into_active_model();

        self.mono_storage
            .batch_save_model_with_txn(vec![commit], Some(&txn))
            .await?;
        converter.refs.insert(&txn).await?;

        let mega_trees = converter.mega_trees.borrow().values().cloned().collect();
        self.mono_storage
            .batch_save_model_with_txn(mega_trees, Some(&txn))
            .await?;
        let mega_blobs = converter.mega_blobs.borrow().values().cloned().collect();
        let raw_blobs = converter.raw_blobs.into_inner();
        self.git_service.put_objects(raw_blobs).await?;
        self.mono_storage
            .batch_save_model_with_txn(mega_blobs, Some(&txn))
            .await?;

        let root_ref = self
            .mono_storage
            .get_main_ref_in_txn("/", &txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(
                    "Monorepo initializer did not persist a root ref; refusing to report success"
                        .to_string(),
                )
            })?;
        ensure_existing_root_ref_matches_config(
            mono_config,
            &root_ref.ref_commit_hash,
            &root_ref.ref_tree_hash,
        )?;
        Ok(txn.commit().await?)
    }

    /// `commit_id` follows ADR-MC-06 attribution: the caller passes the push
    /// chain tip (`RefCommand.new_id`), which is stamped on every tree/blob of
    /// the batch. It means "the chain tip of the push that last touched the
    /// object", not the commit that introduced it. Live reader: the file
    /// browser's "last commit" column (`item_to_commit_map`); exactly correct
    /// under single-commit-per-push, approximate once MC-06 opens multi-commit
    /// pushes. Any new reader must revisit ADR-MC-06 first.
    pub async fn save_entry(
        &self,
        commit_id: &str,
        entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<Vec<mega_commit::ActiveModel>, MegaError> {
        let git_objects = Arc::new(Mutex::new(GitMegaObjects::default()));

        let results: Vec<Result<(), MegaError>> = stream::iter(entry_list)
            .map(|entry| {
                let git_objects = git_objects.clone();
                async move {
                    let raw_obj = process_entry(entry.inner);

                    let model = raw_obj.convert_to_mega_model(entry.meta);
                    match model {
                        MegaObjectModel::Commit(commit) => git_objects
                            .lock()
                            .await
                            .commits
                            .push(commit.into_active_model()),
                        MegaObjectModel::Tree(mut tree) => {
                            commit_id.clone_into(&mut tree.commit_id);
                            git_objects
                                .lock()
                                .await
                                .trees
                                .push(tree.into_active_model());
                        }
                        MegaObjectModel::Blob(mut blob, raw) => {
                            commit_id.clone_into(&mut blob.commit_id);

                            self.git_service
                                .save_object_from_model(raw, &blob.blob_id)
                                .await?;
                            git_objects
                                .lock()
                                .await
                                .blobs
                                .push(blob.into_active_model());
                        }
                        MegaObjectModel::Tag(tag) => {
                            git_objects.lock().await.tags.push(tag.into_active_model())
                        }
                    }
                    Ok(())
                }
            })
            .buffer_unordered(16)
            .collect()
            .await;

        if let Some(err) = results.into_iter().find_map(Result::err) {
            tracing::error!("at least one blob upload to object storage failed");
            return Err(err);
        }

        let git_objects = Arc::try_unwrap(git_objects)
            .expect("Failed to unwrap Arc")
            .into_inner();

        self.mono_storage
            .batch_save_model(git_objects.commits.clone())
            .await?;
        self.mono_storage
            .batch_save_model(git_objects.trees)
            .await?;
        self.mono_storage
            .batch_save_model(git_objects.blobs)
            .await?;
        self.mono_storage.batch_save_model(git_objects.tags).await?;

        Ok(git_objects.commits)
    }

    /// Writes blobs to object storage (S3) first, then to DB.
    /// This order avoids leaving blob rows in DB when S3 write fails (same as save_entry).
    pub async fn save_blobs(&self, commit_id: &str, blobs: Vec<Blob>) -> Result<(), MegaError> {
        let mega_blobs: Vec<mega_blob::ActiveModel> = blobs
            .iter()
            .map(|b| (*b).clone().into_mega_model(EntryMeta::default()))
            .map(|mut m: mega_blob::Model| {
                m.commit_id = commit_id.to_owned();
                m.into_active_model()
            })
            .collect();
        self.git_service.put_objects(blobs).await?;
        self.mono_storage.batch_save_model(mega_blobs).await
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;

    use super::{MonoService, ensure_existing_root_ref_matches_config};
    use crate::{
        callisto::{mega_blob, mega_commit, mega_tree},
        common::utils::is_full_hex_object_id,
        config::{MonoConfig, MonoObjectFormat},
        jupiter::{
            service::git_service::GitService,
            storage::{base_storage::StorageConnector, object_storage::mock_object_storage},
            tests::test_storage,
        },
    };

    #[tokio::test]
    async fn normal_service_initialization_rejects_non_sha1_before_storage_access() {
        let sha256_config = MonoConfig {
            object_format: MonoObjectFormat::Sha256,
            ..Default::default()
        };
        let err = MonoService::mock()
            .init_monorepo(&sha256_config)
            .await
            .expect_err("SHA-256 must use the controlled bootstrap entrypoint");
        assert!(err.to_string().contains("bootstrap-only"));

        let mono_config = MonoConfig {
            object_format: MonoObjectFormat::Blake3,
            ..Default::default()
        };

        let err = MonoService::mock()
            .init_monorepo(&mono_config)
            .await
            .expect_err("BLAKE3 must use the controlled bootstrap entrypoint");
        assert!(err.to_string().contains("bootstrap-only"));
    }

    #[test]
    fn existing_root_ref_must_match_configured_object_format() {
        let sha1 = MonoConfig::default();
        assert!(
            ensure_existing_root_ref_matches_config(&sha1, &"a".repeat(40), &"b".repeat(40),)
                .is_ok()
        );

        let sha256 = MonoConfig {
            object_format: MonoObjectFormat::Sha256,
            ..Default::default()
        };
        assert!(
            ensure_existing_root_ref_matches_config(&sha256, &"a".repeat(64), &"b".repeat(64),)
                .is_ok()
        );

        let err =
            ensure_existing_root_ref_matches_config(&sha256, &"a".repeat(40), &"b".repeat(40))
                .expect_err("a SHA-1 root ref must not be accepted as SHA-256");
        assert!(err.to_string().contains("do not convert"));

        let blake3 = MonoConfig {
            object_format: MonoObjectFormat::Blake3,
            ..Default::default()
        };
        assert!(
            ensure_existing_root_ref_matches_config(&blake3, &"a".repeat(64), &"b".repeat(64),)
                .is_ok()
        );
        let err =
            ensure_existing_root_ref_matches_config(&blake3, &"a".repeat(40), &"b".repeat(40))
                .expect_err("a SHA-1 root ref must not be accepted as BLAKE3");
        assert!(err.to_string().contains("do not convert"));
    }

    #[tokio::test]
    async fn controlled_bootstrap_persists_sha256_initial_graph() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut storage = test_storage(temp_dir.path()).await;
        let mono_storage = storage.mono_storage();
        let git_service = GitService {
            obj_storage: mock_object_storage(),
        };
        storage.mono_service = MonoService {
            mono_storage: mono_storage.clone(),
            git_service: git_service.clone(),
        };
        let mono_config = MonoConfig {
            object_format: MonoObjectFormat::Sha256,
            ..Default::default()
        };

        storage
            .mono_service
            .bootstrap_monorepo(&mono_config)
            .await
            .expect("SHA-256 controlled bootstrap");

        let root_ref = mono_storage
            .get_main_ref("/")
            .await
            .expect("read root ref")
            .expect("root ref should be persisted");
        let db = mono_storage.get_connection();
        let commits = mega_commit::Entity::find()
            .all(db)
            .await
            .expect("read commits");
        let trees = mega_tree::Entity::find().all(db).await.expect("read trees");
        let blobs = mega_blob::Entity::find().all(db).await.expect("read blobs");

        assert_eq!(commits.len(), 1);
        assert!(!trees.is_empty());
        assert!(!blobs.is_empty());
        assert!(
            [
                root_ref.ref_commit_hash.as_str(),
                root_ref.ref_tree_hash.as_str(),
            ]
            .into_iter()
            .chain(
                commits
                    .iter()
                    .flat_map(|commit| { [commit.commit_id.as_str(), commit.tree.as_str()] })
            )
            .chain(trees.iter().map(|tree| tree.tree_id.as_str()))
            .chain(blobs.iter().map(|blob| blob.blob_id.as_str()))
            .all(|object_id| object_id.len() == 64 && is_full_hex_object_id(object_id))
        );

        git_service
            .get_object_as_bytes(&blobs[0].blob_id)
            .await
            .expect("a persisted SHA-256 blob should be addressable in object storage");
    }

    #[tokio::test]
    async fn concurrent_bootstraps_do_not_mix_object_formats() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut storage = test_storage(temp_dir.path()).await;
        let mono_storage = storage.mono_storage();
        storage.mono_service = MonoService {
            mono_storage: mono_storage.clone(),
            git_service: GitService {
                obj_storage: mock_object_storage(),
            },
        };

        let sha1_config = MonoConfig::default();
        let sha256_config = MonoConfig {
            object_format: MonoObjectFormat::Sha256,
            ..Default::default()
        };
        let sha1_service = storage.mono_service.clone();
        let sha256_service = storage.mono_service.clone();

        let (sha1_result, sha256_result) = tokio::join!(
            sha1_service.bootstrap_monorepo(&sha1_config),
            sha256_service.bootstrap_monorepo(&sha256_config),
        );

        let sha1_succeeded = sha1_result.is_ok();
        let sha256_succeeded = sha256_result.is_ok();
        assert_ne!(
            sha1_succeeded, sha256_succeeded,
            "exactly one configured initial graph must win the transaction lock"
        );
        let losing_result = if sha1_succeeded {
            sha256_result
        } else {
            sha1_result
        };
        assert!(
            losing_result
                .expect_err("the losing object format must fail closed")
                .to_string()
                .contains("incompatible")
        );

        let expected_hex_len = if sha1_succeeded { 40 } else { 64 };
        let root_ref = mono_storage
            .get_main_ref("/")
            .await
            .expect("read root ref")
            .expect("root ref should be persisted");
        let db = mono_storage.get_connection();
        let commits = mega_commit::Entity::find()
            .all(db)
            .await
            .expect("read commits");
        let trees = mega_tree::Entity::find().all(db).await.expect("read trees");
        let blobs = mega_blob::Entity::find().all(db).await.expect("read blobs");

        assert_eq!(commits.len(), 1);
        assert!(
            [
                root_ref.ref_commit_hash.as_str(),
                root_ref.ref_tree_hash.as_str(),
            ]
            .into_iter()
            .chain(
                commits
                    .iter()
                    .flat_map(|commit| { [commit.commit_id.as_str(), commit.tree.as_str()] })
            )
            .chain(trees.iter().map(|tree| tree.tree_id.as_str()))
            .chain(blobs.iter().map(|blob| blob.blob_id.as_str()))
            .all(|object_id| {
                object_id.len() == expected_hex_len && is_full_hex_object_id(object_id)
            })
        );
    }
}
