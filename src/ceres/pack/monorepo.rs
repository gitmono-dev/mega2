use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

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
        sea_orm_active_enums::{PositionStatusEnum, PushQueueKindEnum, RefTypeEnum},
    },
    ceres::{
        api_service::{ApiHandler, cache::GitObjectCache, mono_api_service::MonoApiService},
        code_edit::{model::collect_cl_chain, on_push::OnpushCodeEdit, utils::get_changed_files},
        merge_checker::MAX_CL_CHAIN_COMMITS,
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
    config::PushPolicy,
    contract::{
        api::common::Pagination,
        policy::notify::{
            authz_barrier_enabled, authz_blob_id, mark_authz_dirty_and_compensate,
            notify_authz_changed_best_effort,
        },
    },
    jupiter::{
        service::push_queue_service::{
            EnqueueRequest, ExecuteOutcome, ExecuteRequest, PushExecContext, PushPayload,
            QueueWaitResult, push_operation_id,
        },
        storage::{Storage, blob_path_index::BlobPathIndexMode},
        utils::converter::FromMegaModel,
    },
};
#[rustfmt::skip]
use crate::orbit_api::{error::IoOrbitError, object_storage::MultiObjectByteStream};

pub struct Monorepo {
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

/// ADR-TP-18: after a squash (or any landed tip the client is not based on),
/// refusals tell the client how to align. Real `git push` without fetch+reset
/// either NFF-fails in B3 (`old_id` mismatch) or fails chain validation when
/// the client force-sends a history that does not include the squash.
fn trunk_nff_align_message(message: &str) -> String {
    const ALIGN: &str = "git fetch && git reset --hard origin/main";
    let needs_align =
        message.contains("non-fast-forward") || message.contains("push chain is broken");
    if needs_align && !message.contains(ALIGN) {
        format!("{message}; align with `{ALIGN}`")
    } else {
        message.to_owned()
    }
}

fn trunk_align_finalize_err(err: MegaError) -> MegaError {
    let raw = err.to_string();
    let aligned = trunk_nff_align_message(&raw);
    if aligned == raw {
        return err;
    }
    let inner = aligned.strip_prefix("Other error: ").unwrap_or(&aligned);
    MegaError::Other(inner.to_owned())
}

#[async_trait]
impl RepoHandler for Monorepo {
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

    /// Codex R3 P1: Monorepo binds the accepted chain's newly introduced
    /// commits in its post-push pipeline; the protocol layer must not re-upsert
    /// the tip (an empty-pack idempotent re-push or a known-tip push would
    /// otherwise clobber the existing binding).
    fn bind_tip_after_receive(&self) -> bool {
        false
    }

    async fn refs_with_head_hash(&self) -> Result<(String, Vec<Refs>), MegaError> {
        let path = self
            .path
            .to_str()
            .ok_or_else(|| MegaError::Other("repository path is not valid UTF-8".into()))?;
        let trunk = self.storage.config().monorepo.push_policy == PushPolicy::Trunk;
        let refs: Vec<Refs> = super::materialize::materialize_path_refs(&self.storage, path)
            .await?
            .into_iter()
            .filter(|r| !trunk || !r.is_cl)
            .map(Into::into)
            .collect();
        Ok(self.find_head_hash(refs))
    }

    async fn finalize_receive_pack(&self) -> Result<(), MegaError> {
        let trunk = self.storage.config().monorepo.push_policy == PushPolicy::Trunk;
        let result = async {
            self.validate_incoming_push().await?;
            if trunk {
                self.finalize_trunk_push().await
            } else {
                self.persist_mono_branch_cl_mega_refs_transaction().await?;
                self.run_mono_post_push_pipeline().await
            }
        }
        .await;
        if trunk {
            result.map_err(trunk_align_finalize_err)
        } else {
            result
        }
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
            // Monorepo product rule: tags are Web/API-only (docs/monorepo.md §2).
            // ImportRepo keeps client tag push; never silently write refs/tags/* here.
            Err(GitError::CustomError(
                "Monorepo rejects Git-client tag create/update/delete; use Web UI or /tags API"
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
            .unwrap()
            .is_some()
    }

    async fn check_default_branch(&self) -> bool {
        true
    }

    async fn traverses_tree_and_update_filepath(&self) -> Result<(), MegaError> {
        // Review morphology: sync C-segment with isolation (`indexed_push_id IS NULL`).
        let Some(cmd) = self.primary_branch_command() else {
            tracing::info!("Skipping file path update: no branch update in this push.");
            return Ok(());
        };
        let Some(chain) = self.build_push_chain(&cmd).await? else {
            // ADR-MC-05 no-op: nothing new to index.
            return Ok(());
        };
        let tip = chain.tip;
        let prefix = self.path.to_str().unwrap_or("/");

        tracing::info!(
            "Starting file path update for commit {} with root tree {}",
            tip.id,
            tip.tree_id
        );

        self.storage
            .mono_storage()
            .index_tree_blob_paths(&tip.tree_id.to_string(), prefix, BlobPathIndexMode::Review)
            .await
            .map_err(|e| {
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

impl Monorepo {
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
            if cmd.ref_type == RefTypeEnum::Branch {
                self.apply_cl_mega_ref_for_push_command(cmd, Some(&txn))
                    .await?;
            }
        }
        txn.commit().await.map_err(MegaError::Db)?;
        // UN-16 / TP-22: receive-pack Delete cannot touch main. When the
        // authz barrier is on, this queue-external hook dirty-marks and
        // compensates without a `published_version` compare. `off` keeps the
        // original equal-blob notify (a no-op unless a future path removes
        // main's `/.mega_cedar.json`).
        if cmds.iter().any(|cmd| {
            cmd.ref_type == RefTypeEnum::Branch
                && (cmd.command_type == CommandType::Delete || cmd.new_id == ZERO_ID)
        }) {
            if authz_barrier_enabled(&self.storage) {
                mark_authz_dirty_and_compensate(&self.storage).await;
            } else {
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
                notify_authz_changed_best_effort(
                    &self.storage,
                    blob_id.as_deref(),
                    blob_id.as_deref(),
                )
                .await;
            }
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
            let existing = match txn {
                Some(t) => storage.get_ref_by_name_in_txn(&cmd.ref_name, t).await?,
                None => storage.get_ref_by_name(&cmd.ref_name).await?,
            };
            if let Some(existing) = existing {
                storage.remove_ref(existing).await?;
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
            );
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
        if self.storage.config().monorepo.push_policy == PushPolicy::Trunk {
            for cmd in cmds.iter().filter(|c| c.ref_type == RefTypeEnum::Branch) {
                if cmd.ref_name != MEGA_BRANCH_NAME {
                    return Err(MegaError::Other(format!(
                        "trunk push rejects ref '{}'; the only public branch is {MEGA_BRANCH_NAME}",
                        cmd.ref_name
                    )));
                }
            }
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
            .validate(
                &cmd,
                &self.storage.mono_storage(),
                open_cl.as_ref(),
                self.chain_commit_limit(),
            )
            .await
    }

    fn chain_commit_limit(&self) -> usize {
        match self.storage.config().monorepo.push_policy {
            PushPolicy::Trunk => self.storage.config().monorepo.max_push_commits,
            PushPolicy::Review => MAX_CL_CHAIN_COMMITS,
        }
    }

    /// Build the [`PushChain`] for a branch command, cached per `new_id` so a
    /// finalize builds each chain at most once (MC01-R1 P2-2). `None` means
    /// the ADR-MC-05 no-op (empty pack / known `new_id`) — callers must leave
    /// CL refs and the CL untouched; the notice is logged and stored on the
    /// first (cache-miss) build only.
    ///
    /// Under `push_policy=trunk` (GAP-14 / TP-12) a Noop still yields a chain
    /// walked from storage so B1/B3 can persist `{commits, fork_base, n}`.
    /// Review morphology keeps the Noop short-circuit (hard constraint 8).
    pub(crate) async fn build_push_chain(
        &self,
        cmd: &RefCommand,
    ) -> Result<Option<PushChain>, MegaError> {
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
            tip_commit.clone(),
            &self.storage.mono_storage(),
            self.chain_commit_limit(),
        )
        .await?
        {
            PushChainResolution::Chain(chain) => Some(*chain),
            PushChainResolution::Noop { notice } => {
                if self.storage.config().monorepo.push_policy == PushPolicy::Trunk {
                    let tip = tip_commit.ok_or_else(|| {
                        MegaError::Other(format!(
                            "trunk Noop bridge expected a known tip for {}",
                            cmd.new_id
                        ))
                    })?;
                    Some(
                        PushChain::from_known_tip(
                            cmd,
                            tip,
                            &self.storage.mono_storage(),
                            self.chain_commit_limit(),
                        )
                        .await?,
                    )
                } else {
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

    /// Trunk morphology: enqueue `kind=push` and run B3. Does not write
    /// `refs/cl/*` or run the CL post-push pipeline. Requester is the protocol
    /// actor (`None` under `push_auth=none`), never [`Self::username`]'s
    /// `"Anonymous"` default.
    async fn finalize_trunk_push(&self) -> Result<(), MegaError> {
        let cmds = self
            .command_list
            .lock()
            .expect("command_list lock poisoned")
            .clone();
        if cmds.iter().any(|c| {
            c.ref_type == RefTypeEnum::Branch
                && (c.command_type == CommandType::Delete || c.new_id == ZERO_ID)
        }) {
            return Err(MegaError::Other(
                "trunk push rejects delete commands; remove content via a parent-path commit"
                    .into(),
            ));
        }
        let Some(cmd) = self.primary_branch_command() else {
            return Ok(());
        };
        let Some(chain) = self.build_push_chain(&cmd).await? else {
            return Err(MegaError::Other(
                "trunk receive-pack expected a push chain (GAP-14 Noop bridge)".into(),
            ));
        };
        let payload = PushPayload::from_chain(&cmd.old_id, &cmd.new_id, &chain);
        let path = self
            .path
            .to_str()
            .ok_or_else(|| MegaError::Other("repository path is not valid UTF-8".into()))?;
        let wait = self
            .storage
            .push_queue_service
            .enqueue_and_wait(EnqueueRequest {
                kind: PushQueueKindEnum::Push,
                operation_id: push_operation_id(&cmd.old_id, &cmd.new_id),
                path: path.to_owned(),
                old_id: cmd.old_id.clone(),
                new_id: cmd.new_id.clone(),
                requester: self.username.clone(),
                payload: payload.to_json(),
                ref_name: Some(cmd.ref_name.clone()),
                is_delete: false,
            })
            .await?;
        let landed = self.follow_push_queue(wait).await?;
        if payload.n > 1 {
            *self
                .no_op_notice
                .lock()
                .expect("no_op_notice lock poisoned") = Some(format!(
                "trunk squash landed as {landed}; git fetch && git reset --hard origin/main"
            ));
        }
        Ok(())
    }

    async fn follow_push_queue(&self, mut wait: QueueWaitResult) -> Result<String, MegaError> {
        const MAX_ROUNDS: usize = 32;
        let ctx = PushExecContext {
            storage: self.storage.clone(),
            git_object_cache: self.git_object_cache.clone(),
        };
        for _ in 0..MAX_ROUNDS {
            match wait {
                QueueWaitResult::Replayed {
                    landed_commit_id, ..
                } => {
                    return landed_commit_id.ok_or_else(|| {
                        MegaError::Other("push replay missing landed_commit_id".into())
                    });
                }
                QueueWaitResult::Abandoned { id } => {
                    return Err(MegaError::Other(format!(
                        "push wait abandoned for push_queue id {id}"
                    )));
                }
                QueueWaitResult::Rejected { id, message } => {
                    return Err(MegaError::Other(format!(
                        "push rejected for push_queue id {id}: {}",
                        trunk_nff_align_message(&message)
                    )));
                }
                QueueWaitResult::Ready { id } => {
                    let outcome = self
                        .storage
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
                        .await?;
                    match outcome {
                        ExecuteOutcome::Done {
                            landed_commit_id, ..
                        } => return Ok(landed_commit_id),
                        ExecuteOutcome::Requeued { successor_id, .. } => {
                            wait = self
                                .storage
                                .push_queue_service
                                .wait_and_claim(successor_id)
                                .await?;
                        }
                        ExecuteOutcome::ClaimLost { id } => {
                            wait = self.storage.push_queue_service.wait_and_claim(id).await?;
                        }
                        ExecuteOutcome::Failed { message, .. } => {
                            return Err(MegaError::Other(trunk_nff_align_message(&message)));
                        }
                        ExecuteOutcome::HardStopped { id } => {
                            return Err(MegaError::Other(format!(
                                "push hard-stopped for push_queue id {id}"
                            )));
                        }
                        ExecuteOutcome::BypassDetected { id } => {
                            return Err(MegaError::Other(format!(
                                "queue bypass detected for push_queue id {id}"
                            )));
                        }
                    }
                }
            }
        }
        Err(MegaError::Other(
            "push follow exceeded max conflict requeue rounds".into(),
        ))
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
                commit::Commit,
                signature::{Signature, SignatureType},
                tree::{Tree, TreeItem, TreeItemMode},
            },
            pack::entry::Entry,
        },
    };
    use sea_orm::{
        ColumnTrait, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder,
        TransactionTrait,
    };
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    use super::{Monorepo, RepoHandler, trunk_nff_align_message};
    use crate::{
        bellatrix::Bellatrix,
        callisto::{
            commit_auths, mega_cl, mega_commit, mega_refs, mega_tree, push_queue,
            sea_orm_active_enums::{PushQueueKindEnum, PushQueueStatusEnum},
        },
        ceres::{
            api_service::cache::GitObjectCache, pack::materialize,
            protocol::import_refs::RefCommand,
        },
        common::{
            errors::{MegaError, ProtocolError},
            utils::{MEGA_BRANCH_NAME, ZERO_ID},
        },
        config::{PushPolicy, testing::isolated_config},
        jupiter::{
            storage::{Storage, base_storage::StorageConnector},
            tests::{test_storage, test_storage_with_config, with_test_vault},
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

    fn test_monorepo(
        storage: &Storage,
        commands: Vec<RefCommand>,
        pack_commit_ids: HashSet<String>,
        new_commit_ids: HashSet<String>,
    ) -> Monorepo {
        // Never connected: the gate paths under test do not touch the cache.
        let connection = ::redis::aio::ConnectionManager::new_lazy_with_config(
            ::redis::Client::open("redis://127.0.0.1:6379").expect("redis client"),
            ::redis::aio::ConnectionManagerConfig::new(),
        )
        .expect("lazy connection manager");
        Monorepo {
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

    fn blob_item(name: &str, hex: &str) -> TreeItem {
        TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str(hex).unwrap(),
            name.to_string(),
        )
    }

    async fn trunk_storage(temp: &std::path::Path) -> Storage {
        let mut config = isolated_config(temp.join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        let storage = test_storage_with_config(temp, config).await;
        with_test_vault(storage, temp).await
    }

    async fn trunk_monorepo(
        storage: &Storage,
        path: &str,
        commands: Vec<RefCommand>,
        pack_commit_ids: HashSet<String>,
        new_commit_ids: HashSet<String>,
        username: Option<String>,
    ) -> Monorepo {
        Monorepo {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection: crate::jupiter::tests::test_redis_manager().await,
                prefix: String::new(),
            }),
            path: PathBuf::from(path),
            base_branch: "main".to_string(),
            pack_commit_ids: Mutex::new(pack_commit_ids),
            new_commit_ids: Mutex::new(new_commit_ids),
            no_op_notice: Mutex::new(None),
            push_chain_cache: Mutex::new(HashMap::new()),
            cl_link: Arc::new(RwLock::new(None)),
            bellatrix: Arc::new(Bellatrix::new(storage.config().build.clone())),
            username,
            command_list: Mutex::new(commands),
        }
    }

    async fn trunk_path_fixture(
        dir: &str,
        path_commit_msg: &str,
    ) -> (TempDir, Storage, Tree, Commit, Tree, Commit, String) {
        let temp = TempDir::new().expect("temp");
        let storage = trunk_storage(temp.path()).await;
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
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                root_commit.id.to_string(),
                root_tree.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let path = format!("/{dir}");
        mono.save_refs(
            mega_refs::Model::new(
                path.clone(),
                MEGA_BRANCH_NAME.to_owned(),
                path_commit.id.to_string(),
                child.id.to_string(),
                false,
            ),
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
        let model: mega_commit::Model = tip.clone().into_mega_model(EntryMeta::default());
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
            id: crate::callisto::entity_ext::generate_id(),
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
                id: crate::callisto::entity_ext::generate_id(),
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
                id: crate::callisto::entity_ext::generate_id(),
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

    #[tokio::test]
    async fn delete_path_dirty_outbox_skips_published_version() {
        let temp = TempDir::new().expect("temp dir");
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.cedar.enforcement = "enforce".to_string();
        let storage = crate::jupiter::tests::test_storage_with_config(temp.path(), config).await;
        storage
            .entity_store()
            .swap(
                &crate::contract::policy::entitystore::generate_entity(&["admin".to_string()], "/")
                    .expect("generate"),
            )
            .expect("baseline snapshot");
        assert!(!storage.entity_store().is_dirty());

        let commands = vec![RefCommand::new(
            "a".repeat(40),
            ZERO_ID.to_string(),
            "refs/heads/feature".to_string(),
        )];
        let repo = test_monorepo(&storage, commands, HashSet::new(), HashSet::new());
        repo.persist_mono_branch_cl_mega_refs_transaction()
            .await
            .expect("delete persist");

        let conn = storage.mono_storage().get_connection().clone();
        let rows = crate::callisto::authz_notify_outbox::Entity::find()
            .all(&conn)
            .await
            .expect("outbox");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].dirty, "delete path is an unversioned dirty mark");
        assert!(rows[0].version.is_none());
        assert!(rows[0].replayed_at.is_some());
        assert_eq!(
            crate::jupiter::storage::push_queue_storage::PushQueueStorage::load_published_version(
                &conn
            )
            .await
            .expect("watermark"),
            0,
            "delete compensate must not CAS published_version"
        );
    }

    async fn seed_root_with_dir(storage: &Storage, dir: &str) -> (String, String) {
        let blob_id = storage
            .git_service
            .save_object_from_raw(bytes::Bytes::from_static(b"keep"))
            .await
            .expect("blob");
        let leaf = Tree::from_tree_items(vec![TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str(&blob_id).expect("blob hash"),
            ".gitkeep".to_string(),
        )])
        .expect("leaf");
        let root = Tree::from_tree_items(vec![TreeItem::new(
            TreeItemMode::Tree,
            leaf.id,
            dir.to_string(),
        )])
        .expect("root");
        let commit = test_commit("root with dir");
        let commit = Commit::new(
            commit.author,
            commit.committer,
            root.id,
            vec![],
            &commit.message,
        );
        let commit_id = commit.id.to_string();
        let tree_id = root.id.to_string();
        storage
            .mono_storage()
            .save_mega_trees(
                vec![leaf, root],
                ObjectHash::from_str(&commit_id).unwrap(),
                None,
            )
            .await
            .expect("trees");
        storage
            .mono_storage()
            .save_mega_commits(vec![commit], None)
            .await
            .expect("commit");
        storage
            .mono_storage()
            .save_refs(
                crate::callisto::mega_refs::Model::new(
                    "/",
                    MEGA_BRANCH_NAME.to_owned(),
                    commit_id.clone(),
                    tree_id.clone(),
                    false,
                ),
                None,
            )
            .await
            .expect("root ref");
        (commit_id, tree_id)
    }

    #[tokio::test]
    async fn tp09_materialize_skips_when_tombstone_path_missing_from_root() {
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        seed_root_with_dir(&storage, "other").await;
        storage
            .mono_storage()
            .upsert_tombstone("/foo", MEGA_BRANCH_NAME, &"a".repeat(40), &"b".repeat(40))
            .await
            .unwrap();
        let head = crate::ceres::code_edit::utils::create_repo_commit(&storage, "/foo")
            .await
            .unwrap();
        assert_eq!(head, ZERO_ID);
        assert!(
            storage
                .mono_storage()
                .get_main_ref("/foo")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .mono_storage()
                .get_tombstone("/foo", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_some(),
            "skip must keep the tombstone"
        );
    }

    #[tokio::test]
    async fn tp09_materialize_continues_from_tombstone_then_deletes_it() {
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        seed_root_with_dir(&storage, "foo").await;
        let old_tip = "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25";
        storage
            .mono_storage()
            .upsert_tombstone("/foo", MEGA_BRANCH_NAME, old_tip, &"b".repeat(40))
            .await
            .unwrap();
        let head = crate::ceres::code_edit::utils::create_repo_commit(&storage, "/foo")
            .await
            .unwrap();
        assert_ne!(head, ZERO_ID);
        let commit = storage
            .mono_storage()
            .get_commit_by_hash(&head)
            .await
            .unwrap()
            .expect("commit");
        let parents: Vec<String> = serde_json::from_value(commit.parents_id).unwrap();
        assert_eq!(parents, vec![old_tip.to_string()]);
        assert!(
            storage
                .mono_storage()
                .get_tombstone("/foo", MEGA_BRANCH_NAME)
                .await
                .unwrap()
                .is_none(),
            "revival must delete the tombstone"
        );
        let row = storage
            .mono_storage()
            .get_main_ref("/foo")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.ref_commit_hash, head);
    }

    async fn advance_root_dir(
        storage: &Storage,
        dir: &str,
        blob: &'static [u8],
    ) -> (String, String) {
        let mono = storage.mono_storage();
        let old = mono.get_main_ref("/").await.unwrap().expect("root ref");
        let blob_id = storage
            .git_service
            .save_object_from_raw(bytes::Bytes::from_static(blob))
            .await
            .expect("blob");
        let leaf = Tree::from_tree_items(vec![TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str(&blob_id).expect("blob hash"),
            ".gitkeep".to_string(),
        )])
        .expect("leaf");
        let root = Tree::from_tree_items(vec![TreeItem::new(
            TreeItemMode::Tree,
            leaf.id,
            dir.to_string(),
        )])
        .expect("root");
        let commit = test_commit("advanced root");
        let parent = ObjectHash::from_str(&old.ref_commit_hash).unwrap();
        let commit = Commit::new(
            commit.author,
            commit.committer,
            root.id,
            vec![parent],
            &commit.message,
        );
        let commit_id = commit.id.to_string();
        let tree_id = root.id.to_string();
        mono.save_mega_trees(
            vec![leaf, root],
            ObjectHash::from_str(&commit_id).unwrap(),
            None,
        )
        .await
        .expect("trees");
        mono.save_mega_commits(vec![commit], None)
            .await
            .expect("commit");
        let txn = mono.get_connection().begin().await.unwrap();
        let ok = mono
            .cas_update_root_main_ref_in_txn(
                &txn,
                Some(&old.ref_commit_hash),
                Some(&old.ref_tree_hash),
                &commit_id,
                &tree_id,
            )
            .await
            .unwrap();
        assert!(ok, "root CAS must succeed");
        txn.commit().await.unwrap();
        (commit_id, tree_id)
    }

    #[tokio::test]
    async fn tp10_materialize_abandons_when_root_advances_after_walk() {
        let _lock = materialize::lock_materialize_tests().await;
        materialize::reset_materialize_test_counters();
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        seed_root_with_dir(&storage, "foo").await;
        let mut first = true;
        let refs = materialize::materialize_path_refs_with_hook(&storage, "/foo", false, || {
            let s = storage.clone();
            let run = first;
            first = false;
            async move {
                if run {
                    advance_root_dir(&s, "foo", b"next").await;
                }
            }
        })
        .await
        .expect("retry after abandon must succeed");
        assert!(
            materialize::ABANDON_COUNT.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "lock insert must abandon the stale walk"
        );
        let row = refs
            .iter()
            .find(|r| r.ref_name == MEGA_BRANCH_NAME)
            .expect("main ref");
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let root_tree = Tree::from_mega_model(
            storage
                .mono_storage()
                .get_tree_by_hash(&root.ref_tree_hash)
                .await
                .unwrap()
                .unwrap(),
        );
        let foo_id = root_tree
            .tree_items
            .iter()
            .find(|i| i.name == "foo")
            .expect("foo")
            .id
            .to_string();
        assert_eq!(row.ref_tree_hash, foo_id);
        assert_ne!(row.ref_commit_hash, ZERO_ID);
    }

    #[tokio::test]
    async fn tp10_materialize_retries_exhausted_returns_error_not_empty_refs() {
        let _lock = materialize::lock_materialize_tests().await;
        materialize::reset_materialize_test_counters();
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        seed_root_with_dir(&storage, "foo").await;
        let err = materialize::materialize_path_refs_with_hook(&storage, "/foo", false, || {
            let s = storage.clone();
            async move {
                advance_root_dir(&s, "foo", b"spin").await;
            }
        })
        .await
        .expect_err("retries exhausted");
        assert!(matches!(err, MegaError::MaterializeAborted));
        let mapped: ProtocolError = err.into();
        assert!(matches!(mapped, ProtocolError::AdvertiseFailed));
        assert!(
            storage
                .mono_storage()
                .get_main_ref("/foo")
                .await
                .unwrap()
                .is_none(),
            "abandon must not insert"
        );
        let create_err = crate::ceres::code_edit::utils::create_repo_commit(&storage, "/missing")
            .await
            .unwrap();
        assert_eq!(
            create_err, ZERO_ID,
            "path absent still advertises as empty, not MaterializeAborted"
        );
    }

    #[tokio::test]
    async fn tp10_not_exists_rereads_persisted_row() {
        let _lock = materialize::lock_materialize_tests().await;
        materialize::reset_materialize_test_counters();
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        seed_root_with_dir(&storage, "foo").await;
        let dummy_commit = "aa".repeat(20);
        let dummy_tree = "bb".repeat(20);
        storage
            .mono_storage()
            .save_refs(
                crate::callisto::mega_refs::Model::new(
                    "/foo",
                    MEGA_BRANCH_NAME.to_owned(),
                    dummy_commit.clone(),
                    dummy_tree.clone(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        let refs =
            materialize::materialize_path_refs_with_hook(&storage, "/foo", true, || async {})
                .await
                .expect("NOT EXISTS re-read");
        let row = refs
            .iter()
            .find(|r| r.ref_name == MEGA_BRANCH_NAME)
            .expect("main");
        assert_eq!(row.ref_commit_hash, dummy_commit);
        assert_eq!(row.ref_tree_hash, dummy_tree);
    }

    #[tokio::test]
    async fn tp10_retry_bypasses_heads_exist() {
        let _lock = materialize::lock_materialize_tests().await;
        materialize::reset_materialize_test_counters();
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        seed_root_with_dir(&storage, "foo").await;
        let dummy_commit = "cc".repeat(20);
        let dummy_tree = "dd".repeat(20);
        let mut n = 0u8;
        let _refs = materialize::materialize_path_refs_with_hook(&storage, "/foo", false, || {
            n += 1;
            let s = storage.clone();
            let insert = n == 1;
            let dummy_commit = dummy_commit.clone();
            let dummy_tree = dummy_tree.clone();
            async move {
                if insert {
                    s.mono_storage()
                        .save_refs(
                            crate::callisto::mega_refs::Model::new(
                                "/foo",
                                MEGA_BRANCH_NAME.to_owned(),
                                dummy_commit,
                                dummy_tree,
                                false,
                            ),
                            None,
                        )
                        .await
                        .unwrap();
                    advance_root_dir(&s, "foo", b"next").await;
                }
            }
        })
        .await
        .expect("retry after concurrent insert");
        assert!(
            materialize::WALK_COUNT.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "retry must bypass heads_exist and walk again"
        );
    }

    #[tokio::test]
    async fn tp10_refs_with_head_hash_materializes_without_race() {
        let _lock = materialize::lock_materialize_tests().await;
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        seed_root_with_dir(&storage, "foo").await;
        let mut repo = test_monorepo(&storage, vec![], HashSet::new(), HashSet::new());
        repo.path = PathBuf::from("/foo");
        let (head, refs) = repo.refs_with_head_hash().await.expect("advertise");
        assert_ne!(head, ZERO_ID);
        assert!(refs.iter().any(|r| r.default_branch));
        let via_create = crate::ceres::code_edit::utils::create_repo_commit(&storage, "/foo")
            .await
            .unwrap();
        assert_eq!(head, via_create);
    }

    #[tokio::test]
    async fn tp13_review_index_writes_null_watermark() {
        use crate::jupiter::storage::blob_path_index::BlobPathIndexMode;

        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        let blob = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let tree =
            Tree::from_tree_items(vec![git_internal::internal::object::tree::TreeItem::new(
                TreeItemMode::Blob,
                ObjectHash::from_str(blob).unwrap(),
                "readme.txt".to_string(),
            )])
            .unwrap();
        let commit = ObjectHash::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        mono.save_mega_trees(vec![tree.clone()], commit, None)
            .await
            .unwrap();
        let stats = mono
            .index_tree_blob_paths(&tree.id.to_string(), "/", BlobPathIndexMode::Review)
            .await
            .unwrap();
        assert!(!stats.skipped);
        let rows = mono.list_blob_paths().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/readme.txt");
        assert!(rows[0].indexed_push_id.is_none());
    }

    #[tokio::test]
    async fn tp13_c_segment_after_commit_uses_queue_watermark() {
        use crate::{
            callisto::mega_refs, common::utils::MEGA_BRANCH_NAME,
            jupiter::storage::blob_path_index::BlobPathIndexMode,
        };

        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        let mono = storage.mono_storage();
        let blob = "cccccccccccccccccccccccccccccccccccccccc";
        let tree =
            Tree::from_tree_items(vec![git_internal::internal::object::tree::TreeItem::new(
                TreeItemMode::Blob,
                ObjectHash::from_str(blob).unwrap(),
                "q.txt".to_string(),
            )])
            .unwrap();
        let commit = ObjectHash::from_str("dddddddddddddddddddddddddddddddddddddddd").unwrap();
        mono.save_mega_trees(vec![tree.clone()], commit, None)
            .await
            .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                "/q",
                MEGA_BRANCH_NAME.to_owned(),
                commit.to_string(),
                tree.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();

        // C-segment runs after B3 commit (lock released); this is the same
        // entry `execute_b3` calls once the txn has committed.
        let stats = mono
            .index_blob_paths_c_segment("/q", BlobPathIndexMode::Queue { push_id: 42 })
            .await
            .unwrap();
        assert!(!stats.skipped);
        let rows = mono.list_blob_paths().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, "/q/q.txt");
        assert_eq!(rows[0].indexed_push_id, Some(42));
    }

    #[test]
    fn trunk_nff_align_appends_fetch_reset_once() {
        let raw = "non-fast-forward: ref_commit_hash does not match old_id";
        let aligned = trunk_nff_align_message(raw);
        assert!(aligned.contains("git fetch && git reset --hard origin/main"));
        assert_eq!(trunk_nff_align_message(&aligned), aligned);
        assert_eq!(trunk_nff_align_message("other"), "other");
        let broken = "push chain is broken: base aaa is not on the first-parent chain of tip bbb";
        let broken_aligned = trunk_nff_align_message(broken);
        assert!(broken_aligned.contains("git fetch && git reset --hard origin/main"));
        assert_eq!(trunk_nff_align_message(&broken_aligned), broken_aligned);
    }

    #[tokio::test]
    async fn tp17_trunk_advertise_omits_cl_refs_review_keeps_them() {
        let _lock = materialize::lock_materialize_tests().await;
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        let (commit_id, tree_id) = seed_root_with_dir(&storage, "foo").await;
        storage
            .mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    "/foo",
                    MEGA_BRANCH_NAME.to_owned(),
                    commit_id.clone(),
                    tree_id.clone(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    "/foo",
                    "refs/cl/archived".to_owned(),
                    commit_id,
                    tree_id,
                    true,
                ),
                None,
            )
            .await
            .unwrap();

        let mut review = test_monorepo(&storage, vec![], HashSet::new(), HashSet::new());
        review.path = PathBuf::from("/foo");
        let (_head, refs) = review
            .refs_with_head_hash()
            .await
            .expect("review advertise");
        assert!(
            refs.iter().any(|r| r.ref_name == "refs/cl/archived"),
            "review morphology advertises archived CL refs: {:?}",
            refs.iter().map(|r| r.ref_name.as_str()).collect::<Vec<_>>()
        );

        let trunk_temp = TempDir::new().expect("trunk temp");
        let trunk_storage = trunk_storage(trunk_temp.path()).await;
        let (commit_id, tree_id) = seed_root_with_dir(&trunk_storage, "foo").await;
        trunk_storage
            .mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    "/foo",
                    MEGA_BRANCH_NAME.to_owned(),
                    commit_id.clone(),
                    tree_id.clone(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        trunk_storage
            .mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    "/foo",
                    "refs/cl/archived".to_owned(),
                    commit_id,
                    tree_id,
                    true,
                ),
                None,
            )
            .await
            .unwrap();
        let repo = trunk_monorepo(
            &trunk_storage,
            "/foo",
            vec![],
            HashSet::new(),
            HashSet::new(),
            Some("tester".into()),
        )
        .await;
        let (_head, refs) = repo.refs_with_head_hash().await.expect("trunk advertise");
        assert!(
            refs.iter().all(|r| r.ref_name != "refs/cl/archived"),
            "trunk morphology must not advertise CL refs: {:?}",
            refs.iter().map(|r| r.ref_name.as_str()).collect::<Vec<_>>()
        );
        assert!(refs.iter().any(|r| r.ref_name == MEGA_BRANCH_NAME));
    }

    #[tokio::test]
    async fn tp17_trunk_finalize_rejects_delete_without_cl() {
        let temp = TempDir::new().expect("temp");
        let storage = trunk_storage(temp.path()).await;
        let cmd = RefCommand::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ZERO_ID.to_string(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let repo = trunk_monorepo(
            &storage,
            "/foo",
            vec![cmd],
            HashSet::new(),
            HashSet::new(),
            Some("tester".into()),
        )
        .await;
        let err = repo
            .finalize_receive_pack()
            .await
            .expect_err("trunk delete must be rejected");
        assert!(
            err.to_string()
                .contains("trunk push rejects delete commands"),
            "{err}"
        );
        let cl_count = mega_cl::Entity::find()
            .count(storage.mono_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(cl_count, 0);
        let cl_refs = mega_refs::Entity::find()
            .filter(mega_refs::Column::IsCl.eq(true))
            .count(storage.mono_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(cl_refs, 0);
    }

    #[tokio::test]
    async fn tp18_trunk_validate_rejects_non_main_ref() {
        let temp = TempDir::new().expect("temp");
        let storage = trunk_storage(temp.path()).await;
        let cmd = RefCommand::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            "refs/heads/dev".to_string(),
        );
        let repo = trunk_monorepo(
            &storage,
            "/foo",
            vec![cmd],
            HashSet::new(),
            HashSet::new(),
            Some("tester".into()),
        )
        .await;
        let err = repo
            .validate_incoming_push()
            .await
            .expect_err("trunk must reject refs/heads/dev at B0");
        assert!(err.to_string().contains("only public branch"), "{err}");
        assert!(err.to_string().contains("refs/heads/dev"), "{err}");
    }

    #[tokio::test]
    async fn tp18_review_validate_does_not_use_trunk_branch_reject() {
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        let cmd = RefCommand::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            "refs/heads/dev".to_string(),
        );
        let repo = test_monorepo(&storage, vec![cmd], HashSet::new(), HashSet::new());
        let err = repo
            .validate_incoming_push()
            .await
            .expect_err("review still validates the chain");
        assert!(
            !err.to_string().contains("only public branch"),
            "review must not use the trunk unique-branch reject: {err}"
        );
    }

    #[tokio::test]
    async fn tp18_review_still_refuses_delete_main() {
        let temp = TempDir::new().expect("temp");
        let storage = test_storage(temp.path()).await;
        let cmd = RefCommand::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ZERO_ID.to_string(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let repo = test_monorepo(&storage, vec![cmd.clone()], HashSet::new(), HashSet::new());
        let err = repo
            .apply_cl_mega_ref_for_push_command(&cmd, None)
            .await
            .expect_err("UN-16 must still refuse deleting main under review");
        assert!(
            err.to_string()
                .contains("refusing to delete the main branch ref"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn tp17_trunk_finalize_n1_lands_client_tip_and_records_requester() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _rt, _rc, _child, path_commit, path) =
            trunk_path_fixture("p17n1", "path tip").await;
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
        let new_id = new_commit.id.to_string();
        let cmd = RefCommand::new(
            path_commit.id.to_string(),
            new_id.clone(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let repo = trunk_monorepo(
            &storage,
            &path,
            vec![cmd],
            id_set(&[&new_id]),
            id_set(&[&new_id]),
            None,
        )
        .await;
        repo.finalize_receive_pack()
            .await
            .expect("trunk N=1 finalize");
        let pref = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, new_id);
        let cl_refs = mega_refs::Entity::find()
            .filter(mega_refs::Column::IsCl.eq(true))
            .count(storage.mono_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(cl_refs, 0, "trunk push must not write refs/cl/*");
        let cl_count = mega_cl::Entity::find()
            .count(storage.mono_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(cl_count, 0, "trunk push must not create CLs");
        let row = push_queue::Entity::find()
            .filter(push_queue::Column::Kind.eq(PushQueueKindEnum::Push))
            .one(storage.mono_storage().get_connection())
            .await
            .unwrap()
            .expect("push_queue row");
        assert_eq!(row.status, PushQueueStatusEnum::Done);
        assert!(
            row.requester.is_none(),
            "push_auth=none records NULL requester"
        );
    }

    #[tokio::test]
    async fn tp17_trunk_finalize_token_requester_and_n_gt1_notice() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _rt, _rc, _child, path_commit, path) =
            trunk_path_fixture("p17n3", "path tip").await;
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
        let c1 = Commit::from_tree_id(t1.id, vec![path_commit.id], "c1");
        let c2 = Commit::from_tree_id(t2.id, vec![c1.id], "c2");
        let c3 = Commit::from_tree_id(t3.id, vec![c2.id], "c3");
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
        let new_id = c3.id.to_string();
        let cmd = RefCommand::new(
            path_commit.id.to_string(),
            new_id.clone(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let repo = trunk_monorepo(
            &storage,
            &path,
            vec![cmd],
            id_set(&[&c1.id.to_string(), &c2.id.to_string(), &new_id]),
            id_set(&[&c1.id.to_string(), &c2.id.to_string(), &new_id]),
            Some("ci".into()),
        )
        .await;
        repo.finalize_receive_pack()
            .await
            .expect("trunk N>1 finalize");
        let pref = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(pref.ref_commit_hash, new_id, "N>1 must squash");
        let squash = storage
            .mono_storage()
            .get_commit_by_hash(&pref.ref_commit_hash)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(squash.tree, t3.id.to_string());
        let parents: Vec<String> = serde_json::from_value(squash.parents_id).unwrap();
        assert_eq!(parents, vec![path_commit.id.to_string()]);
        let notice = repo.receive_pack_notice().expect("ADR-TP-18 squash notice");
        assert!(
            notice.contains(&pref.ref_commit_hash),
            "sideband must name the squash id: {notice}"
        );
        assert!(notice.contains("git fetch && git reset --hard origin/main"));
        let row = push_queue::Entity::find()
            .filter(push_queue::Column::Kind.eq(PushQueueKindEnum::Push))
            .one(storage.mono_storage().get_connection())
            .await
            .unwrap()
            .expect("push_queue row");
        assert_eq!(row.requester.as_deref(), Some("ci"));
    }

    #[tokio::test]
    async fn tp17_trunk_finalize_nff_includes_align_command() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _rt, _rc, _child, path_commit, path) =
            trunk_path_fixture("p17nff", "path tip").await;
        let landed = Tree::from_tree_items(vec![blob_item(
            "y.txt",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        )])
        .unwrap();
        let landed_commit = Commit::from_tree_id(landed.id, vec![path_commit.id], "already landed");
        let sibling = Tree::from_tree_items(vec![blob_item(
            "z.txt",
            "ffffffffffffffffffffffffffffffffffffffff",
        )])
        .unwrap();
        let sibling_commit = Commit::from_tree_id(sibling.id, vec![path_commit.id], "sibling");
        storage
            .mono_storage()
            .save_mega_trees(vec![landed, sibling], landed_commit.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![landed_commit.clone(), sibling_commit.clone()], None)
            .await
            .unwrap();

        let landed_id = landed_commit.id.to_string();
        let land_cmd = RefCommand::new(
            path_commit.id.to_string(),
            landed_id.clone(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let land_repo = trunk_monorepo(
            &storage,
            &path,
            vec![land_cmd],
            id_set(&[&landed_id]),
            id_set(&[&landed_id]),
            Some("tester".into()),
        )
        .await;
        land_repo
            .finalize_receive_pack()
            .await
            .expect("land N=1 tip before NFF probe");

        let new_id = sibling_commit.id.to_string();
        let cmd = RefCommand::new(
            path_commit.id.to_string(),
            new_id.clone(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let repo = trunk_monorepo(
            &storage,
            &path,
            vec![cmd],
            id_set(&[&new_id]),
            id_set(&[&new_id]),
            Some("tester".into()),
        )
        .await;
        let err = repo
            .finalize_receive_pack()
            .await
            .expect_err("stale old_id must NFF");
        let msg = err.to_string();
        assert!(msg.contains("non-fast-forward"), "{msg}");
        assert!(
            msg.contains("git fetch && git reset --hard origin/main"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn tp17_trunk_finalize_stale_nested_baseline_refuses() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, _rt, _rc, child, path_commit, path) =
            trunk_path_fixture("p17st", "path tip").await;
        let nested = Tree::from_tree_items(vec![blob_item(
            "n.txt",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )])
        .unwrap();
        let nested_commit = Commit::from_tree_id(nested.id, vec![], "nested");
        storage
            .mono_storage()
            .save_mega_trees(vec![nested.clone()], nested_commit.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![nested_commit.clone()], None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_refs(
                mega_refs::Model::new(
                    format!("{path}/b"),
                    MEGA_BRANCH_NAME.to_owned(),
                    nested_commit.id.to_string(),
                    nested.id.to_string(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
        let mut pref = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        pref.ref_tree_hash = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into();
        storage.mono_storage().update_ref(pref, None).await.unwrap();

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
        let new_id = new_commit.id.to_string();
        let cmd = RefCommand::new(
            path_commit.id.to_string(),
            new_id.clone(),
            MEGA_BRANCH_NAME.to_string(),
        );
        let repo = trunk_monorepo(
            &storage,
            &path,
            vec![cmd],
            id_set(&[&new_id]),
            id_set(&[&new_id]),
            Some("tester".into()),
        )
        .await;
        let err = repo
            .finalize_receive_pack()
            .await
            .expect_err("stale /a with nested /a/b must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("stale materialized") || msg.contains("advertise"),
            "{msg}"
        );
        let _ = child;
    }

    #[tokio::test]
    async fn tp17_twenty_concurrent_trunk_finalizes_serialize_root_chain() {
        let _lock = materialize::lock_materialize_tests().await;
        const N: usize = 20;
        let temp = TempDir::new().expect("temp");
        let storage = trunk_storage(temp.path()).await;
        let mono = storage.mono_storage();

        let mut child_trees = Vec::new();
        for i in 0..N {
            let tree =
                Tree::from_tree_items(vec![blob_item("base.txt", &format!("{:040x}", i + 1))])
                    .expect("child tree");
            child_trees.push(tree);
        }
        let root_items: Vec<TreeItem> = child_trees
            .iter()
            .enumerate()
            .map(|(i, t)| TreeItem::new(TreeItemMode::Tree, t.id, format!("p{i:02}")))
            .collect();
        let old_root = Tree::from_tree_items(root_items).expect("root");
        let old_commit = Commit::from_tree_id(old_root.id, vec![], "base");
        for t in &child_trees {
            mono.save_mega_trees(vec![t.clone()], old_commit.id, None)
                .await
                .unwrap();
        }
        mono.save_mega_trees(vec![old_root.clone()], old_commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![old_commit.clone()], None)
            .await
            .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                old_commit.id.to_string(),
                old_root.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();

        let mut repos = Vec::new();
        for (i, child) in child_trees.iter().enumerate() {
            let path = format!("/p{i:02}");
            let path_commit = Commit::from_tree_id(child.id, vec![], "path tip");
            let new_tree =
                Tree::from_tree_items(vec![blob_item("new.txt", &format!("{:040x}", 1000 + i))])
                    .expect("new tree");
            let new_commit = Commit::from_tree_id(new_tree.id, vec![path_commit.id], "n1");
            mono.save_mega_trees(vec![new_tree], new_commit.id, None)
                .await
                .unwrap();
            mono.save_mega_commits(vec![path_commit.clone(), new_commit.clone()], None)
                .await
                .unwrap();
            mono.save_refs(
                mega_refs::Model::new(
                    path.clone(),
                    MEGA_BRANCH_NAME.to_owned(),
                    path_commit.id.to_string(),
                    child.id.to_string(),
                    false,
                ),
                None,
            )
            .await
            .unwrap();
            let new_id = new_commit.id.to_string();
            let cmd = RefCommand::new(
                path_commit.id.to_string(),
                new_id.clone(),
                MEGA_BRANCH_NAME.to_string(),
            );
            repos.push(
                trunk_monorepo(
                    &storage,
                    &path,
                    vec![cmd],
                    id_set(&[&new_id]),
                    id_set(&[&new_id]),
                    Some("tester".into()),
                )
                .await,
            );
        }

        let mut joins = Vec::new();
        for repo in repos {
            joins.push(tokio::spawn(
                async move { repo.finalize_receive_pack().await },
            ));
        }
        for j in joins {
            j.await.expect("join").expect("concurrent trunk finalize");
        }

        for i in 0..N {
            let path = format!("/p{i:02}");
            let pref = mono.get_main_ref(&path).await.unwrap().unwrap();
            assert_ne!(
                pref.ref_commit_hash,
                old_commit.id.to_string(),
                "{path} must advance"
            );
        }
        let final_root = mono.get_main_ref("/").await.unwrap().unwrap();
        let final_tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&final_root.ref_tree_hash)
                .await
                .unwrap()
                .unwrap(),
        );
        assert_eq!(final_tree.tree_items.len(), N, "root tree keeps every path");

        let qrows = push_queue::Entity::find()
            .filter(push_queue::Column::Kind.eq(PushQueueKindEnum::Push))
            .filter(push_queue::Column::Status.eq(PushQueueStatusEnum::Done))
            .order_by_asc(push_queue::Column::Id)
            .all(storage.mono_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(qrows.len(), N);

        // Root roll-up parent chain is one commit per serialized B3 round; the
        // changed child name of each roll-up matches push_queue.id order.
        let mut hash = final_root.ref_commit_hash.clone();
        let mut changed_paths = Vec::new();
        for _ in 0..N {
            let commit = mono.get_commit_by_hash(&hash).await.unwrap().unwrap();
            let parents: Vec<String> =
                serde_json::from_value(commit.parents_id).unwrap_or_default();
            let parent_hash = parents
                .into_iter()
                .next()
                .expect("root roll-up must have a parent");
            let this_tree =
                Tree::from_mega_model(mono.get_tree_by_hash(&commit.tree).await.unwrap().unwrap());
            let parent_commit = mono
                .get_commit_by_hash(&parent_hash)
                .await
                .unwrap()
                .unwrap();
            let parent_tree = Tree::from_mega_model(
                mono.get_tree_by_hash(&parent_commit.tree)
                    .await
                    .unwrap()
                    .unwrap(),
            );
            let parent_ids: HashMap<String, String> = parent_tree
                .tree_items
                .iter()
                .map(|i| (i.name.clone(), i.id.to_string()))
                .collect();
            let changed: Vec<String> = this_tree
                .tree_items
                .iter()
                .filter(|i| parent_ids.get(&i.name).map(String::as_str) != Some(&i.id.to_string()))
                .map(|i| format!("/{}", i.name))
                .collect();
            assert_eq!(changed.len(), 1, "each roll-up changes exactly one path");
            changed_paths.push(changed[0].clone());
            hash = parent_hash;
        }
        assert_eq!(hash, old_commit.id.to_string());
        changed_paths.reverse();
        let queue_paths: Vec<String> = qrows.iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            changed_paths, queue_paths,
            "root roll-up parent chain must follow push_queue.id order"
        );
    }
}
