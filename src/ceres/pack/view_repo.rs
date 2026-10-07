use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use git_internal::{
    errors::GitError,
    hash::HashKind,
    internal::{
        metadata::{EntryMeta, MetaAttached},
        object::tree::Tree,
        pack::entry::Entry,
    },
};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    ceres::{
        pack::RepoHandler,
        protocol::import_refs::{RefCommand, Refs},
        view::tree_source::TreeSource,
    },
    common::{
        errors::{MegaError, ViewUnavailableReason},
        utils::{MEGA_BRANCH_NAME, ZERO_ID},
    },
    config::Config,
    jupiter::{
        service::view_projection_service::{CatchUpOutcome, ViewProjectionService},
        storage::{
            Storage, base_storage::StorageConnector, view_projection_storage::ViewReaderState,
            view_root_chain::RootChainOutcome, view_storage::ViewLockMode,
            view_tree_source::ViewTreeSource,
        },
    },
};
#[rustfmt::skip]
use crate::orbit_api::object_storage::MultiObjectByteStream;

pub(crate) struct ViewRepo {
    storage: Storage,
    config: Arc<Config>,
    projection: ViewProjectionService,
    filter_pk: i64,
    filter_id: String,
}

impl ViewRepo {
    pub(crate) fn new(
        storage: Storage,
        config: Arc<Config>,
        projection: ViewProjectionService,
        filter_pk: i64,
        filter_id: String,
    ) -> Self {
        Self {
            storage,
            config,
            projection,
            filter_pk,
            filter_id,
        }
    }

    fn unavailable(&self, reason: ViewUnavailableReason) -> MegaError {
        MegaError::ViewUnavailable {
            filter_id: self.filter_id.clone(),
            reason,
        }
    }

    fn ensure_ready(&self, state: &ViewReaderState) -> Result<(), MegaError> {
        if state.halted {
            Err(self.unavailable(ViewUnavailableReason::RootChainHalted))
        } else if state.ready_seq.is_none() {
            Err(self.unavailable(ViewUnavailableReason::WarmingUp))
        } else {
            Ok(())
        }
    }

    async fn sync_catch_up(&self) -> Option<Result<CatchUpOutcome, MegaError>> {
        let limit = self.config.views.sync_catch_up_commits as usize;
        if limit == 0 {
            return None;
        }
        let outcome = self
            .projection
            .catch_up_one_batch(self.filter_pk, &self.config, limit)
            .await;
        if matches!(outcome, Ok(CatchUpOutcome::MainNotCovered)) {
            self.storage.view_signal().notify_worker();
        }
        Some(outcome)
    }
}

fn read_only_mega_error() -> MegaError {
    MegaError::Other("view URLs are read-only".to_owned())
}

fn read_only_git_error() -> GitError {
    GitError::CustomError("view URLs are read-only".to_owned())
}

#[async_trait]
impl RepoHandler for ViewRepo {
    fn is_monorepo(&self) -> bool {
        true
    }

    fn object_hash_kind(&self) -> Result<HashKind, MegaError> {
        self.config.monorepo.object_hash_kind()
    }

    async fn refs_with_head_hash(&self) -> Result<(String, Vec<Refs>), MegaError> {
        let view_storage = self.storage.view_storage();
        let first = view_storage
            .view_reader_state(self.filter_pk)
            .await?
            .ok_or_else(|| self.unavailable(ViewUnavailableReason::WarmingUp))?;
        self.ensure_ready(&first)?;

        match view_storage
            .extend_root_chain(
                Some(self.config.views.max_append_walk as usize),
                self.config.views.batch_size as usize,
                ViewLockMode::Blocking,
            )
            .await
        {
            Ok(RootChainOutcome::CaughtUp) => {
                let _ = self.sync_catch_up().await;
            }
            Ok(RootChainOutcome::NotCaughtUp) => {
                self.storage.view_signal().notify_worker();
            }
            Ok(RootChainOutcome::Discontinuous(_)) => {
                return Err(self.unavailable(ViewUnavailableReason::RootChainHalted));
            }
            Err(error) => {
                tracing::warn!(filter_id = %self.filter_id, %error, "view advertise root-chain extension failed");
            }
        }

        let second = view_storage
            .view_reader_state(self.filter_pk)
            .await?
            .ok_or_else(|| self.unavailable(ViewUnavailableReason::WarmingUp))?;
        self.ensure_ready(&second)?;
        match second.view_tip {
            Some(tip) => Ok((
                tip.clone(),
                vec![Refs {
                    id: self.filter_pk,
                    ref_name: MEGA_BRANCH_NAME.to_owned(),
                    ref_hash: tip,
                    default_branch: true,
                }],
            )),
            None => Ok((ZERO_ID.to_owned(), Vec::new())),
        }
    }

    async fn check_wants_and_ready(&self, want: &[String]) -> Result<(), MegaError> {
        let state = self
            .storage
            .view_storage()
            .view_want_state(self.filter_pk, want)
            .await?
            .ok_or_else(|| self.unavailable(ViewUnavailableReason::WarmingUp))?;
        if state.halted {
            return Err(self.unavailable(ViewUnavailableReason::RootChainHalted));
        }
        if state.ready_seq.is_none() {
            return Err(self.unavailable(ViewUnavailableReason::WarmingUp));
        }
        if state.wants.len() != want.len() {
            return Err(MegaError::Other(
                "view want state returned an incomplete result".to_owned(),
            ));
        }
        for (oid, seq_from) in state.wants {
            if seq_from.is_none_or(|seq| seq > state.projected_seq) {
                return Err(MegaError::ViewPackRejected(format!(
                    "upload-pack: not our ref {oid}"
                )));
            }
        }
        Ok(())
    }

    async fn finalize_receive_pack(&self) -> Result<(), MegaError> {
        Err(read_only_mega_error())
    }

    async fn save_entry(
        &self,
        _entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<(), MegaError> {
        Err(read_only_mega_error())
    }

    async fn update_pack_id(&self, _temp_pack_id: &str, _pack_id: &str) -> Result<(), MegaError> {
        Err(read_only_mega_error())
    }

    async fn check_entry(&self, _entry: &Entry) -> Result<(), GitError> {
        Err(read_only_git_error())
    }

    async fn full_pack(&self, _want: Vec<String>) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        Err(GitError::CustomError(
            "view pack not implemented".to_owned(),
        ))
    }

    async fn incremental_pack(
        &self,
        _want: Vec<String>,
        _have: Vec<String>,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        Err(GitError::CustomError(
            "view pack not implemented".to_owned(),
        ))
    }

    async fn get_trees_by_hashes(&self, hashes: Vec<String>) -> Result<Vec<Tree>, MegaError> {
        let mono = self.storage.mono_storage();
        let view_storage = self.storage.view_storage();
        let mut source = ViewTreeSource::new(
            &mono,
            view_storage.get_connection(),
            self.object_hash_kind()?,
        )?;
        source.prefetch(&hashes).await?;
        hashes
            .iter()
            .map(|hash| {
                source
                    .read_tree(hash)
                    .map_err(|missing| MegaError::Other(format!("view tree {missing:?}")))
            })
            .collect()
    }

    async fn get_blobs_by_hashes(
        &self,
        hashes: Vec<String>,
    ) -> Result<MultiObjectByteStream<'_>, MegaError> {
        Ok(self.storage.git_service.get_objects_stream(hashes))
    }

    async fn get_blob_metadata_by_hashes(
        &self,
        hashes: Vec<String>,
    ) -> Result<HashMap<String, EntryMeta>, MegaError> {
        let blobs = self
            .storage
            .mono_storage()
            .get_mega_blobs_by_hashes(hashes)
            .await?;
        Ok(blobs
            .into_iter()
            .map(|blob| {
                (
                    blob.blob_id,
                    EntryMeta {
                        pack_id: Some(blob.pack_id),
                        pack_offset: Some(blob.pack_offset as usize),
                        file_path: Some(blob.file_path),
                        is_delta: Some(blob.is_delta_in_pack),
                        crc32: None,
                    },
                )
            })
            .collect())
    }

    async fn update_refs(&self, _refs: &RefCommand) -> Result<(), GitError> {
        Err(read_only_git_error())
    }

    async fn check_commit_exist(&self, hash: &str) -> bool {
        self.storage
            .view_storage()
            .view_commit_exists(self.filter_pk, hash)
            .await
    }

    async fn check_object_exist(&self, _hash: &str) -> bool {
        false
    }

    async fn check_default_branch(&self) -> bool {
        true
    }

    async fn traverses_tree_and_update_filepath(&self) -> Result<(), MegaError> {
        Err(read_only_mega_error())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use chrono::Utc;
    use futures::TryStreamExt;
    use git_internal::{hash::HashKind, internal::object::ObjectTrait};
    use sea_orm::{
        ActiveModelTrait, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, IntoActiveModel,
        PaginatorTrait, QueryFilter, Set, Statement, TransactionTrait,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{
            mega_blob, mega_tree, mega_view_filter, mega_view_object, mega_view_root_chain,
        },
        ceres::view::filter::parse_for_registration,
        config::testing::isolated_config,
        jupiter::{
            storage::{
                view_storage::{VIEW_LOCK_TIMEOUT, ViewLock, ViewLockMode, acquire_view_lock},
                view_test_fixtures::{
                    RootCommitFixture, cas_fixture_main, root_tree_from_paths,
                    seed_linear_root_history_with_trees, seed_single_parent_root_commit_with_tree,
                },
            },
            tests::test_storage_with_config,
        },
    };

    struct Fixture {
        _temp: TempDir,
        storage: Storage,
        roots: Vec<RootCommitFixture>,
        filter_pk: i64,
        filter_id: String,
    }

    impl Fixture {
        async fn new(root_count: usize, spec: &str) -> Self {
            Self::with_limits(root_count, spec, 2, 100).await
        }

        async fn with_limits(root_count: usize, spec: &str, sync: u64, walk: u64) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let mut config = isolated_config(temp.path().join("config"));
            config.views.batch_size = 100;
            config.views.sync_catch_up_commits = sync;
            config.views.max_append_walk = walk;
            let storage = test_storage_with_config(temp.path(), config).await;
            let trees = (0..root_count)
                .map(|index| {
                    root_tree_from_paths(
                        HashKind::Sha1,
                        &[
                            (
                                "repo/file.txt".to_owned(),
                                format!("root-{index}").into_bytes(),
                            ),
                            (
                                "other/file.txt".to_owned(),
                                format!("other-{index}").into_bytes(),
                            ),
                        ],
                    )
                })
                .collect();
            let roots = seed_linear_root_history_with_trees(
                storage.view_storage().get_connection(),
                HashKind::Sha1,
                trees,
            )
            .await;
            assert_eq!(
                storage
                    .view_storage()
                    .extend_root_chain(None, 100, ViewLockMode::Try)
                    .await
                    .unwrap(),
                RootChainOutcome::CaughtUp
            );
            let filter_pk = 5001;
            let filter_id = insert_filter(&storage, filter_pk, spec).await;
            Self {
                _temp: temp,
                storage,
                roots,
                filter_pk,
                filter_id,
            }
        }

        fn repo(&self, config: Arc<Config>) -> ViewRepo {
            ViewRepo::new(
                self.storage.clone(),
                config,
                ViewProjectionService::new(self.storage.clone(), self.storage.view_metrics()),
                self.filter_pk,
                self.filter_id.clone(),
            )
        }

        fn repo_limits(&self, sync: u64, walk: u64) -> ViewRepo {
            let mut config = self.storage.config().as_ref().clone();
            config.views.sync_catch_up_commits = sync;
            config.views.max_append_walk = walk;
            self.repo(Arc::new(config))
        }

        fn current_repo(&self) -> ViewRepo {
            self.repo(self.storage.config())
        }

        async fn set_state(&self, projected_seq: i64, ready_seq: Option<i64>, warming: bool) {
            let db = self.storage.view_storage();
            db.get_connection().execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
                "UPDATE mega_view_filter SET projected_seq = $1, ready_seq = $2, warming_since = CASE WHEN $3 THEN now() ELSE NULL END WHERE id = $4",
                [sea_orm::Value::from(projected_seq), sea_orm::Value::from(ready_seq), sea_orm::Value::from(warming), sea_orm::Value::from(self.filter_pk)],
            )).await.unwrap();
        }

        async fn state(&self) -> ViewReaderState {
            self.storage
                .view_storage()
                .view_reader_state(self.filter_pk)
                .await
                .unwrap()
                .unwrap()
        }

        async fn catch_up(&self, count: usize) {
            let repo = self.current_repo();
            for _ in 0..count {
                repo.projection
                    .catch_up_one_batch(self.filter_pk, &repo.config, 100)
                    .await
                    .unwrap();
            }
        }

        async fn append(&mut self, count: usize) {
            let db = self.storage.view_storage();
            for index in 0..count {
                let parent = self.roots.last().unwrap();
                let tree = root_tree_from_paths(
                    HashKind::Sha1,
                    &[(
                        "repo/file.txt".to_owned(),
                        format!("append-{}-{index}", self.roots.len()).into_bytes(),
                    )],
                );
                let next = seed_single_parent_root_commit_with_tree(
                    db.get_connection(),
                    HashKind::Sha1,
                    tree,
                    parent,
                    &format!("append-{index}"),
                )
                .await;
                assert!(cas_fixture_main(db.get_connection(), parent, &next).await);
                self.roots.push(next);
            }
        }

        async fn drain_signal(&self) {
            while tokio::time::timeout(
                Duration::from_millis(10),
                self.storage.view_signal().notified(),
            )
            .await
            .is_ok()
            {}
        }

        async fn assert_signal(&self, expected: bool) {
            let duration = Duration::from_millis(if expected { 100 } else { 200 });
            assert_eq!(
                tokio::time::timeout(duration, self.storage.view_signal().notified())
                    .await
                    .is_ok(),
                expected
            );
        }
    }

    async fn insert_filter(storage: &Storage, filter_pk: i64, spec: &str) -> String {
        let canonical = parse_for_registration(spec).unwrap();
        let filter_id = canonical.filter_id.clone();
        mega_view_filter::ActiveModel {
            id: Set(filter_pk),
            filter_id: Set(filter_id.clone()),
            canonical_spec: Set(canonical.canonical_text),
            algo_version: Set(1),
            object_format: Set("sha1".to_owned()),
            src_paths: Set(serde_json::json!([])),
            push_enabled: Set(false),
            projected_seq: Set(0),
            ready_seq: Set(None),
            warming_since: Set(Some(Utc::now().naive_utc())),
            last_access_at: Set(None),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(storage.view_storage().get_connection())
        .await
        .unwrap();
        filter_id
    }

    fn refs_error(result: Result<(String, Vec<Refs>), MegaError>) -> MegaError {
        match result {
            Ok(_) => panic!("expected refs error"),
            Err(error) => error,
        }
    }

    fn assert_unavailable(error: MegaError, filter_id: &str, reason: ViewUnavailableReason) {
        match error {
            MegaError::ViewUnavailable {
                filter_id: actual_id,
                reason: actual_reason,
            } => {
                assert_eq!(actual_id, filter_id);
                assert_eq!(actual_reason, reason);
            }
            other => panic!("expected ViewUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unavailable_rejects_refs_and_wants() {
        let fixture = Fixture::new(3, ":/repo").await;
        let repo = fixture.current_repo();
        let want = vec![fixture.roots[0].commit.id.to_string()];
        for warming in [true, false] {
            fixture.set_state(0, None, warming).await;
            assert_unavailable(
                refs_error(repo.refs_with_head_hash().await),
                &fixture.filter_id,
                ViewUnavailableReason::WarmingUp,
            );
            assert_unavailable(
                repo.check_wants_and_ready(&want).await.unwrap_err(),
                &fixture.filter_id,
                ViewUnavailableReason::WarmingUp,
            );
        }
        fixture.set_state(1, Some(1), false).await;
        assert!(
            cas_fixture_main(
                fixture.storage.view_storage().get_connection(),
                &fixture.roots[2],
                &fixture.roots[0]
            )
            .await
        );
        assert!(!fixture.state().await.halted);
        fixture.drain_signal().await;
        assert_unavailable(
            refs_error(repo.refs_with_head_hash().await),
            &fixture.filter_id,
            ViewUnavailableReason::RootChainHalted,
        );
        fixture.assert_signal(false).await;
        assert!(fixture.state().await.halted);
        assert_unavailable(
            refs_error(repo.refs_with_head_hash().await),
            &fixture.filter_id,
            ViewUnavailableReason::RootChainHalted,
        );
        assert_unavailable(
            repo.check_wants_and_ready(&want).await.unwrap_err(),
            &fixture.filter_id,
            ViewUnavailableReason::RootChainHalted,
        );
        fixture.set_state(0, None, false).await;
        assert_unavailable(
            refs_error(repo.refs_with_head_hash().await),
            &fixture.filter_id,
            ViewUnavailableReason::RootChainHalted,
        );
        assert_unavailable(
            repo.check_wants_and_ready(&want).await.unwrap_err(),
            &fixture.filter_id,
            ViewUnavailableReason::RootChainHalted,
        );
    }

    #[tokio::test]
    async fn ready_refs_match_design() {
        let empty = Fixture::new(2, ":/never-seen").await;
        empty.catch_up(1).await;
        assert!(empty.state().await.ready_seq.is_some());
        let (head, refs) = empty.current_repo().refs_with_head_hash().await.unwrap();
        assert_eq!(head, ZERO_ID);
        assert!(refs.is_empty());

        let fixture = Fixture::new(2, ":/repo").await;
        fixture.catch_up(1).await;
        let mut fixture = fixture;
        fixture.append(3).await;
        assert_eq!(
            fixture
                .storage
                .view_storage()
                .extend_root_chain(None, 100, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::CaughtUp
        );
        let before = fixture.state().await.view_tip;
        let (head, refs) = fixture.current_repo().refs_with_head_hash().await.unwrap();
        assert_ne!(Some(head.clone()), before);
        assert_eq!(Some(head.clone()), fixture.state().await.view_tip);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_name, MEGA_BRANCH_NAME);
        assert_eq!(refs[0].ref_hash, head);
        assert!(refs[0].default_branch);
    }

    fn assert_not_our_ref(error: MegaError, oid: &str) {
        match error {
            MegaError::ViewPackRejected(message) => {
                assert_eq!(message, format!("upload-pack: not our ref {oid}"));
            }
            other => panic!("expected ViewPackRejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn want_gate_not_our_ref() {
        let fixture = Fixture::new(3, ":/repo").await;
        fixture.catch_up(1).await;
        let repo = fixture.current_repo();
        let own = fixture.state().await.view_tip.unwrap();
        assert!(
            repo.check_wants_and_ready(&[own.clone(), own.clone()])
                .await
                .is_ok()
        );

        let root = fixture.roots[1].commit.id.to_string();
        assert_not_our_ref(
            repo.check_wants_and_ready(std::slice::from_ref(&root))
                .await
                .unwrap_err(),
            &root,
        );

        let other_pk = 5002;
        let other_id = insert_filter(&fixture.storage, other_pk, ":/other").await;
        let other_service =
            ViewProjectionService::new(fixture.storage.clone(), fixture.storage.view_metrics());
        other_service
            .catch_up_one_batch(other_pk, &fixture.storage.config(), 100)
            .await
            .unwrap();
        let other_tip = fixture
            .storage
            .view_storage()
            .view_reader_state(other_pk)
            .await
            .unwrap()
            .unwrap()
            .view_tip
            .unwrap();
        assert_ne!(other_id, fixture.filter_id);
        assert_ne!(other_tip, own);
        assert_not_our_ref(
            repo.check_wants_and_ready(std::slice::from_ref(&other_tip))
                .await
                .unwrap_err(),
            &other_tip,
        );
        assert_not_our_ref(
            repo.check_wants_and_ready(&[own.clone(), other_tip.clone()])
                .await
                .unwrap_err(),
            &other_tip,
        );

        fixture.set_state(0, Some(1), false).await;
        assert_not_our_ref(
            repo.check_wants_and_ready(std::slice::from_ref(&own))
                .await
                .unwrap_err(),
            &own,
        );
    }

    #[tokio::test]
    async fn advertise_uses_reloaded_config() {
        let mut fixture = Fixture::with_limits(2, ":/repo", 1, 1).await;
        fixture.catch_up(1).await;
        let instance_a = fixture.current_repo();
        fixture.append(6).await;
        fixture
            .storage
            .view_storage()
            .extend_root_chain(None, 100, ViewLockMode::Try)
            .await
            .unwrap();
        let before = fixture.state().await.projected_seq;
        instance_a.refs_with_head_hash().await.unwrap();
        assert_eq!(fixture.state().await.projected_seq, before + 1);
        let mut candidate = fixture.storage.config().as_ref().clone();
        candidate.views.sync_catch_up_commits = 3;
        candidate.views.max_append_walk = 100;
        assert!(
            fixture
                .storage
                .config_handle
                .reload(candidate)
                .unwrap()
                .applied()
        );
        let instance_b = fixture.current_repo();
        assert_eq!(fixture.storage.config.views.sync_catch_up_commits, 1);
        assert_eq!(fixture.storage.config.views.max_append_walk, 1);
        instance_a.refs_with_head_hash().await.unwrap();
        assert_eq!(fixture.state().await.projected_seq, before + 2);
        instance_b.refs_with_head_hash().await.unwrap();
        assert_eq!(fixture.state().await.projected_seq, before + 5);
        fixture.append(4).await;
        let db = fixture.storage.view_storage();
        let count_before = mega_view_root_chain::Entity::find()
            .count(db.get_connection())
            .await
            .unwrap();
        instance_a.refs_with_head_hash().await.unwrap();
        assert_eq!(
            mega_view_root_chain::Entity::find()
                .count(db.get_connection())
                .await
                .unwrap(),
            count_before
        );
        fixture.drain_signal().await;
        instance_b.refs_with_head_hash().await.unwrap();
        assert_eq!(
            mega_view_root_chain::Entity::find()
                .count(db.get_connection())
                .await
                .unwrap(),
            count_before + 4
        );
    }

    #[tokio::test]
    async fn remaining_methods_match_design() {
        let fixture = Fixture::new(3, ":/repo").await;
        fixture.catch_up(1).await;
        let repo = fixture.current_repo();
        let own = fixture.state().await.view_tip.unwrap();
        let root = fixture.roots[1].commit.id.to_string();
        assert!(repo.check_commit_exist(&own).await);
        assert!(!repo.check_commit_exist(&root).await);
        let other_pk = 5002;
        let other_id = insert_filter(&fixture.storage, other_pk, ":/other").await;
        let other_service =
            ViewProjectionService::new(fixture.storage.clone(), fixture.storage.view_metrics());
        other_service
            .catch_up_one_batch(other_pk, &fixture.storage.config(), 100)
            .await
            .unwrap();
        let other_tip = fixture
            .storage
            .view_storage()
            .view_reader_state(other_pk)
            .await
            .unwrap()
            .unwrap()
            .view_tip
            .unwrap();
        assert_ne!(other_id, fixture.filter_id);
        assert!(!repo.check_commit_exist(&other_tip).await);
        assert!(repo.is_monorepo());
        assert_eq!(
            repo.object_hash_kind().unwrap(),
            repo.config.monorepo.object_hash_kind().unwrap()
        );
        assert!(repo.check_default_branch().await);

        let db = fixture.storage.view_storage();
        let spine_tree = root_tree_from_paths(
            HashKind::Sha1,
            &[("synthetic-spine/file.txt".to_owned(), b"spine".to_vec())],
        )
        .root;
        let spine = spine_tree.id.to_string();
        mega_view_object::ActiveModel {
            object_id: Set(spine.clone()),
            kind: Set(2),
            data: Set(spine_tree.to_data().unwrap()),
            created_at: Set(Utc::now().naive_utc()),
            gc_marked_at: Set(None),
        }
        .insert(db.get_connection())
        .await
        .unwrap();
        let l0 = fixture.roots[0].commit.tree_id.to_string();
        assert!(
            mega_view_object::Entity::find_by_id(spine.clone())
                .one(db.get_connection())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            mega_tree::Entity::find()
                .filter(mega_tree::Column::TreeId.eq(spine.clone()))
                .one(db.get_connection())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            mega_view_object::Entity::find_by_id(l0.clone())
                .one(db.get_connection())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            mega_tree::Entity::find()
                .filter(mega_tree::Column::TreeId.eq(l0.clone()))
                .one(db.get_connection())
                .await
                .unwrap()
                .is_some()
        );
        let trees = repo
            .get_trees_by_hashes(vec![spine.clone(), l0.clone()])
            .await
            .unwrap();
        assert_eq!(
            trees
                .iter()
                .map(|tree| tree.id.to_string())
                .collect::<Vec<_>>(),
            vec![spine, l0]
        );
        assert!(
            repo.get_trees_by_hashes(vec!["f".repeat(40)])
                .await
                .is_err()
        );

        let blob_id = fixture
            .storage
            .git_service
            .save_object_from_raw(bytes::Bytes::from_static(b"view test blob"))
            .await
            .unwrap();
        assert!(!repo.check_object_exist(&blob_id).await);
        let blob_model = mega_blob::ActiveModel {
            id: Set(8101),
            blob_id: Set(blob_id.clone()),
            name: Set("blob".to_owned()),
            size: Set(14),
            created_at: Set(Utc::now().naive_utc()),
            pack_id: Set("pack-1".to_owned()),
            file_path: Set("repo/file.txt".to_owned()),
            pack_offset: Set(17),
            is_delta_in_pack: Set(false),
            commit_id: Set(root),
        }
        .insert(db.get_connection())
        .await
        .unwrap();
        let metadata = repo
            .get_blob_metadata_by_hashes(vec![blob_id.clone()])
            .await
            .unwrap();
        let entry = metadata.get(&blob_id).unwrap();
        assert_eq!(entry.pack_id.as_deref(), Some(blob_model.pack_id.as_str()));
        assert_eq!(entry.pack_offset, Some(blob_model.pack_offset as usize));
        assert_eq!(
            entry.file_path.as_deref(),
            Some(blob_model.file_path.as_str())
        );
        assert_eq!(entry.is_delta, Some(blob_model.is_delta_in_pack));
        assert_eq!(entry.crc32, None);
        let mono_blob = fixture
            .storage
            .mono_storage()
            .get_mega_blobs_by_hashes(vec![blob_id.clone()])
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(mono_blob, blob_model);
        let mut blobs = repo
            .get_blobs_by_hashes(vec![blob_id.clone()])
            .await
            .unwrap();
        let (returned_id, content, _) = blobs.try_next().await.unwrap().unwrap();
        assert_eq!(returned_id.key, blob_id);
        let bytes = content
            .try_fold(Vec::new(), |mut all, chunk| async move {
                all.extend_from_slice(&chunk);
                Ok(all)
            })
            .await
            .unwrap();
        assert_eq!(bytes, b"view test blob");

        assert!(
            repo.finalize_receive_pack()
                .await
                .unwrap_err()
                .to_string()
                .contains("view URLs are read-only")
        );
        assert!(
            repo.save_entry(vec![])
                .await
                .unwrap_err()
                .to_string()
                .contains("view URLs are read-only")
        );
        assert!(
            repo.update_pack_id("a", "b")
                .await
                .unwrap_err()
                .to_string()
                .contains("view URLs are read-only")
        );
        let git_entry: Entry =
            git_internal::internal::object::blob::Blob::from_content_bytes(b"x".to_vec()).into();
        assert!(
            repo.check_entry(&git_entry)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("view URLs are read-only")
        );
        let command = RefCommand::new(ZERO_ID.to_owned(), own.clone(), MEGA_BRANCH_NAME.to_owned());
        assert!(
            repo.update_refs(&command)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("view URLs are read-only")
        );
        assert!(
            repo.traverses_tree_and_update_filepath()
                .await
                .unwrap_err()
                .to_string()
                .contains("view URLs are read-only")
        );
        assert!(
            matches!(repo.full_pack(vec![own.clone()]).await.err(), Some(GitError::CustomError(message)) if message.contains("view pack not implemented"))
        );
        assert!(
            matches!(repo.incremental_pack(vec![own.clone()], vec![]).await.err(), Some(GitError::CustomError(message)) if message.contains("view pack not implemented"))
        );

        db.get_connection()
            .execute_unprepared(
                "ALTER TABLE mega_view_commit_map RENAME COLUMN view_commit TO hp_gone",
            )
            .await
            .unwrap();
        assert!(!repo.check_commit_exist(&own).await);
        db.get_connection()
            .execute_unprepared(
                "ALTER TABLE mega_view_commit_map RENAME COLUMN hp_gone TO view_commit",
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn advertise_extend_and_sync_catch_up() {
        let mut fixture = Fixture::new(2, ":/repo").await;
        fixture.catch_up(1).await;
        fixture.append(6).await;
        let old = fixture.state().await;
        let db = fixture.storage.view_storage();
        let old_count = mega_view_root_chain::Entity::find()
            .count(db.get_connection())
            .await
            .unwrap();
        fixture.drain_signal().await;
        let (head, _) = fixture
            .repo_limits(2, 2)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(Some(head), old.view_tip);
        assert_eq!(fixture.state().await.projected_seq, old.projected_seq);
        assert_eq!(
            mega_view_root_chain::Entity::find()
                .count(db.get_connection())
                .await
                .unwrap(),
            old_count
        );
        fixture.assert_signal(true).await;
        fixture.drain_signal().await;
        fixture
            .repo_limits(100, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(
            mega_view_root_chain::Entity::find()
                .count(db.get_connection())
                .await
                .unwrap(),
            old_count + 6
        );
        assert_eq!(fixture.state().await.projected_seq, old.projected_seq + 6);
        fixture.assert_signal(false).await;

        fixture.append(6).await;
        let prior = fixture.state().await;
        let txn = db.get_connection().begin().await.unwrap();
        assert!(
            acquire_view_lock(&txn, ViewLock::RootChain, ViewLockMode::Try)
                .await
                .unwrap()
        );
        fixture.drain_signal().await;
        let start = Instant::now();
        let (head, _) = fixture
            .repo_limits(100, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= VIEW_LOCK_TIMEOUT && elapsed < VIEW_LOCK_TIMEOUT + Duration::from_secs(5),
            "{elapsed:?}"
        );
        assert_eq!(Some(head), prior.view_tip);
        assert_eq!(fixture.state().await.projected_seq, prior.projected_seq);
        fixture.assert_signal(true).await;
        txn.rollback().await.unwrap();

        let txn = db.get_connection().begin().await.unwrap();
        assert!(
            acquire_view_lock(&txn, ViewLock::RootChain, ViewLockMode::Try)
                .await
                .unwrap()
        );
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            txn.commit().await.unwrap();
        });
        fixture.drain_signal().await;
        fixture
            .repo_limits(100, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        release.await.unwrap();
        assert_eq!(fixture.state().await.projected_seq, prior.projected_seq + 6);
        fixture.assert_signal(false).await;

        let mut sync = Fixture::new(2, ":/repo").await;
        sync.catch_up(1).await;
        sync.append(5).await;
        sync.storage
            .view_storage()
            .extend_root_chain(None, 100, ViewLockMode::Try)
            .await
            .unwrap();
        let start = sync.state().await;
        sync.drain_signal().await;
        let (head, _) = sync
            .repo_limits(2, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(sync.state().await.projected_seq, start.projected_seq + 2);
        assert_eq!(Some(head), sync.state().await.view_tip);
        sync.assert_signal(false).await;
        let before_zero = sync.state().await;
        let metrics = sync.storage.view_metrics().counters();
        let (head, _) = sync
            .repo_limits(0, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(sync.state().await.projected_seq, before_zero.projected_seq);
        assert_eq!(Some(head), before_zero.view_tip);
        assert_eq!(
            sync.storage
                .view_metrics()
                .counters()
                .view_batch_premise_failures_total,
            metrics.view_batch_premise_failures_total
        );
        sync.assert_signal(false).await;
        let txn = sync
            .storage
            .view_storage()
            .get_connection()
            .begin()
            .await
            .unwrap();
        assert!(
            acquire_view_lock(&txn, ViewLock::Filter(sync.filter_pk), ViewLockMode::Try)
                .await
                .unwrap()
        );
        let before_lock = sync.state().await;
        let (head, _) = sync
            .repo_limits(2, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(Some(head), before_lock.view_tip);
        assert_eq!(sync.state().await.projected_seq, before_lock.projected_seq);
        sync.assert_signal(false).await;
        txn.rollback().await.unwrap();

        let mut budget = Fixture::new(2, ":/repo").await;
        budget.catch_up(1).await;
        budget.append(2).await;
        budget
            .storage
            .view_storage()
            .extend_root_chain(None, 100, ViewLockMode::Try)
            .await
            .unwrap();
        budget.append(3).await;
        let prior = budget.state().await;
        budget.drain_signal().await;
        let (head, _) = budget
            .repo_limits(2, 1)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(Some(head), prior.view_tip);
        assert_eq!(budget.state().await.projected_seq, prior.projected_seq);
        budget.assert_signal(true).await;

        let mut rollback = Fixture::new(2, ":/repo").await;
        rollback.catch_up(1).await;
        rollback.append(2).await;
        rollback
            .storage
            .view_storage()
            .extend_root_chain(None, 100, ViewLockMode::Try)
            .await
            .unwrap();
        assert!(
            cas_fixture_main(
                rollback.storage.view_storage().get_connection(),
                &rollback.roots[3],
                &rollback.roots[1]
            )
            .await
        );
        let before = rollback.state().await.projected_seq;
        rollback.drain_signal().await;
        assert_unavailable(
            refs_error(rollback.repo_limits(2, 100).refs_with_head_hash().await),
            &rollback.filter_id,
            ViewUnavailableReason::RootChainHalted,
        );
        assert_eq!(rollback.state().await.projected_seq, before);
        rollback.assert_signal(false).await;

        let mut uncovered = Fixture::new(2, ":/repo").await;
        uncovered.catch_up(1).await;
        uncovered.append(1).await;
        uncovered.drain_signal().await;
        assert!(matches!(
            uncovered.repo_limits(2, 100).sync_catch_up().await,
            Some(Ok(CatchUpOutcome::MainNotCovered))
        ));
        uncovered.assert_signal(true).await;

        let mut stopped = Fixture::new(2, ":/repo").await;
        stopped.catch_up(1).await;
        stopped.append(5).await;
        let stop_db = stopped.storage.view_storage();
        stop_db
            .extend_root_chain(None, 100, ViewLockMode::Try)
            .await
            .unwrap();
        let stop_before = stopped.state().await;
        let removed_tree = mega_tree::Entity::find()
            .filter(mega_tree::Column::TreeId.eq(stopped.roots[4].commit.tree_id.to_string()))
            .one(stop_db.get_connection())
            .await
            .unwrap()
            .unwrap();
        mega_tree::Entity::delete_by_id(removed_tree.id)
            .exec(stop_db.get_connection())
            .await
            .unwrap();
        stopped.drain_signal().await;
        let (head, _) = stopped
            .repo_limits(4, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(
            stopped.state().await.projected_seq,
            stop_before.projected_seq + 2
        );
        assert_eq!(Some(head), stopped.state().await.view_tip);
        stopped.assert_signal(false).await;
        removed_tree
            .into_active_model()
            .insert(stop_db.get_connection())
            .await
            .unwrap();

        let mut errors = Fixture::new(2, ":/repo").await;
        errors.catch_up(1).await;
        errors.append(3).await;
        let edb = errors.storage.view_storage();
        edb.extend_root_chain(None, 100, ViewLockMode::Try)
            .await
            .unwrap();
        let prior = errors.state().await;
        errors.drain_signal().await;
        edb.get_connection()
            .execute_unprepared("ALTER TABLE mega_refs RENAME COLUMN ref_commit_hash TO hp_gone")
            .await
            .unwrap();
        let (head, _) = errors
            .repo_limits(2, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(Some(head), prior.view_tip);
        assert_eq!(errors.state().await.projected_seq, prior.projected_seq);
        errors.assert_signal(false).await;
        edb.get_connection()
            .execute_unprepared("ALTER TABLE mega_refs RENAME COLUMN hp_gone TO ref_commit_hash")
            .await
            .unwrap();
        edb.get_connection()
            .execute_unprepared(
                "ALTER TABLE mega_view_object_ref RENAME COLUMN object_id TO hp_gone",
            )
            .await
            .unwrap();
        let (head, _) = errors
            .repo_limits(2, 100)
            .refs_with_head_hash()
            .await
            .unwrap();
        assert_eq!(Some(head), prior.view_tip);
        assert_eq!(errors.state().await.projected_seq, prior.projected_seq);
        errors.assert_signal(false).await;
        edb.get_connection()
            .execute_unprepared(
                "ALTER TABLE mega_view_object_ref RENAME COLUMN hp_gone TO object_id",
            )
            .await
            .unwrap();
    }
}
