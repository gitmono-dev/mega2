use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use futures::TryStreamExt;
use git_internal::{
    errors::GitError,
    hash::{HashKind, ObjectHash},
    internal::{
        metadata::{EntryMeta, MetaAttached},
        object::{
            ObjectTrait,
            tree::{Tree, TreeItemMode},
            types::ObjectType,
        },
        pack::{encode::PackEncoder, entry::Entry},
    },
};
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    ceres::{
        pack::RepoHandler,
        protocol::import_refs::{RefCommand, Refs},
        view::tree_source::{InMemoryTreeSource, TreeSource, empty_tree_id, read_tree},
    },
    common::{
        errors::{MegaError, ViewUnavailableReason},
        utils::{MEGA_BRANCH_NAME, ZERO_ID},
    },
    config::Config,
    jupiter::{
        service::view_projection_service::{CatchUpOutcome, ViewProjectionService},
        storage::{
            Storage,
            base_storage::StorageConnector,
            view_projection_storage::{ViewPackCommit, ViewReaderState},
            view_root_chain::RootChainOutcome,
            view_storage::ViewLockMode,
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
    pack_plan: Mutex<Option<ViewPackPlan>>,
}

struct ViewPackPlan {
    commits: Vec<ViewPackCommit>,
    tree_levels: Vec<Vec<String>>,
    blobs: Vec<String>,
    object_count: usize,
}

impl ViewRepo {
    async fn count_trees(
        &self,
        kind: HashKind,
        ids: &[String],
        empty: &str,
    ) -> Result<HashMap<String, Tree>, MegaError> {
        let view_storage = self.storage.view_storage();
        let requested = ids
            .iter()
            .filter(|id| id.as_str() != empty)
            .cloned()
            .collect::<Vec<_>>();
        let mut view_trees = view_storage.view_pack_view_trees(&requested).await?;
        let remaining = requested
            .iter()
            .filter(|id| !view_trees.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        let mono = self.storage.mono_storage();
        let l0_trees = mono
            .get_trees_by_hashes_fallible(view_storage.get_connection(), &remaining)
            .await?;
        for tree in l0_trees {
            let actual =
                ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Tree, &tree.sub_trees)
                    .map_err(GitError::from)?;
            if actual.to_string() != tree.tree_id {
                self.storage.view_metrics().increment_pack_tree_mismatch();
                tracing::error!(
                    metric = %"view_pack_tree_mismatch_total",
                    filter_id = %self.filter_id,
                    tree_id = %tree.tree_id,
                    "view pack tree does not match its stored entries"
                );
                return Err(MegaError::ViewPackRejected(format!(
                    "view {} pack aborted: tree {} does not match its stored entries",
                    self.filter_id, tree.tree_id
                )));
            }
            view_trees.insert(tree.tree_id, tree.sub_trees);
        }
        let source = InMemoryTreeSource::new(kind, view_trees, HashSet::new());
        ids.iter()
            .map(|id| {
                let tree = read_tree(kind, &source, id).map_err(|_| {
                    tracing::error!(
                        filter_id = %self.filter_id,
                        tree_id = %id,
                        "view pack tree is missing"
                    );
                    MegaError::ViewPackRejected(format!(
                        "view {} pack aborted: tree {} is missing",
                        self.filter_id, id
                    ))
                })?;
                Ok((id.clone(), tree))
            })
            .collect()
    }

    async fn count_pack(
        &self,
        kind: HashKind,
        wants: &[String],
        haves: &[String],
    ) -> Result<ViewPackPlan, MegaError> {
        let view_storage = self.storage.view_storage();
        let snapshot = view_storage
            .view_pack_snapshot(self.filter_pk, wants, haves)
            .await?
            .ok_or_else(|| self.unavailable(ViewUnavailableReason::WarmingUp))?;
        if snapshot.halted {
            return Err(self.unavailable(ViewUnavailableReason::RootChainHalted));
        }
        if snapshot.ready_seq.is_none() || !snapshot.wants_valid {
            return Err(self.unavailable(ViewUnavailableReason::WarmingUp));
        }
        let commits = snapshot.commits;
        let empty = empty_tree_id(kind).map_err(GitError::from)?.to_string();
        let mut previous = snapshot.have_tree.unwrap_or_else(|| empty.clone());
        let mut frontier = BTreeSet::new();
        for commit in &commits {
            if commit.tree_id != previous {
                frontier.insert((previous.clone(), commit.tree_id.clone()));
            }
            previous = commit.tree_id.clone();
        }

        let mut seen_trees = HashSet::new();
        let mut seen_blobs = BTreeSet::new();
        let mut tree_levels = Vec::new();
        while !frontier.is_empty() {
            let ids = frontier
                .iter()
                .flat_map(|(old, new)| [old.clone(), new.clone()])
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let trees = self.count_trees(kind, &ids, &empty).await?;
            let mut next = BTreeSet::new();
            let mut level = Vec::new();
            for (old_id, new_id) in frontier {
                if seen_trees.insert(new_id.clone()) {
                    level.push(new_id.clone());
                }
                let old = trees.get(&old_id).ok_or_else(|| {
                    GitError::CustomError(format!("prefetched view tree missing: {old_id}"))
                })?;
                let new = trees.get(&new_id).ok_or_else(|| {
                    GitError::CustomError(format!("prefetched view tree missing: {new_id}"))
                })?;
                let old_items = old
                    .tree_items
                    .iter()
                    .map(|item| (item.name.as_str(), item))
                    .collect::<HashMap<_, _>>();
                for item in &new.tree_items {
                    let prior = old_items.get(item.name.as_str());
                    if prior.is_some_and(|old_item| old_item.id == item.id) {
                        continue;
                    }
                    match item.mode {
                        TreeItemMode::Tree => {
                            let old_child = prior
                                .filter(|old_item| old_item.mode == TreeItemMode::Tree)
                                .map_or_else(|| empty.clone(), |old_item| old_item.id.to_string());
                            next.insert((old_child, item.id.to_string()));
                        }
                        TreeItemMode::Commit => {}
                        TreeItemMode::Blob | TreeItemMode::BlobExecutable | TreeItemMode::Link => {
                            seen_blobs.insert(item.id.to_string());
                        }
                    }
                }
            }
            tree_levels.push(level);
            frontier = next;
        }
        let blobs = seen_blobs.into_iter().collect::<Vec<_>>();
        let object_count = commits.len() + seen_trees.len() + blobs.len();
        Ok(ViewPackPlan {
            commits,
            tree_levels,
            blobs,
            object_count,
        })
    }

    async fn send_pack_entry(
        sender: &mpsc::Sender<MetaAttached<Entry, EntryMeta>>,
        kind: HashKind,
        object_id: &str,
        obj_type: ObjectType,
        data: Vec<u8>,
        meta: EntryMeta,
    ) -> Result<(), GitError> {
        let hash = ObjectHash::from_hex_for_kind(kind, object_id)?;
        sender
            .send(MetaAttached {
                inner: Entry {
                    obj_type,
                    data,
                    hash,
                    chain_len: 0,
                },
                meta,
            })
            .await
            .map_err(|error| GitError::CustomError(format!("view pack send failed: {error}")))
    }

    async fn encode_pack(
        &self,
        plan: ViewPackPlan,
        kind: HashKind,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        let capacity = self.config.pack.channel_message_size;
        let (entry_tx, entry_rx) = mpsc::channel(capacity);
        let (stream_tx, stream_rx) = mpsc::channel(capacity);
        PackEncoder::new_with_hash_kind(kind, plan.object_count, 0, stream_tx)
            .encode_async(entry_rx)
            .await?;

        for commit in plan.commits {
            Self::send_pack_entry(
                &entry_tx,
                kind,
                &commit.object_id,
                ObjectType::Commit,
                commit.data,
                EntryMeta::new(),
            )
            .await?;
        }
        let view_storage = self.storage.view_storage();
        let mono = self.storage.mono_storage();
        for level in plan.tree_levels {
            for chunk in level.chunks(1000) {
                let mut source = ViewTreeSource::new(&mono, view_storage.get_connection(), kind)
                    .map_err(GitError::from)?;
                source.prefetch(chunk).await.map_err(GitError::from)?;
                for id in chunk {
                    let tree = read_tree(kind, &source, id).map_err(|missing| {
                        GitError::CustomError(format!("view tree {missing:?}"))
                    })?;
                    Self::send_pack_entry(
                        &entry_tx,
                        kind,
                        id,
                        ObjectType::Tree,
                        tree.to_data()?,
                        EntryMeta::new(),
                    )
                    .await?;
                }
            }
        }
        for chunk in plan.blobs.chunks(1000) {
            let mut metadata = self
                .get_blob_metadata_by_hashes(chunk.to_vec())
                .await
                .map_err(GitError::from)?;
            let mut stream = self
                .get_blobs_by_hashes(chunk.to_vec())
                .await
                .map_err(GitError::from)?;
            let expected = chunk.iter().map(String::as_str).collect::<HashSet<_>>();
            let mut received = HashSet::new();
            while let Some((key, mut body, _)) = stream
                .try_next()
                .await
                .map_err(MegaError::from)
                .map_err(GitError::from)?
            {
                if !expected.contains(key.key.as_str()) || !received.insert(key.key.clone()) {
                    return Err(GitError::CustomError(format!(
                        "unexpected view blob: {}",
                        key.key
                    )));
                }
                let mut data = Vec::new();
                while let Some(bytes) = body.try_next().await.map_err(|error| {
                    GitError::CustomError(format!("view blob read failed: {error}"))
                })? {
                    data.extend_from_slice(&bytes);
                }
                let actual =
                    ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Blob, &data)?;
                if actual.to_string() != key.key {
                    return Err(GitError::CustomError(format!(
                        "view blob hash mismatch: {}",
                        key.key
                    )));
                }
                let meta = metadata.remove(&key.key).unwrap_or_else(EntryMeta::new);
                Self::send_pack_entry(&entry_tx, kind, &key.key, ObjectType::Blob, data, meta)
                    .await?;
            }
            if received.len() != expected.len() {
                return Err(GitError::CustomError(
                    "view blob missing from object storage".to_owned(),
                ));
            }
        }
        drop(entry_tx);
        Ok(ReceiverStream::new(stream_rx))
    }

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
            pack_plan: Mutex::new(None),
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

    async fn prepare_pack(&self, want: &[String], have: &[String]) -> Result<(), MegaError> {
        *self.pack_plan.lock().await = None;
        let kind = self.object_hash_kind()?;
        let plan = self.count_pack(kind, want, have).await?;
        *self.pack_plan.lock().await = Some(plan);
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
        let kind = self.object_hash_kind().map_err(GitError::from)?;
        let plan = self
            .pack_plan
            .lock()
            .await
            .take()
            .ok_or_else(|| GitError::CustomError("view pack was not prepared".to_owned()))?;
        self.encode_pack(plan, kind).await
    }

    async fn incremental_pack(
        &self,
        _want: Vec<String>,
        _have: Vec<String>,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        let kind = self.object_hash_kind().map_err(GitError::from)?;
        let plan = self
            .pack_plan
            .lock()
            .await
            .take()
            .ok_or_else(|| GitError::CustomError("view pack was not prepared".to_owned()))?;
        self.encode_pack(plan, kind).await
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
        collections::HashSet,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use bytes::Bytes;
    use chrono::Utc;
    use futures::{StreamExt, TryStreamExt};
    use git_internal::{
        hash::{HashKind, ObjectHash},
        internal::{
            object::{
                ObjectTrait,
                tree::{TreeItem, TreeItemMode},
            },
            pack::Pack,
        },
    };
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
                init::database_connection,
                object_storage::mock_object_storage,
                view_storage::{VIEW_LOCK_TIMEOUT, ViewLock, ViewLockMode, acquire_view_lock},
                view_test_fixtures::{
                    RootCommitFixture, RootTreeFixture, cas_fixture_main, root_tree_from_paths,
                    seed_linear_root_history_with_trees, seed_single_parent_root_commit_with_tree,
                },
            },
            tests::{test_db_config, test_storage_with_config},
            utils::converter::{MegaObjectModel, process_entry},
        },
    };

    fn raw_l0_tree(mode: &str, name: &[u8], blob: ObjectHash) -> (ObjectHash, Vec<u8>) {
        let mut bytes = format!("{mode} ").into_bytes();
        bytes.extend_from_slice(name);
        bytes.push(0);
        bytes.extend(hex::decode(blob.to_string()).unwrap());
        let id = ObjectHash::from_type_and_data_for_kind(HashKind::Sha1, ObjectType::Tree, &bytes)
            .unwrap();
        (id, bytes)
    }

    fn replace_fixture_subtree(
        fixture: &mut RootTreeFixture,
        parent_name: &str,
        child_name: &str,
        child_id: ObjectHash,
    ) {
        let old_root = fixture.root.id;
        let old_parent = fixture
            .root
            .tree_items
            .iter()
            .find(|item| item.name == parent_name)
            .unwrap()
            .id;
        let mut parent_items = fixture
            .trees
            .iter()
            .find(|tree| tree.id == old_parent)
            .unwrap()
            .tree_items
            .clone();
        let old_child = parent_items
            .iter_mut()
            .find(|item| item.name == child_name)
            .unwrap();
        let old_child_id = old_child.id;
        old_child.id = child_id;
        let new_parent = Tree::from_tree_items_with_kind(HashKind::Sha1, parent_items).unwrap();
        let mut root_items = fixture.root.tree_items.clone();
        root_items
            .iter_mut()
            .find(|item| item.name == parent_name)
            .unwrap()
            .id = new_parent.id;
        let new_root = Tree::from_tree_items_with_kind(HashKind::Sha1, root_items).unwrap();
        fixture
            .trees
            .retain(|tree| tree.id != old_root && tree.id != old_parent && tree.id != old_child_id);
        fixture.trees.extend([new_parent, new_root.clone()]);
        fixture.root = new_root;
    }

    fn capture_tracing<T>(f: impl FnOnce() -> T) -> (T, String) {
        use std::io::Write;

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

        let _pin_registry = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        );
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(TestWriter(buffer.clone()))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        let result = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        (result, text)
    }

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
            Self::from_trees(trees, spec, sync, walk).await
        }

        async fn from_trees(trees: Vec<RootTreeFixture>, spec: &str, sync: u64, walk: u64) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let mut config = isolated_config(temp.path().join("config"));
            config.views.batch_size = 100;
            config.views.sync_catch_up_commits = sync;
            config.views.max_append_walk = walk;
            let storage = test_storage_with_config(temp.path(), config).await;
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

        async fn save_blobs(&self) {
            let mut saved = HashSet::new();
            for root in &self.roots {
                for blob in &root.blobs {
                    if !saved.insert(blob.id) {
                        continue;
                    }
                    self.storage
                        .git_service
                        .save_object_from_raw(Bytes::copy_from_slice(&blob.data))
                        .await
                        .unwrap();
                }
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

    async fn pack_ids(stream: ReceiverStream<Vec<u8>>, temp: &TempDir) -> (usize, HashSet<String>) {
        let bytes = stream.concat().await;
        assert!(bytes.len() >= 12);
        let declared = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let mut pack = Pack::new_with_hash_kind(
            HashKind::Sha1,
            Some(1),
            Some(64 * 1024 * 1024),
            Some(temp.path().to_path_buf()),
            true,
        );
        let ids = Arc::new(Mutex::new(Vec::new()));
        let sink = ids.clone();
        pack.decode(
            &mut std::io::Cursor::new(bytes),
            move |entry| {
                let expected = ObjectHash::from_type_and_data_for_kind(
                    HashKind::Sha1,
                    entry.inner.obj_type,
                    &entry.inner.data,
                )
                .unwrap();
                assert_eq!(entry.inner.hash, expected);
                sink.lock().unwrap().push(expected.to_string());
            },
            None::<fn(ObjectHash)>,
        )
        .unwrap();
        let ids = ids.lock().unwrap();
        let unique = ids.iter().cloned().collect::<HashSet<_>>();
        assert_eq!(declared, ids.len());
        assert_eq!(unique.len(), ids.len());
        (declared, unique)
    }

    async fn prepared_full_pack(repo: &ViewRepo, want: Vec<String>) -> ReceiverStream<Vec<u8>> {
        repo.prepare_pack(&want, &[]).await.unwrap();
        repo.full_pack(want).await.unwrap()
    }

    async fn prepared_incremental_pack(
        repo: &ViewRepo,
        want: Vec<String>,
        have: Vec<String>,
    ) -> ReceiverStream<Vec<u8>> {
        repo.prepare_pack(&want, &have).await.unwrap();
        repo.incremental_pack(want, have).await.unwrap()
    }

    fn fixture_view_closure(root: &RootCommitFixture, view_tree: &str) -> HashSet<String> {
        let mut ids = HashSet::new();
        let mut pending = vec![view_tree.to_owned()];
        while let Some(tree_id) = pending.pop() {
            if !ids.insert(tree_id.clone()) {
                continue;
            }
            let tree = root
                .trees
                .iter()
                .find(|tree| tree.id.to_string() == tree_id)
                .unwrap();
            for item in &tree.tree_items {
                match item.mode {
                    TreeItemMode::Tree => pending.push(item.id.to_string()),
                    TreeItemMode::Commit => {}
                    TreeItemMode::Blob | TreeItemMode::BlobExecutable | TreeItemMode::Link => {
                        ids.insert(item.id.to_string());
                    }
                }
            }
        }
        ids
    }

    #[tokio::test]
    async fn full_pack_closure_one_to_one() {
        for (spec, root_path, other_path) in [
            (":/repo", "file.txt", None),
            (
                ":prefix=pre",
                "pre/repo/file.txt",
                Some("pre/other/file.txt"),
            ),
            (":exclude[::other/]", "repo/file.txt", None),
            (":/repo:prefix=pre", "pre/file.txt", None),
            (
                ":[:/repo:prefix=r,:/other:prefix=o]",
                "r/file.txt",
                Some("o/file.txt"),
            ),
        ] {
            let fixture = Fixture::new(8, spec).await;
            fixture.save_blobs().await;
            fixture.catch_up(1).await;
            let repo = fixture.current_repo();
            let tip = fixture.state().await.view_tip.unwrap();
            let commits = fixture
                .storage
                .view_storage()
                .view_pack_commits(fixture.filter_pk, 0, 8)
                .await
                .unwrap();
            assert_eq!(commits.len(), 8, "{spec}");
            for index in [2, 7] {
                let want = commits[index].object_id.clone();
                let (count, ids) =
                    pack_ids(prepared_full_pack(&repo, vec![want]).await, &fixture._temp).await;
                let mut expected = HashSet::new();
                for (source_index, commit) in commits.iter().take(index + 1).enumerate() {
                    expected.insert(commit.object_id.clone());
                    let mut paths = vec![(
                        root_path.to_owned(),
                        format!("root-{source_index}").into_bytes(),
                    )];
                    if let Some(other_path) = other_path {
                        paths.push((
                            other_path.to_owned(),
                            format!("other-{source_index}").into_bytes(),
                        ));
                    }
                    let tree = root_tree_from_paths(HashKind::Sha1, &paths);
                    expected.extend(tree.trees.iter().map(|tree| tree.id.to_string()));
                    expected.extend(tree.blobs.iter().map(|blob| blob.id.to_string()));
                    assert_eq!(commit.tree_id, tree.root.id.to_string(), "{spec}");
                }
                assert_eq!(count, expected.len(), "{spec}");
                assert_eq!(ids, expected, "{spec}");
            }
            if spec == ":prefix=pre" {
                let expected = root_tree_from_paths(
                    HashKind::Sha1,
                    &[
                        ("pre/repo/file.txt".to_owned(), b"root-7".to_vec()),
                        ("pre/other/file.txt".to_owned(), b"other-7".to_vec()),
                    ],
                );
                let spine_id = expected.root.id.to_string();
                let spine = mega_view_object::Entity::find_by_id(&spine_id)
                    .one(fixture.storage.view_storage().get_connection())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(spine.kind, 2);
            }
            assert_eq!(tip, commits[7].object_id);
        }
        let target = "1".repeat(40);
        let target_id = ObjectHash::from_hex_for_kind(HashKind::Sha1, &target).unwrap();
        let trees = (0..8)
            .map(|index| {
                let mut fixture = root_tree_from_paths(
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
                );
                let old_root_id = fixture.root.id;
                let old_repo_id = fixture
                    .root
                    .tree_items
                    .iter()
                    .find(|item| item.name == "repo")
                    .unwrap()
                    .id;
                let old_repo = fixture
                    .trees
                    .iter()
                    .find(|tree| tree.id == old_repo_id)
                    .unwrap();
                let mut repo_items = old_repo.tree_items.clone();
                repo_items.push(TreeItem::new(
                    TreeItemMode::Commit,
                    target_id,
                    "submodule".to_owned(),
                ));
                let new_repo = Tree::from_tree_items_with_kind(HashKind::Sha1, repo_items).unwrap();
                let mut root_items = fixture.root.tree_items.clone();
                root_items
                    .iter_mut()
                    .find(|item| item.name == "repo")
                    .unwrap()
                    .id = new_repo.id;
                let new_root = Tree::from_tree_items_with_kind(HashKind::Sha1, root_items).unwrap();
                fixture
                    .trees
                    .retain(|tree| tree.id != old_repo_id && tree.id != old_root_id);
                fixture.trees.extend([new_repo, new_root.clone()]);
                fixture.root = new_root;
                fixture
            })
            .collect::<Vec<RootTreeFixture>>();
        let temp = tempfile::tempdir().unwrap();
        let storage =
            test_storage_with_config(temp.path(), isolated_config(temp.path().join("config")))
                .await;
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
        let filter_id = insert_filter(&storage, filter_pk, ":/repo").await;
        let fixture = Fixture {
            _temp: temp,
            storage,
            roots,
            filter_pk,
            filter_id,
        };
        fixture.save_blobs().await;
        fixture.catch_up(1).await;
        let repo = fixture.current_repo();
        let commits = fixture
            .storage
            .view_storage()
            .view_pack_commits(filter_pk, 0, 8)
            .await
            .unwrap();
        assert_eq!(commits.len(), 8);
        for index in [2, 7] {
            let (_, ids) = pack_ids(
                prepared_full_pack(&repo, vec![commits[index].object_id.clone()]).await,
                &fixture._temp,
            )
            .await;
            assert!(!ids.contains(&target));
            let mut expected = HashSet::new();
            for (root, commit) in fixture.roots.iter().zip(&commits).take(index + 1) {
                expected.insert(commit.object_id.clone());
                expected.extend(fixture_view_closure(root, &commit.tree_id));
            }
            assert_eq!(ids, expected);
        }
    }

    #[tokio::test]
    async fn incremental_pack_have_exclusion() {
        let stable = (0..1500)
            .map(|index| {
                (
                    format!("a/big/file{index:04}"),
                    format!("stable-{index}").into_bytes(),
                )
            })
            .collect::<Vec<_>>();
        let old_target = "1".repeat(40);
        let new_target = "2".repeat(40);
        let trees = (0..12)
            .map(|index| {
                let mut paths = stable.clone();
                let moving = if index < 4 { "a/x/m" } else { "a/y/m" };
                let content_index = if index == 4 { 3 } else { index };
                paths.push((
                    moving.to_owned(),
                    format!("moving-{content_index}").into_bytes(),
                ));
                paths.push(("b/file".to_owned(), format!("other-{index}").into_bytes()));
                let mut fixture = root_tree_from_paths(HashKind::Sha1, &paths);
                if index >= 6 {
                    let target = if index == 6 { &old_target } else { &new_target };
                    let target = ObjectHash::from_hex_for_kind(HashKind::Sha1, target).unwrap();
                    let old_root_id = fixture.root.id;
                    let old_a_id = fixture
                        .root
                        .tree_items
                        .iter()
                        .find(|item| item.name == "a")
                        .unwrap()
                        .id;
                    let old_a = fixture
                        .trees
                        .iter()
                        .find(|tree| tree.id == old_a_id)
                        .unwrap();
                    let mut a_items = old_a.tree_items.clone();
                    a_items.push(TreeItem::new(
                        TreeItemMode::Commit,
                        target,
                        "gitlink".to_owned(),
                    ));
                    crate::jupiter::utils::converter::sort_git_tree_items(&mut a_items);
                    let new_a = Tree::from_tree_items_with_kind(HashKind::Sha1, a_items).unwrap();
                    let mut root_items = fixture.root.tree_items.clone();
                    root_items
                        .iter_mut()
                        .find(|item| item.name == "a")
                        .unwrap()
                        .id = new_a.id;
                    let new_root =
                        Tree::from_tree_items_with_kind(HashKind::Sha1, root_items).unwrap();
                    fixture
                        .trees
                        .retain(|tree| tree.id != old_a_id && tree.id != old_root_id);
                    fixture.trees.extend([new_a, new_root.clone()]);
                    fixture.root = new_root;
                }
                fixture
            })
            .collect::<Vec<_>>();
        let fixture = Fixture::from_trees(trees, ":/a", 2, 100).await;
        fixture.save_blobs().await;
        fixture.catch_up(1).await;
        let repo = fixture.current_repo();
        let commits = fixture
            .storage
            .view_storage()
            .view_pack_commits(fixture.filter_pk, 0, 12)
            .await
            .unwrap();
        assert_eq!(commits.len(), 12);
        let big_id = fixture.roots[0]
            .trees
            .iter()
            .find(|tree| tree.tree_items.len() == 1500)
            .unwrap()
            .id
            .to_string();
        let big_objects = fixture_view_closure(&fixture.roots[0], &big_id);
        assert_eq!(big_objects.len(), 1501);
        let want = commits[11].object_id.clone();
        for have_index in [1, 4, 8] {
            let (_, ids) = pack_ids(
                prepared_incremental_pack(
                    &repo,
                    vec![want.clone()],
                    vec![commits[have_index].object_id.clone()],
                )
                .await,
                &fixture._temp,
            )
            .await;
            let mut expected = HashSet::new();
            for (root, commit) in fixture.roots.iter().zip(&commits).skip(have_index + 1) {
                expected.insert(commit.object_id.clone());
                expected.insert(commit.tree_id.clone());
                let view_tree = root
                    .trees
                    .iter()
                    .find(|tree| tree.id.to_string() == commit.tree_id)
                    .unwrap();
                let moving = view_tree
                    .tree_items
                    .iter()
                    .find(|item| item.name == "x" || item.name == "y")
                    .unwrap();
                expected.insert(moving.id.to_string());
                let moving_tree = root.trees.iter().find(|tree| tree.id == moving.id).unwrap();
                expected.insert(moving_tree.tree_items[0].id.to_string());
            }
            assert_eq!(ids, expected);
            assert!(ids.is_disjoint(&big_objects));
            assert!(!ids.contains(&old_target) && !ids.contains(&new_target));
            let mut have_closure = HashSet::new();
            let mut want_closure = HashSet::new();
            for (index, (root, commit)) in fixture.roots.iter().zip(&commits).enumerate() {
                let closure = fixture_view_closure(root, &commit.tree_id);
                want_closure.insert(commit.object_id.clone());
                want_closure.extend(closure.iter().cloned());
                if index <= have_index {
                    have_closure.insert(commit.object_id.clone());
                    have_closure.extend(closure);
                }
            }
            have_closure.extend(ids);
            assert!(have_closure.is_superset(&want_closure));
        }
        let (_, full) = pack_ids(
            prepared_full_pack(&repo, vec![want.clone()]).await,
            &fixture._temp,
        )
        .await;
        assert!(!full.contains(&old_target) && !full.contains(&new_target));
        let (_, one_have) = pack_ids(
            prepared_incremental_pack(
                &repo,
                vec![want.clone()],
                vec![commits[4].object_id.clone()],
            )
            .await,
            &fixture._temp,
        )
        .await;
        let (_, multiple_haves) = pack_ids(
            prepared_incremental_pack(
                &repo,
                vec![want.clone()],
                vec![
                    commits[1].object_id.clone(),
                    commits[4].object_id.clone(),
                    fixture.roots[0].commit.id.to_string(),
                ],
            )
            .await,
            &fixture._temp,
        )
        .await;
        assert_eq!(one_have, multiple_haves);
        let other_pk = 5002;
        insert_filter(&fixture.storage, other_pk, ":/b").await;
        ViewProjectionService::new(fixture.storage.clone(), fixture.storage.view_metrics())
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
        let (_, off_chain) = pack_ids(
            prepared_incremental_pack(
                &repo,
                vec![want],
                vec![
                    fixture.roots[0].commit.id.to_string(),
                    other_tip,
                    "f".repeat(40),
                ],
            )
            .await,
            &fixture._temp,
        )
        .await;
        assert_eq!(full, off_chain);
    }

    #[tokio::test]
    async fn pack_statement_count_independent_of_chain() {
        let temp = tempfile::tempdir().unwrap();
        let (db_config, _schema) = test_db_config(temp.path()).await;
        let mut config = isolated_config(temp.path().join("config"));
        config.database = db_config.clone();
        config.views.batch_size = 250;
        let counter = Arc::new(AtomicUsize::new(0));
        let metric_counter = counter.clone();
        let mut connection = database_connection(&db_config).await.unwrap();
        connection.set_metric_callback(move |_| {
            metric_counter.fetch_add(1, Ordering::Relaxed);
        });
        let storage = Storage::new_with_connection(
            Arc::new(config),
            Arc::new(connection),
            mock_object_storage(),
        )
        .await
        .unwrap();
        let trees = (0..200)
            .map(|index| {
                root_tree_from_paths(
                    HashKind::Sha1,
                    &[
                        ("a/x/y/file".to_owned(), format!("a-{index}").into_bytes()),
                        (
                            "b/x/y/file".to_owned(),
                            format!("b-{}", index.min(9)).into_bytes(),
                        ),
                    ],
                )
            })
            .collect::<Vec<_>>();
        let roots = seed_linear_root_history_with_trees(
            storage.view_storage().get_connection(),
            HashKind::Sha1,
            trees,
        )
        .await;
        for root in &roots {
            for blob in &root.blobs {
                storage
                    .git_service
                    .save_object_from_raw(Bytes::copy_from_slice(&blob.data))
                    .await
                    .unwrap();
            }
        }
        assert_eq!(
            storage
                .view_storage()
                .extend_root_chain(None, 250, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::CaughtUp
        );
        let a = insert_filter(&storage, 5001, ":/a").await;
        let b = insert_filter(&storage, 5002, ":/b").await;
        let service = ViewProjectionService::new(storage.clone(), storage.view_metrics());
        for filter_pk in [5001, 5002] {
            service
                .catch_up_one_batch(filter_pk, &storage.config(), 250)
                .await
                .unwrap();
        }
        let a_repo = ViewRepo::new(storage.clone(), storage.config(), service.clone(), 5001, a);
        let b_repo = ViewRepo::new(storage.clone(), storage.config(), service, 5002, b);
        let a_commits = storage
            .view_storage()
            .view_pack_commits(5001, 0, 200)
            .await
            .unwrap();
        let b_commits = storage
            .view_storage()
            .view_pack_commits(5002, 0, 200)
            .await
            .unwrap();
        assert_eq!(a_commits.len(), 200);
        assert_eq!(b_commits.len(), 10);
        counter.store(0, Ordering::Relaxed);
        prepared_full_pack(&a_repo, vec![a_commits[199].object_id.clone()])
            .await
            .concat()
            .await;
        let full_a = counter.load(Ordering::Relaxed);
        counter.store(0, Ordering::Relaxed);
        prepared_full_pack(&b_repo, vec![b_commits[9].object_id.clone()])
            .await
            .concat()
            .await;
        let full_b = counter.load(Ordering::Relaxed);
        assert_eq!(full_a, full_b);
        counter.store(0, Ordering::Relaxed);
        prepared_incremental_pack(
            &a_repo,
            vec![a_commits[199].object_id.clone()],
            vec![a_commits[198].object_id.clone()],
        )
        .await
        .concat()
        .await;
        let short = counter.load(Ordering::Relaxed);
        counter.store(0, Ordering::Relaxed);
        prepared_incremental_pack(
            &a_repo,
            vec![a_commits[199].object_id.clone()],
            vec![a_commits[49].object_id.clone()],
        )
        .await
        .concat()
        .await;
        assert_eq!(short, counter.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn commit_map_read_only_by_prepare_pack() {
        let temp = tempfile::tempdir().unwrap();
        let (db_config, _schema) = test_db_config(temp.path()).await;
        let mut config = isolated_config(temp.path().join("config"));
        config.database = db_config.clone();
        let statements = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = statements.clone();
        let mut connection = database_connection(&db_config).await.unwrap();
        connection.set_metric_callback(move |info| {
            recorded.lock().unwrap().push(info.statement.to_string());
        });
        let storage = Storage::new_with_connection(
            Arc::new(config),
            Arc::new(connection),
            mock_object_storage(),
        )
        .await
        .unwrap();
        let trees = (0..3)
            .map(|index| {
                root_tree_from_paths(
                    HashKind::Sha1,
                    &[("a/file".to_owned(), format!("content-{index}").into_bytes())],
                )
            })
            .collect::<Vec<_>>();
        let roots = seed_linear_root_history_with_trees(
            storage.view_storage().get_connection(),
            HashKind::Sha1,
            trees,
        )
        .await;
        for root in &roots {
            for blob in &root.blobs {
                storage
                    .git_service
                    .save_object_from_raw(Bytes::copy_from_slice(&blob.data))
                    .await
                    .unwrap();
            }
        }
        assert_eq!(
            storage
                .view_storage()
                .extend_root_chain(None, 100, ViewLockMode::Try)
                .await
                .unwrap(),
            RootChainOutcome::CaughtUp
        );
        let filter_id = insert_filter(&storage, 5001, ":/a").await;
        let projection = ViewProjectionService::new(storage.clone(), storage.view_metrics());
        projection
            .catch_up_one_batch(5001, &storage.config(), 100)
            .await
            .unwrap();
        let commits = storage
            .view_storage()
            .view_pack_commits(5001, 0, 3)
            .await
            .unwrap();
        assert_eq!(commits.len(), 3);
        let want = vec![commits[2].object_id.clone()];
        for have in [Vec::new(), vec![commits[0].object_id.clone()]] {
            let repo = ViewRepo::new(
                storage.clone(),
                storage.config(),
                projection.clone(),
                5001,
                filter_id.clone(),
            );
            statements.lock().unwrap().clear();
            repo.prepare_pack(&want, &have).await.unwrap();
            {
                let sql = statements.lock().unwrap();
                let filter_reads = sql
                    .iter()
                    .filter(|statement| statement.contains("mega_view_filter"))
                    .collect::<Vec<_>>();
                assert_eq!(filter_reads.len(), 1);
                assert!(filter_reads[0].contains("mega_view_commit_map"));
            }
            if have.is_empty() {
                repo.full_pack(want.clone()).await.unwrap().concat().await;
            } else {
                repo.incremental_pack(want.clone(), have)
                    .await
                    .unwrap()
                    .concat()
                    .await;
            }
            assert_eq!(
                statements
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|statement| statement.contains("mega_view_commit_map"))
                    .count(),
                1
            );
        }
        let config = storage.config();
        let control = ViewRepo::new(storage, config, projection, 5001, filter_id);
        statements.lock().unwrap().clear();
        assert!(control.full_pack(want).await.is_err());
        assert_eq!(
            statements
                .lock()
                .unwrap()
                .iter()
                .filter(|statement| statement.contains("mega_view_commit_map"))
                .count(),
            0
        );
    }

    #[test]
    fn prepare_pack_rejects_l0_and_missing_trees() {
        let (expected, logs) = capture_tracing(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let paths = |index| {
                    [
                        (
                            "a/mode/good.txt".to_owned(),
                            format!("mode-{index}").into_bytes(),
                        ),
                        (
                            "b/gbk/good.txt".to_owned(),
                            format!("gbk-{index}").into_bytes(),
                        ),
                        (
                            "c/d/hp23-only-in-c-d.txt".to_owned(),
                            format!("missing-{index}").into_bytes(),
                        ),
                        (
                            "e/ok.txt".to_owned(),
                            format!("control-{index}").into_bytes(),
                        ),
                    ]
                };
                let first = root_tree_from_paths(HashKind::Sha1, &paths(0));
                let mut second = root_tree_from_paths(HashKind::Sha1, &paths(1));
                let blob = second.blobs[0].id;
                let (mode_id, mode_raw) = raw_l0_tree("100664", b"file.txt", blob);
                let (gbk_id, gbk_raw) =
                    raw_l0_tree("100644", &[0xc4, 0xe3, 0xba, 0xc3, b'.', b't', b'x', b't'], blob);
                replace_fixture_subtree(&mut second, "a", "mode", mode_id);
                replace_fixture_subtree(&mut second, "b", "gbk", gbk_id);
                let fixture = Fixture::from_trees(vec![first, second], ":/a/mode", 2, 100).await;
                let db = fixture.storage.view_storage();
                for (tree_id, raw) in [(mode_id, mode_raw), (gbk_id, gbk_raw)] {
                    let entry = Entry {
                        obj_type: ObjectType::Tree,
                        data: raw,
                        hash: tree_id,
                        chain_len: 0,
                    };
                    let MegaObjectModel::Tree(mut model) =
                        process_entry(entry).convert_to_mega_model(EntryMeta::new())
                    else {
                        panic!("tree entry must make a tree model")
                    };
                    model.commit_id = fixture.roots[1].commit.id.to_string();
                    assert_ne!(
                        ObjectHash::from_type_and_data_for_kind(
                            HashKind::Sha1,
                            ObjectType::Tree,
                            &model.sub_trees
                        )
                        .unwrap()
                        .to_string(),
                        model.tree_id
                    );
                    fixture
                        .storage
                        .mono_storage()
                        .base
                        .batch_save_model::<mega_tree::Entity, mega_tree::ActiveModel>(vec![
                            model.into_active_model(),
                        ])
                        .await
                        .unwrap();
                }
                let c_d_id = fixture.roots[1]
                    .trees
                    .iter()
                    .find(|tree| {
                        tree.tree_items
                            .iter()
                            .any(|item| item.name == "hp23-only-in-c-d.txt")
                    })
                    .unwrap()
                    .id
                    .to_string();
                let filters = [
                    (5001, fixture.filter_id.clone(), mode_id.to_string()),
                    (
                        5002,
                        insert_filter(&fixture.storage, 5002, ":/b/gbk:prefix=p").await,
                        gbk_id.to_string(),
                    ),
                    (
                        5003,
                        insert_filter(&fixture.storage, 5003, ":/c").await,
                        c_d_id.clone(),
                    ),
                    (
                        5004,
                        insert_filter(&fixture.storage, 5004, ":/e").await,
                        String::new(),
                    ),
                ];
                let mut original = HashMap::new();
                for (pk, _, _) in &filters {
                    ViewProjectionService::new(
                        fixture.storage.clone(),
                        fixture.storage.view_metrics(),
                    )
                    .catch_up_one_batch(*pk, &fixture.storage.config(), 100)
                    .await
                    .unwrap();
                    let commits = db.view_pack_commits(*pk, 0, 2).await.unwrap();
                    assert_eq!(commits.len(), 2);
                    original.insert(
                        *pk,
                        commits
                            .into_iter()
                            .map(|commit| commit.object_id)
                            .collect::<Vec<_>>(),
                    );
                }
                for table in ["mega_view_object_ref", "mega_view_commit_map"] {
                    db.get_connection()
                        .execute_unprepared(&format!("DELETE FROM {table}"))
                        .await
                        .unwrap();
                }
                db.get_connection()
                    .execute_unprepared("DELETE FROM mega_view_object")
                    .await
                    .unwrap();
                db.get_connection()
                    .execute_unprepared(
                        "UPDATE mega_view_filter SET projected_seq = 0, ready_seq = NULL, \
                         warming_since = now() WHERE id BETWEEN 5001 AND 5004",
                    )
                    .await
                    .unwrap();
                for (pk, _, _) in &filters {
                    ViewProjectionService::new(
                        fixture.storage.clone(),
                        fixture.storage.view_metrics(),
                    )
                    .catch_up_one_batch(*pk, &fixture.storage.config(), 100)
                    .await
                    .unwrap();
                    let current = db
                        .view_pack_commits(*pk, 0, 2)
                        .await
                        .unwrap()
                        .into_iter()
                        .map(|commit| commit.object_id)
                        .collect::<Vec<_>>();
                    assert_eq!(current, original[pk]);
                    assert!(
                        db.view_reader_state(*pk)
                            .await
                            .unwrap()
                            .unwrap()
                            .ready_seq
                            .is_some()
                    );
                }
                for (pk, filter_id, tree_id) in [&filters[0], &filters[1], &filters[3]] {
                    let before = mega_view_filter::Entity::find_by_id(*pk)
                        .one(db.get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    let repo = ViewRepo::new(
                        fixture.storage.clone(),
                        fixture.storage.config(),
                        ViewProjectionService::new(
                            fixture.storage.clone(),
                            fixture.storage.view_metrics(),
                        ),
                        *pk,
                        filter_id.clone(),
                    );
                    let want = vec![original[pk][1].clone()];
                    repo.check_wants_and_ready(&want).await.unwrap();
                    let result = repo.prepare_pack(&want, &[]).await;
                    if *pk == 5004 {
                        result.unwrap();
                    } else {
                        assert!(matches!(
                            result.unwrap_err(),
                            MegaError::ViewPackRejected(message)
                            if message == format!(
                                "view {filter_id} pack aborted: tree {tree_id} does not match its stored entries"
                            )
                        ));
                    }
                    let after = mega_view_filter::Entity::find_by_id(*pk)
                        .one(db.get_connection())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(before, after);
                }
                assert_eq!(
                    fixture
                        .storage
                        .view_metrics()
                        .counters()
                        .view_pack_tree_mismatch_total,
                    2
                );
                assert_eq!(
                    fixture
                        .roots
                        .iter()
                        .flat_map(|root| &root.trees)
                        .filter(|tree| tree.id.to_string() == c_d_id)
                        .count(),
                    1
                );
                db.get_connection()
                    .execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "DELETE FROM mega_tree WHERE tree_id = $1",
                        [sea_orm::Value::from(c_d_id.clone())],
                    ))
                    .await
                    .unwrap();
                let (pk, filter_id, tree_id) = &filters[2];
                let before = mega_view_filter::Entity::find_by_id(*pk)
                    .one(db.get_connection())
                    .await
                    .unwrap()
                    .unwrap();
                let repo = ViewRepo::new(
                    fixture.storage.clone(),
                    fixture.storage.config(),
                    ViewProjectionService::new(
                        fixture.storage.clone(),
                        fixture.storage.view_metrics(),
                    ),
                    *pk,
                    filter_id.clone(),
                );
                let want = vec![original[pk][1].clone()];
                repo.check_wants_and_ready(&want).await.unwrap();
                assert!(matches!(
                    repo.prepare_pack(&want, &[]).await.unwrap_err(),
                    MegaError::ViewPackRejected(message)
                    if message == format!("view {filter_id} pack aborted: tree {tree_id} is missing")
                ));
                let after = mega_view_filter::Entity::find_by_id(*pk)
                    .one(db.get_connection())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(before, after);
                assert_eq!(
                    fixture
                        .storage
                        .view_metrics()
                        .counters()
                        .view_pack_tree_mismatch_total,
                    2
                );
                filters
            })
        });
        for (pk, filter_id, tree_id) in &expected {
            let lines = logs
                .lines()
                .filter(|line| {
                    line.contains("ERROR")
                        && line.contains(&format!("filter_id={filter_id}"))
                        && (*pk == 5004 || line.contains(&format!("tree_id={tree_id}")))
                })
                .collect::<Vec<_>>();
            if *pk == 5004 {
                assert!(lines.is_empty());
            } else {
                assert_eq!(lines.len(), 1, "{pk}: {logs}");
                assert_eq!(
                    lines[0].contains("metric=view_pack_tree_mismatch_total"),
                    *pk != 5003
                );
            }
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
    async fn prepare_pack_rechecks_single_snapshot() {
        for scenario in 0..5 {
            let fixture = Fixture::new(3, ":/repo").await;
            fixture.catch_up(1).await;
            let commits = fixture
                .storage
                .view_storage()
                .view_pack_commits(fixture.filter_pk, 0, 3)
                .await
                .unwrap();
            assert_eq!(commits.len(), 3);
            let want = vec![commits[2].object_id.clone()];
            let r1 = fixture.current_repo();
            let r2 = fixture.current_repo();
            r1.check_wants_and_ready(&want).await.unwrap();
            r2.check_wants_and_ready(&want).await.unwrap();
            let db = fixture.storage.view_storage();
            match scenario {
                0 => {
                    fixture.set_state(0, None, false).await;
                    db.get_connection()
                        .execute_raw(Statement::from_sql_and_values(
                            DbBackend::Postgres,
                            "DELETE FROM mega_view_object_ref WHERE filter_pk = $1",
                            [sea_orm::Value::from(fixture.filter_pk)],
                        ))
                        .await
                        .unwrap();
                    db.get_connection()
                        .execute_raw(Statement::from_sql_and_values(
                            DbBackend::Postgres,
                            "DELETE FROM mega_view_commit_map WHERE filter_pk = $1",
                            [sea_orm::Value::from(fixture.filter_pk)],
                        ))
                        .await
                        .unwrap();
                }
                1 => fixture.set_state(3, None, false).await,
                2 => {
                    db.get_connection()
                        .execute_raw(Statement::from_sql_and_values(
                            DbBackend::Postgres,
                            "DELETE FROM mega_view_commit_map \
                             WHERE filter_pk = $1 AND view_commit = $2",
                            [
                                sea_orm::Value::from(fixture.filter_pk),
                                sea_orm::Value::from(want[0].clone()),
                            ],
                        ))
                        .await
                        .unwrap();
                }
                3 => {
                    assert!(
                        cas_fixture_main(db.get_connection(), &fixture.roots[2], &fixture.roots[0])
                            .await
                    );
                    assert!(matches!(
                        db.extend_root_chain(None, 100, ViewLockMode::Try)
                            .await
                            .unwrap(),
                        RootChainOutcome::Discontinuous(_)
                    ));
                }
                4 => {}
                _ => unreachable!(),
            }
            for (repo, have) in [(&r1, Vec::new()), (&r2, vec![commits[0].object_id.clone()])] {
                let result = repo.prepare_pack(&want, &have).await;
                if scenario == 4 {
                    result.unwrap();
                } else {
                    assert_unavailable(
                        result.unwrap_err(),
                        &fixture.filter_id,
                        if scenario == 3 {
                            ViewUnavailableReason::RootChainHalted
                        } else {
                            ViewUnavailableReason::WarmingUp
                        },
                    );
                }
            }
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
            !matches!(repo.full_pack(vec![own.clone()]).await.err(), Some(GitError::CustomError(message)) if message.contains("view pack not implemented"))
        );
        assert!(
            !matches!(repo.incremental_pack(vec![own.clone()], vec![]).await.err(), Some(GitError::CustomError(message)) if message.contains("view pack not implemented"))
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
