use std::{
    collections::{HashMap, HashSet},
    path::{Component, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    vec,
};

use async_recursion::async_recursion;
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt, stream};
use git_internal::{
    errors::GitError,
    hash::ObjectHash,
    internal::{
        metadata::{EntryMeta, MetaAttached},
        object::{blob::Blob, commit::Commit, tree::Tree, types::ObjectType},
        pack::{encode::PackEncoder, entry::Entry},
    },
};
use sea_orm::DatabaseTransaction;
use tokio::sync::{RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    bellatrix::Bellatrix,
    callisto::{
        entity_ext::generate_link,
        mega_cl, mega_code_review_anchor, mega_refs,
        sea_orm_active_enums::{PositionStatusEnum, RefTypeEnum},
    },
    ceres::{
        api_service::{ApiHandler, cache::GitObjectCache, mono_api_service::MonoApiService},
        code_edit::{model::collect_cl_chain, on_push::OnpushCodeEdit, utils::get_changed_files},
        model::change_list::ClDiffFile,
        pack::{
            RepoHandler,
            push_chain::{self, PushChain, PushChainResolution},
        },
        protocol::import_refs::{CommandType, RefCommand, Refs},
    },
    common::{
        errors::MegaError,
        utils::{self, MEGA_BRANCH_NAME, ZERO_ID},
    },
    contract::{
        api::common::Pagination,
        policy::notify::{authz_blob_id, notify_authz_changed_best_effort},
    },
    jupiter::{
        storage::{Storage, base_storage::StorageConnector, mono_storage::MonoStorage},
        utils::converter::FromMegaModel,
    },
};
#[rustfmt::skip]
use crate::orbit_api::{error::IoOrbitError, object_storage::MultiObjectByteStream};

pub struct MonoRepo {
    pub storage: Storage,
    pub git_object_cache: Arc<GitObjectCache>,
    pub path: PathBuf,
    pub base_branch: String,
    /// Object ids of every commit the current unpack delivered (presence).
    /// Drives the ADR-MC-05 no-op split and the "pack carries the ref target"
    /// fail-closed check — presence, not newness, keeps rejections sticky for
    /// a retried push whose objects are already stored. base/tip never derive
    /// from pack arrival order (GC-MC-13); the [`PushChain`](crate::ceres::pack::push_chain::PushChain)
    /// is built from `command_list` plus these sets at finalize.
    pub pack_commit_ids: Mutex<HashSet<String>>,
    /// The subset of `pack_commit_ids` that was absent from storage at unpack
    /// time (Codex R1 P1-1). Bounds the fork-point walk in
    /// [`PushChain::resolve`]: a pack may redundantly carry server-known
    /// ancestors, and only newly introduced commits extend the chain.
    pub new_commit_ids: Mutex<HashSet<String>>,
    /// ADR-MC-05 no-op notice set by `build_push_chain`; read by the protocol
    /// layer via `receive_pack_notice` after a successful finalize.
    pub no_op_notice: Mutex<Option<String>>,
    /// Resolved push chains keyed by `new_id` (MC01-R1 P2-2): each branch
    /// command's chain is built once per receive-pack — a single-commit push
    /// costs exactly one DB read and logs the ADR-MC-05 notice once.
    pub push_chain_cache: Mutex<HashMap<String, Option<PushChain>>>,
    pub cl_link: Arc<RwLock<Option<String>>>,
    pub bellatrix: Arc<Bellatrix>,
    pub username: Option<String>,
    /// Ref commands for this push (same role as on [`ImportRepo`](crate::ceres::pack::import_repo::ImportRepo)).
    pub command_list: Mutex<Vec<RefCommand>>,
}

#[async_trait]
impl RepoHandler for MonoRepo {
    fn is_monorepo(&self) -> bool {
        true
    }

    fn save_entry_concurrency(&self) -> usize {
        self.storage.config().pack.save_entry_concurrency
    }

    fn sync_commands_after_unpack(&self, commands: &[RefCommand]) {
        *self
            .command_list
            .lock()
            .expect("command_list lock poisoned") = commands.to_vec();
    }

    fn receive_pack_notice(&self) -> Option<String> {
        self.no_op_notice
            .lock()
            .expect("no_op_notice lock poisoned")
            .clone()
    }

    /// Codex R3 P1: MonoRepo binds the accepted chain's newly introduced
    /// commits in its post-push pipeline; the protocol layer must not re-upsert
    /// the tip (an empty-pack idempotent re-push or a known-tip push would
    /// otherwise clobber the existing binding).
    fn bind_tip_after_receive(&self) -> bool {
        false
    }

    async fn refs_with_head_hash(&self) -> (String, Vec<Refs>) {
        let storage = self.storage.mono_storage();

        let path_refs = storage
            .get_all_refs(self.path.to_str().unwrap(), false)
            .await
            .unwrap();

        let heads_exist = path_refs
            .iter()
            .any(|x| x.ref_name == crate::common::utils::MEGA_BRANCH_NAME);

        let refs = if heads_exist {
            let refs: Vec<Refs> = path_refs.into_iter().map(|x| x.into()).collect();
            refs
        } else {
            let target_path = self.path.clone();
            let mut refs = vec![];

            let root_refs = storage.get_all_refs("/", true).await.unwrap();

            for root_ref in root_refs {
                let (tree_hash, commit_hash) = (root_ref.ref_tree_hash, root_ref.ref_commit_hash);
                let mut tree: Tree = Tree::from_mega_model(
                    storage.get_tree_by_hash(&tree_hash).await.unwrap().unwrap(),
                );

                let commit: Commit = Commit::from_mega_model(
                    storage
                        .get_commit_by_hash(&commit_hash)
                        .await
                        .unwrap()
                        .unwrap(),
                );

                for component in target_path.components() {
                    if component != Component::RootDir {
                        let path_compo_name = component.as_os_str().to_str().unwrap();
                        let path_compo_hash = tree
                            .tree_items
                            .iter()
                            .find(|x| x.name == path_compo_name)
                            .map(|x| x.id);
                        if let Some(hash) = path_compo_hash {
                            tree = Tree::from_mega_model(
                                storage
                                    .get_tree_by_hash(&hash.to_string())
                                    .await
                                    .unwrap()
                                    .unwrap(),
                            );
                        } else {
                            return (ZERO_ID.to_string(), vec![]);
                        }
                    }
                }
                let c = Commit::new(
                    commit.author,
                    commit.committer,
                    tree.id,
                    vec![],
                    &commit.message,
                );

                let new_mega_ref = match mega_refs::Model::new(
                    &self.path,
                    root_ref.ref_name.clone(),
                    c.id.to_string(),
                    c.tree_id.to_string(),
                    false,
                ) {
                    Ok(new_ref) => new_ref,
                    Err(error) => {
                        tracing::error!(error = %error, "failed to allocate monorepo ref ID");
                        return (ZERO_ID.to_string(), vec![]);
                    }
                };

                storage
                    .mega_head_hash_with_txn(new_mega_ref.clone(), c)
                    .await
                    .unwrap();

                refs.push(new_mega_ref.into());
            }
            refs
        };
        self.find_head_hash(refs)
    }

    async fn finalize_receive_pack(&self) -> Result<(), MegaError> {
        self.validate_incoming_push().await?;
        self.persist_mono_branch_cl_mega_refs_transaction().await?;
        self.run_mono_post_push_pipeline().await
    }

    async fn save_entry(
        &self,
        entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<(), MegaError> {
        // ADR-MC-06: object `commit_id` attribution = the push chain tip, i.e.
        // the branch command's `new_id` ("the chain tip of the push that last
        // touched the object on this path") — derived from the ref commands
        // alone, independent of pack arrival order (MC01-R2 P1-2; safety
        // analysis on `push_chain::attribution_commit_id`). Live reader: the
        // file browser's "last commit" column (`item_to_commit_map` →
        // `preview_router::get_tree_commit_info`) — exactly correct under the
        // single-commit-per-push constraint, an approximate value once MC-06
        // opens multi-commit pushes (recorded in ADR-MC-06 / DEFER-MC-03).
        //
        // Codex R1 P1-2: no commit binding here — unpack runs before chain
        // validation, so binding here would let a rejected push upsert
        // `commit_auths` (overwriting existing bindings' usernames). Bindings
        // are written post-finalize for the accepted chain only (see
        // `run_mono_post_push_pipeline`).
        let commit_id = push_chain::attribution_commit_id(
            &self
                .command_list
                .lock()
                .expect("command_list lock poisoned"),
        );
        self.storage
            .mono_service
            .save_entry(&commit_id, entry_list)
            .await?;
        Ok(())
    }

    async fn update_pack_id(&self, temp_pack_id: &str, pack_id: &str) -> Result<(), MegaError> {
        let storage = self.storage.mono_storage();
        storage.update_pack_id(temp_pack_id, pack_id).await
    }

    async fn check_entry(&self, entry: &Entry) -> Result<(), GitError> {
        // MC-06: multi-commit packs are admitted. Commit entries are only
        // recorded here; consistency with the ref update is enforced at
        // finalize — `PushChain::resolve` fail-closes when the pack does not
        // carry the ref update target, and `PushChain::validate` (MC-03) gates
        // the chain before any ref/CL mutation. Objects of a rejected push
        // stay harmless: `batch_save_model` is insert-only (on-conflict
        // do-nothing), so pre-existing rows are untouched and new rows are
        // unreachable garbage (MC01-R2 P2-B accepted limitation: early-flushed
        // batches of a failed push may keep tip attribution until DEFER-MC-03).
        if entry.obj_type == ObjectType::Commit {
            let hash = entry.hash.to_string();
            self.pack_commit_ids
                .lock()
                .expect("pack_commit_ids lock poisoned")
                .insert(hash.clone());
            // Codex R1 P1-1: only commits absent from storage count as newly
            // introduced — packs may redundantly carry server-known ancestors,
            // and the fork-point walk must not extend the chain past the true
            // fork point. Fail closed on a storage error (newness is
            // undecidable then).
            let known = self
                .storage
                .mono_storage()
                .get_commit_by_hash(&hash)
                .await
                .map_err(|e| {
                    GitError::CustomError(format!("commit existence check failed for {hash}: {e}"))
                })?
                .is_some();
            if !known {
                self.new_commit_ids
                    .lock()
                    .expect("new_commit_ids lock poisoned")
                    .insert(hash);
            }
        }
        Ok(())
    }

    async fn full_pack(&self, want: Vec<String>) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        self.incremental_pack(want, Vec::new()).await
    }

    fn supports_shallow_fetch(&self) -> bool {
        true
    }

    async fn shallow_pack(
        &self,
        want: Vec<String>,
        depth: u32,
        _deepen_relative: bool,
    ) -> Result<(ReceiverStream<Vec<u8>>, Vec<String>), GitError> {
        let pack_config = &self.storage.config().pack;
        let storage = self.storage.mono_storage();
        let obj_num = AtomicUsize::new(0);

        let mut exist_objs = HashSet::new();

        let want_commits: Vec<Commit> = storage
            .get_commits_by_hashes(&want)
            .await
            .unwrap()
            .into_iter()
            .map(Commit::from_mega_model)
            .collect();

        let mut shallow_commits: Vec<String> = Vec::new();
        let mut visited: HashSet<String> = want.iter().cloned().collect();
        let mut current_level: Vec<Commit> = want_commits.clone();
        let mut all_commits: Vec<Commit> = want_commits.clone();

        for level in 0..depth {
            let mut next_level: Vec<Commit> = Vec::new();
            for commit in &current_level {
                for p_commit_id in &commit.parent_commit_ids {
                    let p_id = p_commit_id.to_string();
                    if visited.insert(p_id.clone())
                        && let Some(model) = storage.get_commit_by_hash(&p_id).await.unwrap()
                    {
                        let parent = Commit::from_mega_model(model);
                        if level + 1 == depth {
                            shallow_commits.push(p_id);
                        } else {
                            next_level.push(parent.clone());
                        }
                        all_commits.push(parent);
                    }
                }
            }
            current_level = next_level;
        }

        let want_tree_ids = all_commits.iter().map(|c| c.tree_id.to_string()).collect();
        let want_trees: HashMap<ObjectHash, Tree> = storage
            .get_trees_by_hashes(want_tree_ids)
            .await
            .unwrap()
            .into_iter()
            .map(|m| {
                (
                    ObjectHash::from_str(&m.tree_id).unwrap(),
                    Tree::from_mega_model(m),
                )
            })
            .collect();

        obj_num.fetch_add(all_commits.len(), Ordering::SeqCst);

        let mut counted_obj = HashSet::new();
        for c in &all_commits {
            self.traverse_for_count(
                want_trees.get(&c.tree_id).unwrap().clone(),
                &exist_objs,
                &mut counted_obj,
                &obj_num,
            )
            .await?;
        }

        let (entry_tx, entry_rx) = mpsc::channel(pack_config.channel_message_size);
        let (stream_tx, stream_rx) = mpsc::channel(pack_config.channel_message_size);
        let encoder = PackEncoder::new(obj_num.into_inner(), 0, stream_tx);
        encoder
            .encode_async(entry_rx)
            .await
            .map_err(|e| MegaError::Other(format!("pack encode failed: {e}")))?;

        for c in all_commits {
            self.traverse(
                want_trees.get(&c.tree_id).unwrap().clone(),
                &mut exist_objs,
                Some(&entry_tx),
            )
            .await?;
            entry_tx
                .send(MetaAttached {
                    inner: c.into(),
                    meta: EntryMeta::new(),
                })
                .await
                .map_err(|e| MegaError::Other(format!("pack commit entry send failed: {e}")))?;
        }
        drop(entry_tx);

        Ok((ReceiverStream::new(stream_rx), shallow_commits))
    }

    fn supports_filtered_fetch(&self) -> bool {
        true
    }

    async fn filtered_pack(
        &self,
        want: Vec<String>,
        have: Vec<String>,
        filter_spec: &str,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        if filter_spec != "blob:none" {
            return Err(GitError::CustomError(format!(
                "unsupported filter spec: {filter_spec}"
            )));
        }

        let mut want_clone = want.clone();
        let pack_config = &self.storage.config().pack;
        let storage = self.storage.mono_storage();
        let obj_num = AtomicUsize::new(0);

        let mut exist_objs = HashSet::new();

        let mut want_commits: Vec<Commit> = storage
            .get_commits_by_hashes(&want_clone)
            .await
            .unwrap()
            .into_iter()
            .map(Commit::from_mega_model)
            .collect();
        if want_commits.is_empty() {
            return self.direct_object_pack(want).await;
        }
        let mut traversal_list: Vec<Commit> = want_commits.clone();

        while let Some(temp) = traversal_list.pop() {
            for p_commit_id in temp.parent_commit_ids {
                let p_commit_id = p_commit_id.to_string();

                if !have.contains(&p_commit_id) && !want_clone.contains(&p_commit_id) {
                    let parent: Commit = Commit::from_mega_model(
                        storage
                            .get_commit_by_hash(&p_commit_id)
                            .await
                            .unwrap()
                            .unwrap(),
                    );
                    want_commits.push(parent.clone());
                    want_clone.push(p_commit_id);
                    traversal_list.push(parent);
                }
            }
        }

        let want_tree_ids = want_commits.iter().map(|c| c.tree_id.to_string()).collect();
        let want_trees: HashMap<ObjectHash, Tree> = storage
            .get_trees_by_hashes(want_tree_ids)
            .await
            .unwrap()
            .into_iter()
            .map(|m| {
                (
                    ObjectHash::from_str(&m.tree_id).unwrap(),
                    Tree::from_mega_model(m),
                )
            })
            .collect();

        obj_num.fetch_add(want_commits.len(), Ordering::SeqCst);

        let have_commits = storage.get_commits_by_hashes(&have).await.unwrap();
        let have_trees = storage
            .get_trees_by_hashes(have_commits.iter().map(|x| x.tree.clone()).collect())
            .await
            .unwrap();
        for have_tree in have_trees {
            self.traverse(Tree::from_mega_model(have_tree), &mut exist_objs, None)
                .await?;
        }

        let mut counted_obj = HashSet::new();
        for c in want_commits.clone() {
            self.traverse_trees_only_for_count(
                want_trees.get(&c.tree_id).unwrap().clone(),
                &exist_objs,
                &mut counted_obj,
                &obj_num,
            )
            .await?;
        }

        let (entry_tx, entry_rx) = mpsc::channel(pack_config.channel_message_size);
        let (stream_tx, stream_rx) = mpsc::channel(pack_config.channel_message_size);
        let encoder = PackEncoder::new(obj_num.into_inner(), 0, stream_tx);
        encoder
            .encode_async(entry_rx)
            .await
            .map_err(|e| MegaError::Other(format!("pack encode failed: {e}")))?;

        for c in want_commits {
            self.traverse_trees_only(
                want_trees.get(&c.tree_id).unwrap().clone(),
                &mut exist_objs,
                Some(&entry_tx),
            )
            .await?;
            entry_tx
                .send(MetaAttached {
                    inner: c.into(),
                    meta: EntryMeta::new(),
                })
                .await
                .map_err(|e| MegaError::Other(format!("pack commit entry send failed: {e}")))?;
        }
        drop(entry_tx);

        Ok(ReceiverStream::new(stream_rx))
    }

    async fn incremental_pack(
        &self,
        want: Vec<String>,
        have: Vec<String>,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        let mut want_clone = want.clone();
        let pack_config = &self.storage.config().pack;
        let storage = self.storage.mono_storage();
        let obj_num = AtomicUsize::new(0);

        let mut exist_objs = HashSet::new();

        let mut want_commits: Vec<Commit> = storage
            .get_commits_by_hashes(&want_clone)
            .await
            .unwrap()
            .into_iter()
            .map(Commit::from_mega_model)
            .collect();
        if want_commits.is_empty() {
            return self.direct_object_pack(want).await;
        }
        let mut traversal_list: Vec<Commit> = want_commits.clone();

        // traverse commit's all parents to find the commit that client does not have
        while let Some(temp) = traversal_list.pop() {
            for p_commit_id in temp.parent_commit_ids {
                let p_commit_id = p_commit_id.to_string();

                if !have.contains(&p_commit_id) && !want_clone.contains(&p_commit_id) {
                    let parent: Commit = Commit::from_mega_model(
                        storage
                            .get_commit_by_hash(&p_commit_id)
                            .await
                            .unwrap()
                            .unwrap(),
                    );
                    want_commits.push(parent.clone());
                    want_clone.push(p_commit_id);
                    traversal_list.push(parent);
                }
            }
        }

        let want_tree_ids = want_commits.iter().map(|c| c.tree_id.to_string()).collect();
        let want_trees: HashMap<ObjectHash, Tree> = storage
            .get_trees_by_hashes(want_tree_ids)
            .await
            .unwrap()
            .into_iter()
            .map(|m| {
                (
                    ObjectHash::from_str(&m.tree_id).unwrap(),
                    Tree::from_mega_model(m),
                )
            })
            .collect();

        obj_num.fetch_add(want_commits.len(), Ordering::SeqCst);

        let have_commits = storage.get_commits_by_hashes(&have).await.unwrap();
        let have_trees = storage
            .get_trees_by_hashes(have_commits.iter().map(|x| x.tree.clone()).collect())
            .await
            .unwrap();
        for have_tree in have_trees {
            self.traverse(Tree::from_mega_model(have_tree), &mut exist_objs, None)
                .await?;
        }

        let mut counted_obj = HashSet::new();
        // traverse for get obj nums
        for c in want_commits.clone() {
            self.traverse_for_count(
                want_trees.get(&c.tree_id).unwrap().clone(),
                &exist_objs,
                &mut counted_obj,
                &obj_num,
            )
            .await?;
        }
        let (entry_tx, entry_rx) = mpsc::channel(pack_config.channel_message_size);
        let (stream_tx, stream_rx) = mpsc::channel(pack_config.channel_message_size);
        let encoder = PackEncoder::new(obj_num.into_inner(), 0, stream_tx);
        encoder
            .encode_async(entry_rx)
            .await
            .map_err(|e| MegaError::Other(format!("pack encode failed: {e}")))?;
        // todo: For now, send metadata only for blob objects.
        for c in want_commits {
            self.traverse(
                want_trees.get(&c.tree_id).unwrap().clone(),
                &mut exist_objs,
                Some(&entry_tx),
            )
            .await?;
            entry_tx
                .send(MetaAttached {
                    inner: c.into(),
                    meta: EntryMeta::new(),
                })
                .await
                .map_err(|e| MegaError::Other(format!("pack commit entry send failed: {e}")))?;
        }
        drop(entry_tx);

        Ok(ReceiverStream::new(stream_rx))
    }

    async fn get_trees_by_hashes(&self, hashes: Vec<String>) -> Result<Vec<Tree>, MegaError> {
        Ok(self
            .storage
            .mono_storage()
            .get_trees_by_hashes(hashes)
            .await
            .unwrap()
            .into_iter()
            .map(Tree::from_mega_model)
            .collect())
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
        let models = self
            .storage
            .mono_storage()
            .get_mega_blobs_by_hashes(hashes)
            .await?;

        let map = models
            .into_iter()
            .map(|blob| {
                (
                    blob.blob_id.clone(),
                    EntryMeta {
                        pack_id: Some(blob.pack_id.clone()),
                        pack_offset: Some(blob.pack_offset as usize),
                        file_path: Some(blob.file_path.clone()),
                        is_delta: Some(blob.is_delta_in_pack),
                        // TODO: Populate `crc32` once mono blob metadata exposes it.
                        // For now we set it to `None` because `get_mega_blobs_by_hashes` does
                        // not provide CRC32 information for monorepo-backed packs.
                        crc32: None,
                    },
                )
            })
            .collect::<HashMap<String, EntryMeta>>();

        Ok(map)
    }

    async fn update_refs(&self, refs: &RefCommand) -> Result<(), GitError> {
        if refs.ref_type == RefTypeEnum::Tag {
            // MonoRepo product rule: tags are Web/API-only (docs/monorepo.md §2).
            // ImportRepo keeps client tag push; never silently write refs/tags/* here.
            Err(GitError::CustomError(
                "MonoRepo rejects Git-client tag create/update/delete; use Web UI or /tags API"
                    .to_string(),
            ))
        } else {
            self.apply_cl_mega_ref_for_push_command(refs, None)
                .await
                .map_err(GitError::from)
        }
    }

    async fn check_commit_exist(&self, hash: &str) -> bool {
        self.storage
            .mono_storage()
            .get_commit_by_hash(hash)
            .await
            .ok()
            .flatten()
            .is_some()
    }

    async fn check_object_exist(&self, hash: &str) -> bool {
        // Git-client tag pushes are rejected before this check on MonoRepo.
        // A commit check is retained as the fail-closed fallback for future
        // callers that need a non-tag object existence probe.
        self.check_commit_exist(hash).await
    }

    async fn check_default_branch(&self) -> bool {
        true
    }

    async fn traverses_tree_and_update_filepath(&self) -> Result<(), MegaError> {
        // File indexing follows the push tip's tree; the tip is resolved from
        // the ref command (GC-MC-13), same as `ImportRepo`'s explicit tip
        // query.
        let Some(cmd) = self.primary_branch_command() else {
            tracing::info!("Skipping file path update: no branch update in this push.");
            return Ok(());
        };
        let Some(chain) = self.build_push_chain(&cmd).await? else {
            // ADR-MC-05 no-op: nothing new to index.
            return Ok(());
        };
        let tip = chain.tip;

        let tree_hashes = vec![tip.tree_id.to_string()];
        let trees = self
            .storage
            .mono_storage()
            .get_trees_by_hashes(tree_hashes)
            .await
            .map_err(|e| {
                MegaError::Other(format!(
                    "Failed to retrieve root tree for commit {}: {}",
                    tip.id, e
                ))
            })?;

        if trees.is_empty() {
            return Err(MegaError::Other(format!(
                "Root tree {} not found for commit {}",
                tip.tree_id, tip.id
            )));
        }

        let root_tree = Tree::from_mega_model(trees[0].clone());

        tracing::info!(
            "Starting file path update for commit {} with root tree {}",
            tip.id,
            tip.tree_id
        );

        let storage = self.storage.mono_storage();
        let pairs = collect_mega_blob_filepaths(storage.clone(), root_tree, PathBuf::new())
            .await
            .map_err(|e| {
                MegaError::Other(format!(
                    "Failed to update file paths for commit {}: {}",
                    tip.id, e
                ))
            })?;
        storage.update_blob_filepaths(pairs).await.map_err(|e| {
            MegaError::Other(format!(
                "Failed to update file paths for commit {}: {}",
                tip.id, e
            ))
        })?;

        tracing::info!(
            "Successfully completed file path update for commit {}",
            tip.id
        );

        Ok(())
    }
}

#[async_recursion]
pub(crate) async fn collect_mega_blob_filepaths(
    storage: MonoStorage,
    tree: Tree,
    path: PathBuf,
) -> Result<Vec<(String, String)>, MegaError> {
    let mut pairs = Vec::new();
    for item in tree.tree_items {
        let item_path = path.join(&item.name);

        if item.is_tree() {
            let tree_hash = item.id.to_string();
            let trees = storage
                .get_trees_by_hashes(vec![tree_hash.clone()])
                .await
                .map_err(|e| {
                    MegaError::Other(format!(
                        "Failed to retrieve tree {} at path '{}': {}",
                        tree_hash,
                        item_path.display(),
                        e
                    ))
                })?;

            if trees.is_empty() {
                return Err(MegaError::Other(format!(
                    "Tree {} not found at path '{}'",
                    tree_hash,
                    item_path.display()
                )));
            }

            let child_tree = Tree::from_mega_model(trees[0].clone());
            pairs.extend(
                collect_mega_blob_filepaths(storage.clone(), child_tree, item_path.clone())
                    .await
                    .map_err(|e| {
                        MegaError::Other(format!(
                            "Failed to process subtree {} at path '{}': {}",
                            tree_hash,
                            item_path.display(),
                            e
                        ))
                    })?,
            );
        } else {
            let blob_id = item.id.to_string();
            let file_path = item_path.to_str().ok_or_else(|| {
                MegaError::Other(format!(
                    "Invalid UTF-8 path for blob {}: '{}'",
                    blob_id,
                    item_path.display()
                ))
            })?;
            pairs.push((blob_id, file_path.to_owned()));
        }
    }

    Ok(pairs)
}

impl MonoRepo {
    async fn direct_object_pack(
        &self,
        want: Vec<String>,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        let pack_config = &self.storage.config().pack;
        let storage = self.storage.mono_storage();

        let tree_models = storage
            .get_trees_by_hashes(want.clone())
            .await
            .map_err(|e| GitError::CustomError(format!("tree lookup failed: {e}")))?;
        let blob_models = storage
            .get_mega_blobs_by_hashes(want)
            .await
            .map_err(|e| GitError::CustomError(format!("blob lookup failed: {e}")))?;

        let obj_num = tree_models.len() + blob_models.len();
        if obj_num == 0 {
            return Err(GitError::CustomError(
                "requested objects were not found".to_owned(),
            ));
        }

        let blob_hashes = blob_models
            .iter()
            .map(|blob| blob.blob_id.clone())
            .collect::<Vec<_>>();
        let blob_meta = blob_models
            .into_iter()
            .map(|blob| {
                (
                    blob.blob_id.clone(),
                    EntryMeta {
                        pack_id: Some(blob.pack_id.clone()),
                        pack_offset: Some(blob.pack_offset as usize),
                        file_path: Some(blob.file_path.clone()),
                        is_delta: Some(blob.is_delta_in_pack),
                        crc32: None,
                    },
                )
            })
            .collect::<HashMap<_, _>>();

        let (entry_tx, entry_rx) = mpsc::channel(pack_config.channel_message_size);
        let (stream_tx, stream_rx) = mpsc::channel(pack_config.channel_message_size);
        let encoder = PackEncoder::new(obj_num, 0, stream_tx);
        encoder
            .encode_async(entry_rx)
            .await
            .map_err(|e| GitError::CustomError(format!("pack encode failed: {e}")))?;

        for tree_model in tree_models {
            entry_tx
                .send(MetaAttached {
                    inner: Tree::from_mega_model(tree_model).into(),
                    meta: EntryMeta::new(),
                })
                .await
                .map_err(|e| GitError::CustomError(format!("pack tree entry send failed: {e}")))?;
        }

        let default_meta = EntryMeta::default();
        let blobs = self.storage.git_service.get_objects_stream(blob_hashes);
        blobs
            .try_for_each_concurrent(16, |(_, stream, _)| {
                let entry_tx = entry_tx.clone();
                let blob_meta = &blob_meta;
                let default_meta = &default_meta;
                async move {
                    let data = stream
                        .try_fold(Vec::new(), |mut acc, bytes| async move {
                            acc.extend_from_slice(&bytes);
                            Ok(acc)
                        })
                        .await?;
                    let blob = Blob::from_content_bytes(data);
                    let meta = blob_meta
                        .get(&blob.id.to_string())
                        .unwrap_or(default_meta)
                        .to_owned();
                    entry_tx
                        .send(MetaAttached {
                            inner: blob.into(),
                            meta,
                        })
                        .await
                        .map_err(|e| IoOrbitError::Other(format!("pack entry send failed: {e}")))?;

                    Ok(())
                }
            })
            .await
            .map_err(|e| GitError::CustomError(format!("blob stream failed: {e}")))?;
        drop(entry_tx);

        Ok(ReceiverStream::new(stream_rx))
    }

    /// All branch commands update CL `mega_refs` in **one** DB transaction (same idea as import’s single-txn metadata commit).
    async fn persist_mono_branch_cl_mega_refs_transaction(&self) -> Result<(), MegaError> {
        let cmds = self
            .command_list
            .lock()
            .expect("command_list lock poisoned")
            .clone();
        let txn = self.storage.begin_db_transaction().await?;
        for cmd in &cmds {
            if cmd.ref_type == RefTypeEnum::Branch
                && let Err(error) = self
                    .apply_cl_mega_ref_for_push_command(cmd, Some(&txn))
                    .await
            {
                let _ = txn.rollback().await;
                return Err(error);
            }
        }
        txn.commit().await.map_err(MegaError::Db)?;
        // UN-16: receive-pack Delete branch hook (post-commit). A branch
        // delete cannot touch main (rejected above), so the old/new blob IDs
        // from main's tree are equal and the notify is a no-op — but the hook
        // point is wired so any future path that removes main's
        // `/.mega_cedar.json` marks the shared snapshot dirty (fail-closed).
        if cmds.iter().any(|cmd| {
            cmd.ref_type == RefTypeEnum::Branch
                && (cmd.command_type == CommandType::Delete || cmd.new_id == ZERO_ID)
        }) {
            let storage = self.storage.mono_storage();
            // The ref deletions are already committed, so a failure while
            // reading main's tree leaves the snapshot stale: mark dirty
            // (fail-closed) before propagating.
            let blob_id = match async {
                let root = storage.get_main_ref("/").await?;
                let tree = match root {
                    Some(r) => storage.get_tree_by_hash(&r.ref_tree_hash).await?,
                    None => None,
                };
                Ok::<_, MegaError>(tree.and_then(|t| authz_blob_id(&Tree::from_mega_model(t))))
            }
            .await
            {
                Ok(blob_id) => blob_id,
                Err(e) => {
                    self.storage.entity_store().mark_dirty();
                    return Err(e);
                }
            };
            notify_authz_changed_best_effort(&self.storage, blob_id.as_deref(), blob_id.as_deref())
                .await;
        }
        Ok(())
    }

    async fn apply_cl_mega_ref_for_push_command(
        &self,
        cmd: &RefCommand,
        txn: Option<&DatabaseTransaction>,
    ) -> Result<(), MegaError> {
        let storage = self.storage.mono_storage();
        if cmd.command_type == CommandType::Delete || cmd.new_id == ZERO_ID {
            // UN-16: reject deleting the main branch ref. The shared authz
            // snapshot is keyed on main's `/.mega_cedar.json`; deleting main
            // would leave the snapshot without a source of truth. This is a
            // deliberate safety closure (not an enforcement gate), surfaced to
            // git clients as an actionable error.
            if cmd.ref_name == MEGA_BRANCH_NAME {
                return Err(MegaError::Other(format!(
                    "refusing to delete the main branch ref `{MEGA_BRANCH_NAME}`: \
                     the authorization snapshot is keyed on main's `/.mega_cedar.json`; \
                     use the Web UI / API to manage the default branch"
                )));
            }
            let path = self
                .path
                .to_str()
                .ok_or_else(|| MegaError::Other("monorepo path is not valid UTF-8".to_owned()))?;
            let deleted = match txn {
                Some(t) => {
                    storage
                        .remove_ref_if_unchanged(path, &cmd.ref_name, &cmd.old_id, t)
                        .await?
                }
                None => {
                    storage
                        .remove_ref_if_unchanged(
                            path,
                            &cmd.ref_name,
                            &cmd.old_id,
                            storage.get_connection(),
                        )
                        .await?
                }
            };
            if !deleted {
                return Err(MegaError::Other(format!(
                    "ref {} moved since advertisement (expected {})",
                    cmd.ref_name, cmd.old_id
                )));
            }
            return Ok(());
        }

        // ADR-MC-05: empty pack / already-known `new_id` is an explicit no-op
        // — CL ref and CL stay untouched.
        let Some(chain) = self.build_push_chain(cmd).await? else {
            return Ok(());
        };
        let cl_link = self.fetch_or_new_cl_link(&chain.base).await?;
        let ref_name = utils::cl_ref_name(&cl_link);

        let existing = match txn {
            Some(t) => storage.get_ref_by_name_in_txn(&ref_name, t).await?,
            None => storage.get_ref_by_name(&ref_name).await?,
        };

        // `ref_commit_hash` and `ref_tree_hash` come from the same tip commit
        // (`cmd.new_id` and its tree).
        if let Some(mut cl_ref) = existing {
            cl_ref.ref_commit_hash = chain.tip.id.to_string();
            cl_ref.ref_tree_hash = chain.tip.tree_id.to_string();
            storage.update_ref(cl_ref, txn).await?;
        } else {
            let new_ref = mega_refs::Model::new(
                &self.path,
                ref_name,
                chain.tip.id.to_string(),
                chain.tip.tree_id.to_string(),
                true,
            )?;
            storage.save_refs(new_ref, txn).await?;
        }
        Ok(())
    }

    /// CL / conversations / build / code-review hooks after branch `mega_refs` are committed.
    async fn run_mono_post_push_pipeline(&self) -> Result<(), MegaError> {
        let Some(cmd) = self.primary_branch_command() else {
            return Ok(());
        };
        // ADR-MC-05 no-op: CL / build / reanchor hooks stay untouched.
        let Some(chain) = self.build_push_chain(&cmd).await? else {
            return Ok(());
        };
        let from_hash = chain.base.clone();
        let to_hash = chain.tip.id.to_string();
        let username = self.username();
        let mono_api_service = self.into();
        let editor = OnpushCodeEdit::from(
            self.path.to_str().unwrap(),
            &self.base_branch,
            &from_hash,
            &mono_api_service,
        );
        let cl = editor
            .update_or_create_cl(&self.storage, &from_hash, &to_hash, &username)
            .await?;
        // MC-04: the commit listing is rebuilt in the CL update's own
        // transaction (code_edit/model.rs). A CL whose listing is missing or
        // does not cover its current `to_hash` — legacy CLs, or intermediate
        // states from before this invariant — gets the listing rebuilt
        // idempotently here, without touching the CL body (ADR-MC-05 R2: this
        // is not "creating or changing a CL"). The rebuild is conditional on
        // the CL still sitting at the probed `(from_hash, to_hash)` — a
        // concurrent newer push abandons it rather than being overwritten by a
        // stale listing (Codex MC-04 R1 P1-2).
        let cl_stg = self.storage.cl_storage();
        if !cl_stg.cl_commits_contain(&cl.link, &cl.to_hash).await? {
            let chain = collect_cl_chain(&self.storage, &cl.from_hash, &cl.to_hash).await?;
            cl_stg
                .rebuild_cl_commits_if_current(&cl.link, &cl.from_hash, &cl.to_hash, &chain)
                .await?;
        }
        self.traverses_tree_and_update_filepath().await?;
        if self.bellatrix.enable_build() {
            editor
                .trigger_build_and_check(
                    self.storage.clone(),
                    self.git_object_cache.clone(),
                    self.bellatrix.clone(),
                    &cl,
                    &username,
                )
                .await?;
        }
        self.reanchor_code_review_threads(&cl, &to_hash).await?;
        // Codex R1 P1-2 / R2 P1-2: commit bindings are written only now — the
        // push has been accepted (chain validated, CL updated) — and cover
        // exactly the accepted chain's *newly introduced* commits: a rejected
        // push leaves `commit_auths` untouched, and a no-new-content push
        // re-binds nothing (the commits keep their original binding).
        let new_ids = self
            .new_commit_ids
            .lock()
            .expect("new_commit_ids lock poisoned")
            .clone();
        let bindings: Vec<(String, String)> = chain
            .ordered_commits
            .iter()
            .filter(|c| new_ids.contains(&c.id.to_string()))
            .map(|c| (c.id.to_string(), c.author.email.clone()))
            .collect();
        if !bindings.is_empty() {
            self.storage
                .mono_storage()
                .process_commit_bindings(&bindings, self.username.clone().as_deref())
                .await?;
        }
        Ok(())
    }

    /// The semantics-defining command of this push: the first non-delete
    /// branch command. Delete commands never build a chain; a push with more
    /// than one non-delete branch command is rejected by
    /// [`Self::validate_incoming_push`] before this is ever consulted at
    /// finalize (ADR-MC-04).
    fn primary_branch_command(&self) -> Option<RefCommand> {
        push_chain::primary_branch_command(
            &self
                .command_list
                .lock()
                .expect("command_list lock poisoned"),
        )
    }

    /// Receive-pack admission gate (MC-06), run before any ref/CL mutation:
    ///
    /// 1. ADR-MC-04 — a receive-pack carrying more than one non-delete branch
    ///    command is rejected as a whole; the message tells the user to push
    ///    one branch at a time. Delete commands do not count.
    /// 2. MC-03 chain validation — the primary branch command's resolved
    ///    [`PushChain`] is validated against storage, with the path's existing
    ///    open CL (queried on the same path as `fetch_or_new_cl_link` /
    ///    `update_or_create_cl`) supplying the ADR-MC-07 cumulative boundary.
    ///
    /// A failure aborts `finalize_receive_pack`; the protocol layer marks every
    /// branch command `ng` with this message, so the client rejects the whole
    /// push. The ADR-MC-05 no-op path and delete-only pushes pass through.
    async fn validate_incoming_push(&self) -> Result<(), MegaError> {
        let cmds = self
            .command_list
            .lock()
            .expect("command_list lock poisoned")
            .clone();
        let branch_updates = cmds
            .iter()
            .filter(|c| {
                c.ref_type == RefTypeEnum::Branch
                    && c.command_type != CommandType::Delete
                    && c.new_id != ZERO_ID
            })
            .count();
        if branch_updates > 1 {
            return Err(MegaError::Other(format!(
                "monorepo receive-pack accepts at most one branch update per push \
                 (got {branch_updates}); push one branch at a time \
                 (delete commands are unaffected)"
            )));
        }
        let Some(cmd) = push_chain::primary_branch_command(&cmds) else {
            return Ok(());
        };
        // ADR-MC-05 no-op: nothing to validate.
        let Some(chain) = self.build_push_chain(&cmd).await? else {
            return Ok(());
        };
        let open_cl = self
            .storage
            .cl_storage()
            .get_open_cl_by_path(self.path.to_str().unwrap(), &self.username())
            .await?;
        chain
            .validate(&cmd, &self.storage.mono_storage(), open_cl.as_ref())
            .await
    }

    /// Build the [`PushChain`] for a branch command, cached per `new_id` so a
    /// finalize builds each chain at most once (MC01-R1 P2-2). `None` means
    /// the ADR-MC-05 no-op (empty pack / known `new_id`) — callers must leave
    /// CL refs and the CL untouched; the notice is logged and stored on the
    /// first (cache-miss) build only.
    async fn build_push_chain(&self, cmd: &RefCommand) -> Result<Option<PushChain>, MegaError> {
        if let Some(cached) = self
            .push_chain_cache
            .lock()
            .expect("push_chain_cache lock poisoned")
            .get(&cmd.new_id)
        {
            return Ok(cached.clone());
        }
        let pack_commit_ids = self
            .pack_commit_ids
            .lock()
            .expect("pack_commit_ids lock poisoned")
            .clone();
        let new_commit_ids = self
            .new_commit_ids
            .lock()
            .expect("new_commit_ids lock poisoned")
            .clone();
        let tip_commit = self
            .storage
            .mono_storage()
            .get_commit_by_hash(&cmd.new_id)
            .await?
            .map(Commit::from_mega_model);
        let chain = match PushChain::resolve(
            cmd,
            &pack_commit_ids,
            &new_commit_ids,
            tip_commit,
            &self.storage.mono_storage(),
        )
        .await?
        {
            PushChainResolution::Chain(chain) => Some(*chain),
            PushChainResolution::Noop { notice } => {
                // GC-MC-14: the no-op is logged, not silent; the notice also
                // reaches the git client as a `remote:` line (sideband
                // channel 2, see `receive_pack_notice`).
                tracing::info!(ref_name = %cmd.ref_name, new_id = %cmd.new_id, "{notice}");
                *self
                    .no_op_notice
                    .lock()
                    .expect("no_op_notice lock poisoned") = Some(notice);
                None
            }
        };
        self.push_chain_cache
            .lock()
            .expect("push_chain_cache lock poisoned")
            .insert(cmd.new_id.clone(), chain.clone());
        Ok(chain)
    }

    async fn fetch_or_new_cl_link(&self, from_hash: &str) -> Result<String, MegaError> {
        let storage = self.storage.cl_storage();
        let path_str = self.path.to_str().unwrap();
        let cl_link = match storage
            .get_open_cl_by_path(path_str, &self.username())
            .await?
        {
            Some(cl) => cl.link.clone(),
            None => {
                if from_hash == ZERO_ID {
                    return Err(MegaError::Other(
                        "Can not init directory under monorepo directory!".to_string(),
                    ));
                }
                generate_link()
            }
        };
        let mut lock = self.cl_link.write().await;
        *lock = Some(cl_link.clone());
        Ok(cl_link)
    }

    pub fn username(&self) -> String {
        self.username.clone().unwrap_or(String::from("Anonymous"))
    }

    pub async fn get_commit_blobs(
        &self,
        commit_hash: &str,
    ) -> Result<Vec<(PathBuf, ObjectHash)>, MegaError> {
        let api_service: MonoApiService = self.into();
        api_service.get_commit_blobs(commit_hash).await
    }

    pub async fn cl_files_list(
        &self,
        old_files: Vec<(PathBuf, ObjectHash)>,
        new_files: Vec<(PathBuf, ObjectHash)>,
    ) -> Result<Vec<ClDiffFile>, MegaError> {
        let api_service: MonoApiService = self.into();
        api_service.cl_files_list(old_files, new_files).await
    }

    // Mark code review threads whose anchors may be affected by this change as outdated.
    // These threads will require reanchoring to restore accurate code positions.
    // `to_hash` is the push chain tip (CL `to_hash`).
    pub async fn reanchor_code_review_threads(
        &self,
        cl: &mega_cl::Model,
        to_hash: &str,
    ) -> Result<(), MegaError> {
        let mono_api_service: MonoApiService = self.into();
        let cl_link = cl.link.clone();

        // Marks code review threads as outdated if their file paths
        // are affected by the latest change list.
        let changed_files = get_changed_files(&mono_api_service, cl).await?;
        let files_with_threads = self
            .storage
            .code_review_thread_storage()
            .get_files_with_threads_by_link(&cl_link)
            .await?;

        let files_with_threads_set: HashSet<&String> = files_with_threads.iter().collect();

        // Intersection: files that are changed AND have threads
        let affected_files: Vec<String> = changed_files
            .into_iter()
            .filter(|file| files_with_threads_set.contains(file))
            .collect();

        tracing::info!(
            "Reanchor code review thread in cl_link: {}, affected files: {:?}",
            cl_link,
            affected_files
        );

        let pending_reanchor_threads = self
            .storage
            .code_review_thread_storage()
            .find_threads_by_file_paths(affected_files)
            .await?;

        let pending_reanchor_thread_ids: Vec<i64> = pending_reanchor_threads
            .iter()
            .map(|thread| thread.id)
            .collect();

        // Mark as PendingReanchor
        self.storage
            .code_review_thread_storage()
            .mark_positions_status_by_thread_ids(
                &pending_reanchor_thread_ids,
                PositionStatusEnum::PendingReanchor,
            )
            .await?;

        // Start reanchor
        let anchors = self
            .storage
            .code_review_thread_storage()
            .get_anchors_by_thread_ids(&pending_reanchor_thread_ids)
            .await?;

        let mono_api_service = Arc::new(mono_api_service);
        let mut anchors_map: HashMap<i64, Vec<mega_code_review_anchor::Model>> = HashMap::new();
        for anchor in anchors {
            anchors_map
                .entry(anchor.thread_id)
                .or_default()
                .push(anchor);
        }

        let reanchor_tasks: Vec<_> = pending_reanchor_threads
            .into_iter()
            .map(|thread| {
                let cl_link = cl_link.clone();
                let mono_api_service = Arc::clone(&mono_api_service);
                let anchors_map = anchors_map.clone();
                let to_hash = to_hash.to_owned();

                async move {
                    let thread_id = thread.id;

                    let thread_anchors = match anchors_map.get(&thread_id) {
                        Some(anchors) => anchors,
                        None => {
                            tracing::warn!("Thread {} has no anchors", thread_id);
                            return Err(MegaError::Other(format!(
                                "Thread {} has no anchors",
                                thread_id
                            )));
                        }
                    };

                    let (diff_content, _) = mono_api_service
                        .paged_content_diff(&cl_link, Pagination::default())
                        .await?;

                    let mut blob_cache: HashMap<String, String> = HashMap::new();

                    for anchor in thread_anchors {
                        let file_path = anchor.file_path.clone();

                        // Fetch blob once per file
                        let latest_blob = if let Some(blob) = blob_cache.get(&file_path) {
                            blob.clone()
                        } else {
                            let blob = mono_api_service
                                .get_blob_as_string(PathBuf::from(&file_path), Some(&to_hash))
                                .await?
                                .expect("latest blob must exist");

                            blob_cache.insert(file_path.clone(), blob.clone());
                            blob
                        };

                        // Reanchor
                        if let Err(e) = self
                            .storage
                            .code_review_service
                            .reanchor_thread(
                                anchor,
                                Some(latest_blob),
                                diff_content.clone(),
                                &to_hash,
                            )
                            .await
                        {
                            tracing::error!("Reanchor failed for anchor {}: {:?}", anchor.id, e);
                        }
                    }

                    Ok(())
                }
            })
            .collect::<Vec<_>>();

        let results: Vec<Result<(), MegaError>> = stream::iter(reanchor_tasks)
            .buffer_unordered(self.storage.get_recommended_batch_concurrency())
            .collect()
            .await;

        for res in results {
            if let Err(e) = res {
                tracing::error!("Reanchor task failed: {:?}", e);
            }
        }

        Ok(())
    }
}

// Codex R1 P1-2: commit bindings (`commit_auths`) are written only after a
// push has been accepted, and only for the accepted chain's commits. These
// tests pin the two named rejection paths (multi-branch refusal, ref target
// missing from the pack) plus the unpack stage itself.
#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
        str::FromStr,
        sync::{Arc, Mutex},
    };

    use git_internal::{
        hash::ObjectHash,
        internal::{
            metadata::{EntryMeta, MetaAttached},
            object::{
                blob::Blob,
                commit::Commit,
                signature::{Signature, SignatureType},
                tree::{Tree, TreeItem, TreeItemMode},
            },
            pack::entry::Entry,
        },
    };
    use sea_orm::{EntityTrait, IntoActiveModel, PaginatorTrait, TransactionTrait};
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    use super::{MonoRepo, RepoHandler, collect_mega_blob_filepaths};
    use crate::{
        bellatrix::Bellatrix,
        callisto::{commit_auths, mega_commit, mega_tree},
        ceres::{api_service::cache::GitObjectCache, protocol::import_refs::RefCommand},
        common::utils::ZERO_ID,
        jupiter::{
            storage::{Storage, base_storage::StorageConnector},
            tests::test_storage,
            utils::converter::{FromMegaModel, IntoMegaModel},
        },
    };

    fn test_commit(message: &str) -> Commit {
        let tree_id = ObjectHash::from_str("27dd8d4cf39f3868c6eee38b601bc9e9939304f5").unwrap();
        let parent = ObjectHash::from_str("119bc457cb05b52dfb0d6b14f66d9a8a52d09e25").unwrap();
        Commit::new(
            Signature::new(
                SignatureType::Author,
                "Monoengine Test".to_string(),
                "monoengine-test@example.invalid".to_string(),
            ),
            Signature::new(
                SignatureType::Committer,
                "Monoengine Test".to_string(),
                "monoengine-test@example.invalid".to_string(),
            ),
            tree_id,
            vec![parent],
            message,
        )
    }

    fn id_set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn collect_mega_blob_filepaths_preserves_nested_tree_order() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        let first = Blob::from_content("first");
        let second = Blob::from_content("second");
        let nested = Tree::from_tree_items(vec![
            TreeItem {
                mode: TreeItemMode::Blob,
                id: first.id,
                name: "文件.txt".to_owned(),
            },
            TreeItem {
                mode: TreeItemMode::Blob,
                id: second.id,
                name: "second.txt".to_owned(),
            },
        ])
        .expect("build nested tree");
        let nested_model = nested
            .clone()
            .into_mega_model(EntryMeta::default())
            .expect("test ID generator initialized");
        mega_tree::Entity::insert(nested_model.into_active_model())
            .exec(storage.mono_storage().get_connection())
            .await
            .expect("insert nested tree");
        let root = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Tree,
            id: nested.id,
            name: "src".to_owned(),
        }])
        .expect("build root tree");

        let pairs = collect_mega_blob_filepaths(storage.mono_storage(), root, PathBuf::new())
            .await
            .expect("collect nested paths");

        assert_eq!(
            pairs,
            vec![
                (first.id.to_string(), "src/文件.txt".to_owned()),
                (second.id.to_string(), "src/second.txt".to_owned()),
            ]
        );
    }

    fn test_monorepo(
        storage: &Storage,
        commands: Vec<RefCommand>,
        pack_commit_ids: HashSet<String>,
        new_commit_ids: HashSet<String>,
    ) -> MonoRepo {
        // Never connected: the gate paths under test do not touch the cache.
        let connection = ::redis::aio::ConnectionManager::new_lazy_with_config(
            ::redis::Client::open("redis://127.0.0.1:6379").expect("redis client"),
            ::redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("lazy connection manager");
        MonoRepo {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection,
                prefix: "mc06-test".to_string(),
            }),
            path: PathBuf::from("/"),
            base_branch: "main".to_string(),
            pack_commit_ids: Mutex::new(pack_commit_ids),
            new_commit_ids: Mutex::new(new_commit_ids),
            no_op_notice: Mutex::new(None),
            push_chain_cache: Mutex::new(HashMap::new()),
            cl_link: Arc::new(RwLock::new(None)),
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
            username: Some("tester".to_string()),
            command_list: Mutex::new(commands),
        }
    }

    async fn commit_auth_count(storage: &Storage) -> u64 {
        commit_auths::Entity::find()
            .count(storage.mono_storage().get_connection())
            .await
            .expect("count commit_auths")
    }

    #[tokio::test]
    async fn multi_branch_rejection_never_binds_commits() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        let old = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string();
        let t1 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string();
        let t2 = "cccccccccccccccccccccccccccccccccccccccc".to_string();
        let commands = vec![
            RefCommand::new(old.clone(), t1.clone(), "refs/heads/one".to_string()),
            RefCommand::new(old.clone(), t2.clone(), "refs/heads/two".to_string()),
        ];
        let repo = test_monorepo(&storage, commands, id_set(&[&t1]), id_set(&[&t1]));

        let err = repo
            .validate_incoming_push()
            .await
            .expect_err("a two-branch receive-pack must be rejected");

        assert!(
            err.to_string()
                .contains("at most one branch update per push"),
            "{err}"
        );
        assert_eq!(
            commit_auth_count(&storage).await,
            0,
            "a rejected push must leave commit_auths untouched"
        );
    }

    #[tokio::test]
    async fn missing_ref_target_rejection_never_binds_commits() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        // The ref target exists in storage but the pack carries only an
        // unrelated commit — resolve fail-closes on "tip not in pack".
        let tip = test_commit("the ref update target");
        let other = test_commit("unrelated pack content");
        let model: mega_commit::Model = tip
            .clone()
            .into_mega_model(EntryMeta::default())
            .expect("test ID generator initialized");
        mega_commit::Entity::insert(model.into_active_model())
            .exec(storage.mono_storage().get_connection())
            .await
            .expect("insert tip commit");
        let tip_id = tip.id.to_string();
        let commands = vec![RefCommand::new(
            ZERO_ID.to_string(),
            tip_id.clone(),
            "refs/heads/main".to_string(),
        )];
        let repo = test_monorepo(
            &storage,
            commands,
            id_set(&[&other.id.to_string()]),
            id_set(&[&other.id.to_string()]),
        );

        let err = repo
            .validate_incoming_push()
            .await
            .expect_err("a pack missing the ref update target must be rejected");

        assert!(
            err.to_string()
                .contains("does not contain the ref update target"),
            "{err}"
        );
        assert_eq!(
            commit_auth_count(&storage).await,
            0,
            "a rejected push must leave commit_auths untouched"
        );
    }

    #[tokio::test]
    async fn save_entry_does_not_bind_commits() {
        let temp = TempDir::new().expect("temp dir");
        let mut storage = test_storage(temp.path()).await;
        // `test_storage` hands out a mock MonoService on a disconnected
        // connection; re-point it at the test database. The mock GitService is
        // fine: a commit-only entry never touches object storage.
        storage.mono_service = crate::jupiter::service::mono_service::MonoService {
            mono_storage: storage.mono_storage(),
            git_service: storage.git_service.clone(),
        };
        let tip = test_commit("unpacked but not yet accepted");
        let tip_id = tip.id.to_string();
        let commands = vec![RefCommand::new(
            ZERO_ID.to_string(),
            tip_id.clone(),
            "refs/heads/main".to_string(),
        )];
        let repo = test_monorepo(&storage, commands, id_set(&[&tip_id]), id_set(&[&tip_id]));

        let entry: Entry = tip.into();
        repo.save_entry(vec![MetaAttached {
            inner: entry,
            meta: EntryMeta::new(),
        }])
        .await
        .expect("save_entry must persist the commit");

        assert!(
            storage
                .mono_storage()
                .get_commit_by_hash(&tip_id)
                .await
                .expect("lookup")
                .is_some(),
            "save_entry must have persisted the commit row"
        );
        assert_eq!(
            commit_auth_count(&storage).await,
            0,
            "unpack/save_entry must not bind commits (Codex R1 P1-2)"
        );
    }

    // --- MC-04: pipeline backfill of a missing/stale CL commit listing ---

    /// The well-known git empty-tree id: the seeded chain shares it, so the
    /// pipeline's file walk is a no-op.
    const MC04_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn mc04_sha(n: u64) -> String {
        format!("{n:040x}")
    }

    fn mc04_commit_row(n: u64, parents: &[u64]) -> mega_commit::Model {
        mega_commit::Model {
            id: crate::callisto::entity_ext::generate_id().expect("test ID generator initialized"),
            commit_id: mc04_sha(n),
            tree: MC04_TREE.to_string(),
            parents_id: serde_json::json!(parents.iter().map(|p| mc04_sha(*p)).collect::<Vec<_>>()),
            author: Some("author Test User <mc04@example.invalid> 1750000000 +0000".to_string()),
            committer: Some(
                "committer Test User <mc04@example.invalid> 1750000000 +0000".to_string(),
            ),
            content: Some(format!("mc04 message {n}")),
            created_at: chrono::Utc::now().naive_utc(),
            pack_id: String::new(),
            pack_offset: 0,
        }
    }

    /// MC-04 AC⑥: a CL whose listing does not cover its current `to_hash`
    /// (missing or stale — legacy rows, or intermediate states from before the
    /// transactional invariant) gets the listing rebuilt by the post-push
    /// pipeline over the full frozen `(from_hash, to_hash]` chain, with the CL
    /// body untouched.
    #[tokio::test]
    async fn post_push_pipeline_backfills_missing_cl_commits() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        let mono_storage = storage.mono_storage();
        // Seed the shared empty tree and the chain t0 ← t1 ← t2.
        mega_tree::Entity::insert(
            mega_tree::Model {
                id: crate::callisto::entity_ext::generate_id()
                    .expect("test ID generator initialized"),
                tree_id: MC04_TREE.to_string(),
                sub_trees: Vec::new(),
                size: 0,
                created_at: chrono::Utc::now().naive_utc(),
                pack_id: String::new(),
                pack_offset: 0,
                commit_id: String::new(),
            }
            .into_active_model(),
        )
        .exec(mono_storage.get_connection())
        .await
        .expect("insert empty tree");
        mega_commit::Entity::insert_many(
            vec![
                mc04_commit_row(700, &[]),
                mc04_commit_row(701, &[700]),
                mc04_commit_row(702, &[701]),
            ]
            .into_iter()
            .map(|m| m.into_active_model())
            .collect::<Vec<_>>(),
        )
        .exec(mono_storage.get_connection())
        .await
        .expect("insert chain");
        // The open CL exists with to_hash = t2, but its listing is empty (the
        // state the backfill exists for).
        let seeded_cl = storage
            .cl_storage()
            .new_cl_model(
                "/",
                "CLMC04BP",
                "backfill probe",
                "main",
                &mc04_sha(700),
                &mc04_sha(702),
                "tester",
            )
            .await
            .expect("seed open CL");

        let commands = vec![RefCommand::new(
            mc04_sha(700),
            mc04_sha(702),
            "refs/heads/main".to_string(),
        )];
        let repo = test_monorepo(
            &storage,
            commands,
            id_set(&[&mc04_sha(701), &mc04_sha(702)]),
            id_set(&[&mc04_sha(701), &mc04_sha(702)]),
        );

        repo.run_mono_post_push_pipeline()
            .await
            .expect("pipeline must run");

        let listing = storage
            .cl_storage()
            .get_cl_commits("CLMC04BP")
            .await
            .expect("read backfilled listing");
        let shas: Vec<String> = listing.iter().map(|r| r.commit_sha.clone()).collect();
        assert_eq!(
            shas,
            vec![mc04_sha(701), mc04_sha(702)],
            "the backfilled listing must be the full (from, to] chain, oldest first"
        );
        assert_eq!(listing[0].author_email, "mc04@example.invalid");

        let cl_after = storage
            .cl_storage()
            .get_cl("CLMC04BP")
            .await
            .expect("get_cl")
            .expect("CL row exists");
        assert_eq!(cl_after.from_hash, seeded_cl.from_hash, "CL from unchanged");
        assert_eq!(cl_after.to_hash, seeded_cl.to_hash, "CL to unchanged");
        assert_eq!(cl_after.title, seeded_cl.title, "CL title unchanged");
        assert_eq!(cl_after.status, seeded_cl.status, "CL status unchanged");
        assert_eq!(
            cl_after.updated_at, seeded_cl.updated_at,
            "the backfill must not touch the CL body (updated_at unchanged)"
        );
    }

    /// MC-04 R1 P2: the other staleness shape — an old listing exists but does
    /// not cover the CL's current `to_hash` (it was built for a previous tip).
    /// The probe must call it stale and the backfill must rebuild the full
    /// chain to the current tip.
    #[tokio::test]
    async fn post_push_pipeline_backfills_stale_cl_commits() {
        let temp = TempDir::new().expect("temp dir");
        let storage = test_storage(temp.path()).await;
        let mono_storage = storage.mono_storage();
        mega_tree::Entity::insert(
            mega_tree::Model {
                id: crate::callisto::entity_ext::generate_id()
                    .expect("test ID generator initialized"),
                tree_id: MC04_TREE.to_string(),
                sub_trees: Vec::new(),
                size: 0,
                created_at: chrono::Utc::now().naive_utc(),
                pack_id: String::new(),
                pack_offset: 0,
                commit_id: String::new(),
            }
            .into_active_model(),
        )
        .exec(mono_storage.get_connection())
        .await
        .expect("insert empty tree");
        mega_commit::Entity::insert_many(
            vec![
                mc04_commit_row(800, &[]),
                mc04_commit_row(801, &[800]),
                mc04_commit_row(802, &[801]),
            ]
            .into_iter()
            .map(|m| m.into_active_model())
            .collect::<Vec<_>>(),
        )
        .exec(mono_storage.get_connection())
        .await
        .expect("insert chain");
        let seeded_cl = storage
            .cl_storage()
            .new_cl_model(
                "/",
                "CLMC04ST",
                "stale listing probe",
                "main",
                &mc04_sha(800),
                &mc04_sha(802),
                "tester",
            )
            .await
            .expect("seed open CL");
        // Stage the stale listing: built for the previous tip t1, so it does
        // not contain the current to_hash t2.
        let cl_stg = storage.cl_storage();
        let stale_chain = [Commit::from_mega_model(mc04_commit_row(801, &[800]))];
        let txn = mono_storage
            .get_connection()
            .begin()
            .await
            .expect("begin txn");
        cl_stg
            .save_cl_commits_in_txn("CLMC04ST", &stale_chain, &txn)
            .await
            .expect("stage stale listing");
        txn.commit().await.expect("commit staging txn");

        let commands = vec![RefCommand::new(
            mc04_sha(800),
            mc04_sha(802),
            "refs/heads/main".to_string(),
        )];
        let repo = test_monorepo(
            &storage,
            commands,
            id_set(&[&mc04_sha(801), &mc04_sha(802)]),
            id_set(&[&mc04_sha(801), &mc04_sha(802)]),
        );

        repo.run_mono_post_push_pipeline()
            .await
            .expect("pipeline must run");

        let listing = cl_stg
            .get_cl_commits("CLMC04ST")
            .await
            .expect("read backfilled listing");
        let shas: Vec<String> = listing.iter().map(|r| r.commit_sha.clone()).collect();
        assert_eq!(
            shas,
            vec![mc04_sha(801), mc04_sha(802)],
            "a stale listing (previous tip only) must be rebuilt to the full current chain"
        );

        let cl_after = cl_stg
            .get_cl("CLMC04ST")
            .await
            .expect("get_cl")
            .expect("CL row exists");
        assert_eq!(cl_after.from_hash, seeded_cl.from_hash);
        assert_eq!(cl_after.to_hash, seeded_cl.to_hash);
        assert_eq!(
            cl_after.updated_at, seeded_cl.updated_at,
            "the backfill must not touch the CL body"
        );
    }
}
