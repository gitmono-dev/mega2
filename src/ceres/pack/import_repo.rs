use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use async_recursion::async_recursion;
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use git_internal::{
    errors::GitError,
    hash::{HashKind, ObjectHash},
    internal::{
        metadata::{EntryMeta, MetaAttached},
        object::{blob::Blob, commit::Commit, tag::Tag, tree::Tree},
        pack::{encode::PackEncoder, entry::Entry},
    },
};
use sea_orm::{DatabaseTransaction, TransactionTrait};
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    callisto::{
        import_repo_cleanups::{self, CleanupState},
        sea_orm_active_enums::{ActorTypeEnum, AuditActionEnum, RefTypeEnum, TargetTypeEnum},
    },
    ceres::{
        api_service::cache::GitObjectCache,
        pack::RepoHandler,
        protocol::{
            import_refs::{CommandType, RefCommand, Refs},
            repo::Repo,
        },
    },
    common::{
        errors::{ImportRepoError, MegaError},
        utils::{ZERO_ID, is_protocol_zero_id},
    },
    jupiter::{
        service::{
            git_service::GitService,
            push_queue_service::{
                AttachCommand, AttachExecContext, AttachOp, AttachPayload, EnqueueRequest,
                ExecuteOutcome, ExecuteRequest, QueueWaitResult, attach_operation_id,
                detach_operation_id, normalize_attach_commands,
            },
        },
        storage::{
            Storage,
            audit_storage::{AuditStorage, IMPORT_REPO_REMOVE_KIND},
            base_storage::StorageConnector,
            git_db_storage::{
                CLEANUP_RESUME_BATCH, GitDbStorage, SWEEP_STATEMENT_BUDGET, SweepCounts,
            },
        },
        utils::converter::FromGitModel,
    },
};
#[rustfmt::skip]
use crate::orbit_api::object_storage::MultiObjectByteStream;

pub struct ImportRepo {
    pub storage: Storage,
    pub repo: Repo,
    pub command_list: Mutex<Vec<RefCommand>>,
    pub git_object_cache: Arc<GitObjectCache>,
    pub receive_pack_extra_timings_ms: Mutex<Vec<(String, u128)>>,
}

#[async_trait]
impl RepoHandler for ImportRepo {
    fn is_monorepo(&self) -> bool {
        false
    }

    fn object_hash_kind(&self) -> Result<HashKind, MegaError> {
        self.storage.config().monorepo.object_hash_kind()
    }

    fn save_entry_concurrency(&self) -> usize {
        self.storage.config().pack.save_entry_concurrency
    }

    fn receive_pack_extra_timings_ms(&self) -> Vec<(String, u128)> {
        std::mem::take(
            &mut self
                .receive_pack_extra_timings_ms
                .lock()
                .expect("receive_pack_extra_timings_ms lock poisoned"),
        )
    }

    fn sync_commands_after_unpack(&self, commands: &[RefCommand]) {
        *self
            .command_list
            .lock()
            .expect("command_list lock poisoned") = commands.to_vec();
    }

    async fn refs_with_head_hash(&self) -> Result<(String, Vec<Refs>), MegaError> {
        let result = self
            .storage
            .git_db_storage()
            .get_ref(self.repo.repo_id)
            .await?;
        let refs: Vec<Refs> = result.into_iter().map(|x| x.into()).collect();

        Ok(self.find_head_hash(refs))
    }

    async fn finalize_receive_pack(&self) -> Result<(), MegaError> {
        let t0 = Instant::now();
        let t_fp = Instant::now();
        self.traverses_tree_and_update_filepath().await?;
        self.receive_pack_extra_timings_ms
            .lock()
            .expect("receive_pack_extra_timings_ms lock poisoned")
            .push((
                "import_filepath_update_ms".to_string(),
                t_fp.elapsed().as_millis(),
            ));

        let t_attach = Instant::now();
        self.attach_to_monorepo_parent().await?;
        self.receive_pack_extra_timings_ms
            .lock()
            .expect("receive_pack_extra_timings_ms lock poisoned")
            .extend([
                (
                    "import_attach_to_monorepo_parent_ms".to_string(),
                    t_attach.elapsed().as_millis(),
                ),
                (
                    "import_finalize_total_ms".to_string(),
                    t0.elapsed().as_millis(),
                ),
            ]);
        Ok(())
    }

    async fn save_entry(
        &self,
        entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<(), MegaError> {
        self.storage
            .import_service
            .save_entry(self.repo.repo_id, &self.repo.repo_path, entry_list)
            .await
    }

    async fn update_pack_id(&self, temp_pack_id: &str, pack_id: &str) -> Result<(), MegaError> {
        let storage = self.storage.git_db_storage();
        storage.update_pack_id(temp_pack_id, pack_id).await
    }

    async fn check_entry(&self, _: &Entry) -> Result<(), GitError> {
        Ok(())
    }

    async fn full_pack(&self, _: Vec<String>) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        let pack_config = &self.storage.config().pack;
        let (entry_tx, entry_rx) = mpsc::channel(pack_config.channel_message_size);
        let (stream_tx, stream_rx) = mpsc::channel(pack_config.channel_message_size);

        let storage = self.storage.git_db_storage();
        let git_service = self.storage.git_service.clone();
        let total = storage.get_obj_count_by_repo_id(self.repo.repo_id).await;
        let encoder =
            PackEncoder::new_with_hash_kind(self.object_hash_kind()?, total, 0, stream_tx);
        encoder.encode_async(entry_rx).await?;

        let repo_id = self.repo.repo_id;
        tokio::spawn(async move {
            if let Err(e) = process_objects(repo_id, git_service, storage, entry_tx).await {
                tracing::error!(?e, "process_blobs failed");
            }
        });

        Ok(ReceiverStream::new(stream_rx))
    }

    async fn incremental_pack(
        &self,
        want: Vec<String>,
        have: Vec<String>,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        let mut want_clone = want.clone();
        let pack_config = &self.storage.config().pack;
        let storage = self.storage.git_db_storage();
        let obj_num = AtomicUsize::new(0);

        let mut exist_objs = HashSet::new();

        let mut want_commits: Vec<Commit> = storage
            .get_commits_by_hashes(self.repo.repo_id, &want_clone)
            .await
            .unwrap()
            .into_iter()
            .map(Commit::from_git_model)
            .collect();
        let mut traversal_list: Vec<Commit> = want_commits.clone();

        // traverse commit's all parents to find the commit that client does not have
        while let Some(temp) = traversal_list.pop() {
            for p_commit_id in temp.parent_commit_ids {
                let p_commit_id = p_commit_id.to_string();

                if !have.contains(&p_commit_id) && !want_clone.contains(&p_commit_id) {
                    let parent: Commit = Commit::from_git_model(
                        storage
                            .get_commit_by_hash(self.repo.repo_id, &p_commit_id)
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

        let hash_kind = self.object_hash_kind()?;
        let want_tree_ids = want_commits.iter().map(|c| c.tree_id.to_string()).collect();
        let want_trees: HashMap<ObjectHash, Tree> = storage
            .get_trees_by_hashes(self.repo.repo_id, want_tree_ids)
            .await
            .unwrap()
            .into_iter()
            .map(|m| {
                (
                    ObjectHash::from_hex_for_kind(hash_kind, &m.tree_id).unwrap(),
                    Tree::from_git_model(m),
                )
            })
            .collect();

        obj_num.fetch_add(want_commits.len(), Ordering::SeqCst);

        let have_commits = storage
            .get_commits_by_hashes(self.repo.repo_id, &have)
            .await
            .unwrap();
        let have_trees = storage
            .get_trees_by_hashes(
                self.repo.repo_id,
                have_commits.iter().map(|x| x.tree.clone()).collect(),
            )
            .await
            .unwrap();
        // traverse to get exist_objs
        for have_tree in have_trees {
            self.traverse(Tree::from_git_model(have_tree), &mut exist_objs, None)
                .await?;
        }

        let mut counted_obj = HashSet::new();
        let mut counted_roots = HashSet::new();
        // traverse for get obj nums; shared commit trees counted once
        for c in want_commits.clone() {
            if !counted_roots.insert(c.tree_id.to_string()) {
                continue;
            }
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
        let encoder = PackEncoder::new_with_hash_kind(
            self.object_hash_kind()?,
            obj_num.into_inner(),
            0,
            stream_tx,
        );
        encoder
            .encode_async(entry_rx)
            .await
            .map_err(|e| MegaError::Other(format!("pack encode failed: {e}")))?;

        // Every object must appear exactly once in the pack; two want commits
        // may share one tree.
        for c in want_commits {
            if exist_objs.insert(c.tree_id.to_string()) {
                self.traverse(
                    want_trees.get(&c.tree_id).unwrap().clone(),
                    &mut exist_objs,
                    Some(&entry_tx),
                )
                .await?;
            }
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
            .git_db_storage()
            .get_trees_by_hashes(self.repo.repo_id, hashes)
            .await
            .unwrap()
            .into_iter()
            .map(Tree::from_git_model)
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
            .git_db_storage()
            .get_blobs_by_hashes(self.repo.repo_id, hashes)
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
                        // NOTE: We currently do not have CRC32 information available in the
                        // blob metadata returned from `git_db_storage()`. Downstream callers
                        // treat `None` as "CRC32 unknown" rather than "CRC32 invalid". Once
                        // pack index entries (or another source) expose CRC32 for these blobs,
                        // this should be populated with the actual checksum instead of `None`.
                        // TODO: Thread CRC32 from the underlying Git storage into `EntryMeta`.
                        crc32: None,
                    },
                )
            })
            .collect::<HashMap<String, EntryMeta>>();

        Ok(map)
    }

    async fn update_refs(&self, refs: &RefCommand) -> Result<(), GitError> {
        if refs.ref_type != RefTypeEnum::Tag {
            // Branch `import_refs` rows are written in the same DB transaction as monorepo attach.
            return Ok(());
        }
        // Tags are written right after unpack, one ref at a time, with the
        // receive-pack CAS (plan-20260923 ADR-FU-08 items 2 and 4), each in
        // its own transaction behind the liveness lock (ADR-FU-09 item 5).
        let txn = self
            .storage
            .git_db_storage()
            .get_connection()
            .begin()
            .await
            .map_err(|e| GitError::CustomError(e.to_string()))?;
        let applied = match self.write_tag_ref_in_txn(refs, &txn).await {
            Ok(applied) => applied,
            Err(e) => {
                let _ = txn.rollback().await;
                return Err(GitError::CustomError(e.to_string()));
            }
        };
        if applied {
            txn.commit()
                .await
                .map_err(|e| GitError::CustomError(e.to_string()))?;
        } else {
            let _ = txn.rollback().await;
        }
        if !applied {
            return Err(GitError::CustomError(
                ImportRepoError::StaleRef {
                    ref_name: refs.ref_name.clone(),
                    expected: refs.old_id.clone(),
                }
                .to_string(),
            ));
        }
        Ok(())
    }

    async fn check_commit_exist(&self, hash: &str) -> bool {
        self.storage
            .git_db_storage()
            .get_commit_by_hash(self.repo.repo_id, hash)
            .await
            .ok()
            .flatten()
            .is_some()
    }

    async fn check_object_exist(&self, hash: &str) -> bool {
        self.storage
            .git_db_storage()
            .object_exists(self.repo.repo_id, hash)
            .await
            .unwrap_or(false)
    }

    async fn check_default_branch(&self) -> bool {
        let storage = self.storage.git_db_storage();
        storage
            .default_branch_exist(self.repo.repo_id)
            .await
            .unwrap()
    }

    async fn traverses_tree_and_update_filepath(&self) -> Result<(), MegaError> {
        // A repository removed meanwhile is refused before any object is read
        // (plan-20260923 ADR-FU-09 item 5).
        if !import_repo_is_live(&self.storage, self.repo.repo_id, &self.repo.repo_path).await? {
            return Err(ImportRepoError::Removed {
                path: self.repo.repo_path.clone(),
            }
            .into());
        }
        let pairs = match self.filepath_pairs().await {
            Ok(pairs) => pairs,
            Err(error) => return Err(self.removed_or(error).await),
        };
        self.storage
            .git_db_storage()
            .update_import_blob_filepaths_fenced(self.repo.repo_id, &self.repo.repo_path, pairs)
            .await
    }
}

#[async_recursion]
pub(crate) async fn collect_git_blob_filepaths(
    storage: GitDbStorage,
    repo_id: i64,
    tree: Tree,
    path: PathBuf,
) -> Result<Vec<(String, String)>, MegaError> {
    let mut pairs = Vec::new();
    for item in tree.tree_items {
        if item.is_tree() {
            let child = Tree::from_git_model(
                storage
                    .get_tree_by_hash(repo_id, &item.id.to_string())
                    .await?
                    .ok_or_else(|| MegaError::NotFound(format!("tree {}", item.id)))?,
            );
            pairs.extend(
                collect_git_blob_filepaths(storage.clone(), repo_id, child, path.join(item.name))
                    .await?,
            );
        } else {
            pairs.push((
                item.id.to_string(),
                path.join(item.name).to_string_lossy().into_owned(),
            ));
        }
    }
    Ok(pairs)
}

impl ImportRepo {
    /// One receive-pack tag write inside `txn`: the liveness lock, then
    /// exactly one CAS statement; whether the CAS applied.
    pub(crate) async fn write_tag_ref_in_txn(
        &self,
        refs: &RefCommand,
        txn: &DatabaseTransaction,
    ) -> Result<bool, MegaError> {
        let storage = self.storage.git_db_storage();
        storage
            .lock_live_import_repo(txn, self.repo.repo_id, &self.repo.repo_path)
            .await?;
        match refs.command_type {
            CommandType::Create => {
                storage
                    .create_ref_if_absent(self.repo.repo_id, refs.clone().into(), txn)
                    .await
            }
            CommandType::Delete => {
                storage
                    .remove_ref_if_unchanged(self.repo.repo_id, &refs.ref_name, &refs.old_id, txn)
                    .await
            }
            CommandType::Update => {
                storage
                    .update_ref_if_unchanged(
                        self.repo.repo_id,
                        &refs.ref_name,
                        &refs.old_id,
                        &refs.new_id,
                        txn,
                    )
                    .await
            }
        }
    }

    /// `(blob_id, path)` of every blob under this push's head.
    async fn filepath_pairs(&self) -> Result<Vec<(String, String)>, MegaError> {
        // Prefer the branch tip from this receive-pack (same as `attach_to_monorepo_parent`).
        // DB `import_refs` is not updated until the attach transaction, so reading HEAD only
        // from the DB would still see the pre-push tip during finalize.
        let from_commands = {
            let cmds = self
                .command_list
                .lock()
                .expect("command_list lock poisoned");
            cmds.iter()
                .find(|c| c.ref_type == RefTypeEnum::Branch && !is_protocol_zero_id(&c.new_id))
                .map(|c| c.new_id.clone())
        };
        let current_head = match from_commands {
            Some(h) => h,
            None => self.refs_with_head_hash().await?.0,
        };
        let git_db = self.storage.git_db_storage();
        let commit = Commit::from_git_model(
            git_db
                .get_commit_by_hash(self.repo.repo_id, &current_head)
                .await?
                .ok_or_else(|| MegaError::NotFound(format!("commit {current_head}")))?,
        );
        let root_tree = Tree::from_git_model(
            git_db
                .get_tree_by_hash(self.repo.repo_id, &commit.tree_id.to_string())
                .await?
                .ok_or_else(|| MegaError::NotFound(format!("tree {}", commit.tree_id)))?,
        );
        collect_git_blob_filepaths(git_db, self.repo.repo_id, root_tree, PathBuf::new()).await
    }

    /// `IMPORT_REPO_REMOVED` when the repository is gone (the sweep took the
    /// objects), else `error`.
    async fn removed_or(&self, error: MegaError) -> MegaError {
        match import_repo_is_live(&self.storage, self.repo.repo_id, &self.repo.repo_path).await {
            Ok(false) => ImportRepoError::Removed {
                path: self.repo.repo_path.clone(),
            }
            .into(),
            _ => error,
        }
    }

    /// Whether this push's branch commands are already in effect: every
    /// Create / Update ref points at its `new_id` and every Delete ref is gone.
    async fn attach_refs_applied(&self, payload: &AttachPayload) -> Result<bool, MegaError> {
        let names: Vec<String> = payload
            .commands
            .iter()
            .map(|cmd| cmd.ref_name.clone())
            .collect();
        let refs = self
            .storage
            .git_db_storage()
            .get_refs_by_names(self.repo.repo_id, &names)
            .await?;
        Ok(payload.commands.iter().all(|cmd| {
            let current = refs
                .iter()
                .find(|r| r.ref_name == cmd.ref_name)
                .map(|r| r.ref_git_id.as_str());
            match cmd.command_type.as_str() {
                "Delete" => current.is_none(),
                _ => current == Some(cmd.new_id.as_str()),
            }
        }))
    }

    // attach import repo to monorepo parent tree via MonoWriteQueue (TP-08).
    pub(crate) async fn attach_to_monorepo_parent(&self) -> Result<(), MegaError> {
        // Snapshot commands without holding the mutex across await (Send + avoids deadlocks).
        let commands_snapshot: Vec<RefCommand> = self
            .command_list
            .lock()
            .expect("command_list lock poisoned")
            .clone();
        let commit_id = commands_snapshot
            .iter()
            .find(|c| {
                c.status == "ok"
                    && c.ref_type == RefTypeEnum::Branch
                    && !is_protocol_zero_id(&c.new_id)
            })
            .map(|c| c.new_id.clone())
            // A delete-only batch has no tip; it still goes through B3 so the
            // branch writes stay inside the attach transaction (GC-FU-03).
            .unwrap_or_else(|| ZERO_ID.to_owned());

        let path = crate::common::utils::canonicalize_mono_ref_path(&self.repo.repo_path)?;
        // Materialization precheck permits P=/ (main@/ does not participate), but
        // ImportRepo attach mounts a named leaf via search_and_create_tree and
        // cannot target the monorepo root itself — fail before enqueue.
        if path == "/" {
            return Err(MegaError::Other(
                "attach to monorepo path '/' is not supported (no leaf name for tree mount)".into(),
            ));
        }
        let mono = self.storage.mono_storage();
        // Materialization precheck runs under B3 lock only so identical retries
        // can adopt/replay Done without being blocked by post-success lazy
        // materialization of the target path.

        let attach_cmds: Vec<AttachCommand> = commands_snapshot
            .iter()
            .filter(|c| c.status == "ok" && c.ref_type == RefTypeEnum::Branch)
            .map(|c| AttachCommand {
                ref_name: c.ref_name.clone(),
                old_id: c.old_id.clone(),
                new_id: c.new_id.clone(),
                command_type: match c.command_type {
                    CommandType::Create => "Create".into(),
                    CommandType::Update => "Update".into(),
                    CommandType::Delete => "Delete".into(),
                },
                ref_type: "branch".into(),
                default_branch: c.default_branch,
            })
            .collect();
        let payload = AttachPayload {
            op: AttachOp::Attach,
            repo_id: self.repo.repo_id,
            repo_path: path.clone(),
            commands: attach_cmds.clone(),
        };
        let fingerprint_rows: Vec<_> = attach_cmds
            .iter()
            .map(|c| {
                (
                    c.ref_name.clone(),
                    c.command_type.clone(),
                    c.old_id.clone(),
                    c.new_id.clone(),
                )
            })
            .collect();
        let fingerprint = normalize_attach_commands(&fingerprint_rows);
        let mut operation_id = attach_operation_id(&self.repo.repo_id.to_string(), &fingerprint);
        let payload_json = serde_json::to_value(&payload)
            .map_err(|e| MegaError::Other(format!("attach payload encode: {e}")))?;

        // A Done row is a permanent replay key for its operation id, and once
        // pushes may update, force and delete, the same command set can come
        // round again after the refs moved away. A replay therefore only
        // counts when this push's refs are already in place; otherwise the
        // push is enqueued once more under a fresh id and B3's CAS decides.
        let mut replayed_stale = false;
        let wait = loop {
            let old_id = mono
                .get_main_ref("/")
                .await?
                .map(|r| r.ref_commit_hash)
                .unwrap_or_else(|| ZERO_ID.to_owned());
            let wait = self
                .storage
                .push_queue_service
                .enqueue_and_wait(EnqueueRequest {
                    kind: crate::callisto::sea_orm_active_enums::PushQueueKindEnum::Attach,
                    operation_id: operation_id.clone(),
                    path: path.clone(),
                    old_id,
                    new_id: commit_id.clone(),
                    requester: None,
                    payload: payload_json.clone(),
                    ref_name: None,
                    is_delete: false,
                })
                .await?;
            match wait {
                QueueWaitResult::Replayed { id, .. }
                    if !self.attach_refs_applied(&payload).await? =>
                {
                    if replayed_stale {
                        return Err(MegaError::Other(format!(
                            "ImportRepo attach replayed push_queue id {id} but the refs did not move; retry the push"
                        )));
                    }
                    replayed_stale = true;
                    operation_id = attach_operation_id(
                        &format!("{}#{}", self.repo.repo_id, uuid::Uuid::new_v4()),
                        &fingerprint,
                    );
                }
                wait => break wait,
            }
        };

        match wait {
            // Refs already in effect, but a delete-only round replayed after a
            // detach also reads as applied: the repository must still be live.
            QueueWaitResult::Replayed { .. } => {
                if import_repo_is_live(&self.storage, self.repo.repo_id, &self.repo.repo_path)
                    .await?
                {
                    Ok(())
                } else {
                    Err(ImportRepoError::Removed {
                        path: self.repo.repo_path.clone(),
                    }
                    .into())
                }
            }
            QueueWaitResult::Abandoned { id } => Err(MegaError::Other(format!(
                "attach wait abandoned for push_queue id {id}"
            ))),
            QueueWaitResult::Rejected { id, message } => {
                Err(attach_refusal(&payload, id, &message))
            }
            QueueWaitResult::Ready { id } => {
                let ctx = AttachExecContext {
                    storage: self.storage.clone(),
                    git_object_cache: self.git_object_cache.clone(),
                };
                match self
                    .storage
                    .push_queue_service
                    .execute_b3(
                        ExecuteRequest {
                            id,
                            ..Default::default()
                        },
                        Some(&ctx),
                        None,
                        None,
                    )
                    .await?
                {
                    ExecuteOutcome::Done { .. } => Ok(()),
                    ExecuteOutcome::Failed { id, message, .. } => {
                        Err(attach_refusal(&payload, id, &message))
                    }
                    _ => Err(MegaError::Other(format!(
                        "ImportRepo attach for push_queue id {id} did not complete; retry the push"
                    ))),
                }
            }
        }
    }
}

/// Detach ImportRepo `repo_id` from `repo_path` through the write queue
/// (plan-20260923 ADR-FU-09 items 1 and 2) and return its cleanup id: the id
/// of the ledger row the detach wrote, or of the one an earlier detach of the
/// same repository wrote. `None` means there was nothing to detach and no
/// cleanup is pending. A repeat of a finished detach replays its row. There
/// is no product entry yet; FU-17's cleanup entry builds on this.
pub(crate) async fn detach_import_repo(
    storage: &Storage,
    git_object_cache: Arc<GitObjectCache>,
    repo_id: i64,
    repo_path: &str,
    requester: Option<String>,
) -> Result<Option<i64>, MegaError> {
    let path = crate::common::utils::canonicalize_mono_ref_path(repo_path)?;
    let payload = AttachPayload {
        repo_id,
        repo_path: path.clone(),
        commands: Vec::new(),
        op: AttachOp::Detach,
    };
    let old_id = storage
        .mono_storage()
        .get_main_ref("/")
        .await?
        .map(|r| r.ref_commit_hash)
        .unwrap_or_else(|| ZERO_ID.to_owned());
    let payload_json = serde_json::to_value(&payload)
        .map_err(|e| MegaError::Other(format!("detach payload encode: {e}")))?;
    // A `Done` row is a permanent replay key for its operation id, and a row
    // that never detached can carry it (a pre-FU-16 binary runs a detach row
    // as a delete-only attach). A replay therefore only counts once the
    // repository is gone; otherwise the detach is enqueued once more under a
    // salted id, as attach does for its replays.
    let mut operation_id = detach_operation_id(repo_id, &path);
    let mut replayed_live = false;
    let wait = loop {
        let wait = storage
            .push_queue_service
            .enqueue_and_wait(EnqueueRequest {
                kind: crate::callisto::sea_orm_active_enums::PushQueueKindEnum::Attach,
                operation_id: operation_id.clone(),
                path: path.clone(),
                old_id: old_id.clone(),
                new_id: ZERO_ID.to_owned(),
                requester: requester.clone(),
                payload: payload_json.clone(),
                ref_name: None,
                is_delete: false,
            })
            .await?;
        match wait {
            QueueWaitResult::Replayed { id, .. }
                if import_repo_is_live(storage, repo_id, &path).await? =>
            {
                if replayed_live {
                    return Err(MegaError::Other(format!(
                        "ImportRepo detach replayed push_queue id {id} but the repository is still registered; retry"
                    )));
                }
                replayed_live = true;
                operation_id =
                    detach_operation_id(repo_id, &format!("{path}#{}", uuid::Uuid::new_v4()));
            }
            wait => break wait,
        }
    };
    let id = match wait {
        QueueWaitResult::Replayed { id, .. } => {
            return resolve_cleanup_id(storage, id, repo_id, &path).await;
        }
        QueueWaitResult::Abandoned { id } => {
            return Err(MegaError::Other(format!(
                "ImportRepo detach wait abandoned for push_queue id {id}"
            )));
        }
        QueueWaitResult::Rejected { id, message } => {
            return Err(detach_refusal(&path, id, &message));
        }
        QueueWaitResult::Ready { id } => id,
    };
    let ctx = AttachExecContext {
        storage: storage.clone(),
        git_object_cache,
    };
    match storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id,
                ..Default::default()
            },
            Some(&ctx),
            None,
            None,
        )
        .await?
    {
        ExecuteOutcome::Done { id, .. } => resolve_cleanup_id(storage, id, repo_id, &path).await,
        ExecuteOutcome::Failed { id, message, .. } => Err(detach_refusal(&path, id, &message)),
        _ => Err(MegaError::Other(format!(
            "ImportRepo detach for push_queue id {id} did not complete; retry"
        ))),
    }
}

/// Outcome of one cleanup request (plan-20260923 ADR-FU-10 item 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// The ledger row is `swept`, by this request or an earlier one.
    Removed { repo_id: i64, cleanup_id: i64 },
    /// Detached, but the sweep ran out of this request's budget: resend with
    /// `cleanup_id`. Without a live repository it names the lowest row still
    /// `detached` on the path.
    Pending { repo_id: i64, cleanup_id: i64 },
    /// New operation only: no live repository at the path and nothing pending.
    Absent,
}

/// ImportRepo cleanup entry (plan-20260923 ADR-FU-09 items 4, 6, 7; ADR-FU-10
/// items 3, 5). One request handles at most `CLEANUP_RESUME_BATCH` ledger
/// rows and runs at most `SWEEP_STATEMENT_BUDGET` delete statements. A
/// `continuation` names one ledger row to finish and never detaches a live
/// repository. No product entry calls this yet (FU-20 / FU-21).
pub(crate) async fn remove_import_repo(
    storage: &Storage,
    git_object_cache: Arc<GitObjectCache>,
    canonical_path: &str,
    requester: Option<String>,
    continuation: Option<i64>,
) -> Result<RemoveOutcome, MegaError> {
    let path = crate::common::utils::canonicalize_mono_ref_path(canonical_path)?;
    let actor = requester.clone().unwrap_or_else(|| "anonymous".to_owned());
    let git_db = storage.git_db_storage();
    let conn = git_db.get_connection();
    let mut budget = SWEEP_STATEMENT_BUDGET;

    if let Some(cleanup_id) = continuation {
        // Only the ledger is consulted: a re-import at the same path has a
        // new repo_id and is never touched.
        let row = match git_db.cleanup_by_id(cleanup_id, conn).await? {
            Some(row) if row.path == path => row,
            _ => {
                return Err(ImportRepoError::CleanupNotFound {
                    path,
                    cleanup_id: cleanup_id.to_string(),
                }
                .into());
            }
        };
        return Ok(
            match sweep_ledger_row(&git_db, &row, &actor, &mut budget).await? {
                RowSweep::Swept => RemoveOutcome::Removed {
                    repo_id: row.repo_id,
                    cleanup_id: row.id,
                },
                RowSweep::OverBudget => RemoveOutcome::Pending {
                    repo_id: row.repo_id,
                    cleanup_id: row.id,
                },
            },
        );
    }

    if let Some(live) = git_db.find_git_repo_exact_match(&path).await? {
        // Write-free pre-check: nothing is enqueued for a parent with
        // children. B3 re-checks under its own lock and stays the authority.
        if git_db
            .import_repo_has_children(live.id, &path, conn)
            .await?
        {
            return Err(ImportRepoError::HasChildren { path }.into());
        }
        if let Some(cleanup_id) =
            detach_import_repo(storage, git_object_cache, live.id, &path, requester).await?
        {
            let row = git_db
                .cleanup_by_id(cleanup_id, conn)
                .await?
                .ok_or_else(|| {
                    MegaError::Other(format!(
                        "cleanup ledger row {cleanup_id} missing after detach"
                    ))
                })?;
            let mut handled = 0;
            if row.state == CleanupState::Detached {
                handled = 1;
                if sweep_ledger_row(&git_db, &row, &actor, &mut budget).await?
                    == RowSweep::OverBudget
                {
                    return Ok(RemoveOutcome::Pending {
                        repo_id: row.repo_id,
                        cleanup_id: row.id,
                    });
                }
            }
            // The request's own row is swept; what is left of the budget goes
            // to older rows of the path, whose fate does not change the answer.
            if let Resume::Remaining(next) =
                resume_pending_for_path(&git_db, &path, &actor, &mut budget, handled).await?
            {
                tracing::info!(
                    path = %path,
                    cleanup_id = next.id,
                    "older ImportRepo cleanups remain; the next request resumes them"
                );
            }
            return Ok(RemoveOutcome::Removed {
                repo_id: row.repo_id,
                cleanup_id: row.id,
            });
        }
        // Nothing was detached (the repository was gone when B3 locked its
        // row, or the round replayed one that detached nothing): the ledger
        // decides below.
    }

    Ok(
        match resume_pending_for_path(&git_db, &path, &actor, &mut budget, 0).await? {
            Resume::AllSwept => RemoveOutcome::Absent,
            Resume::Remaining(row) => RemoveOutcome::Pending {
                repo_id: row.repo_id,
                cleanup_id: row.id,
            },
        },
    )
}

#[derive(Debug, PartialEq, Eq)]
enum RowSweep {
    Swept,
    OverBudget,
}

/// Sweep one ledger row within what is left of `budget`; on completion the
/// row turns `swept` together with its `phase: "swept"` audit row, in one
/// short transaction outside the write lock. The row counts are evidence
/// only: two requests sweeping the same row, or a crash between the deletes
/// and the progress write, can leave them under- or over-counted; the state
/// transition never depends on them.
async fn sweep_ledger_row(
    git_db: &GitDbStorage,
    row: &import_repo_cleanups::Model,
    actor: &str,
    budget: &mut u32,
) -> Result<RowSweep, MegaError> {
    if row.state == CleanupState::Swept {
        return Ok(RowSweep::Swept);
    }
    let conn = git_db.get_connection();
    let report = git_db
        .sweep_import_repo_objects(row.repo_id, *budget, conn)
        .await?;
    *budget = budget.saturating_sub(report.statements);
    let total = SweepCounts::from_json(row.rows_deleted.as_ref()).saturating_add(report.deleted);
    if !report.complete {
        // Another request may have finished this row meanwhile; a `pending`
        // answer must never name a swept row.
        if git_db
            .cleanup_by_id(row.id, conn)
            .await?
            .is_some_and(|now| now.state == CleanupState::Swept)
        {
            return Ok(RowSweep::Swept);
        }
        if report.statements > 0 {
            git_db
                .record_cleanup_progress(row.id, total.to_json(), conn)
                .await?;
        }
        return Ok(RowSweep::OverBudget);
    }
    let txn = conn.begin().await?;
    if git_db
        .mark_cleanup_swept(row.id, total.to_json(), &txn)
        .await?
    {
        AuditStorage::log_audit_in_txn(
            &txn,
            // Reserved actor: storage-only has no numeric user id.
            0,
            ActorTypeEnum::Human,
            AuditActionEnum::Delete,
            TargetTypeEnum::Repository,
            row.repo_id,
            Some(serde_json::json!({
                "kind": IMPORT_REPO_REMOVE_KIND,
                "cleanup_id": row.id,
                "path": row.path,
                "requester": actor,
                "phase": "swept",
                "rows_deleted": total.to_json(),
            })),
        )
        .await?;
    } else {
        tracing::debug!(
            cleanup_id = row.id,
            "cleanup already marked swept by another request"
        );
    }
    txn.commit().await?;
    Ok(RowSweep::Swept)
}

enum Resume {
    AllSwept,
    Remaining(import_repo_cleanups::Model),
}

/// Sweep the oldest `detached` ledger rows of `path`, at most
/// `CLEANUP_RESUME_BATCH - handled` of them, in index order. `Remaining`
/// names the lowest row still `detached` afterwards.
async fn resume_pending_for_path(
    git_db: &GitDbStorage,
    path: &str,
    actor: &str,
    budget: &mut u32,
    handled: usize,
) -> Result<Resume, MegaError> {
    let conn = git_db.get_connection();
    let room = (CLEANUP_RESUME_BATCH as usize).saturating_sub(handled);
    let page = git_db.pending_cleanups_by_path(path, conn).await?;
    for row in page.iter().take(room) {
        if sweep_ledger_row(git_db, row, actor, budget).await? == RowSweep::OverBudget {
            return Ok(Resume::Remaining(row.clone()));
        }
    }
    if page.len() > room {
        return Ok(Resume::Remaining(page[room].clone()));
    }
    if page.len() == CLEANUP_RESUME_BATCH as usize {
        // A full page proves nothing about the rest: one more bounded read.
        return Ok(
            match git_db
                .pending_cleanups_by_path(path, conn)
                .await?
                .into_iter()
                .next()
            {
                Some(next) => Resume::Remaining(next),
                None => Resume::AllSwept,
            },
        );
    }
    Ok(Resume::AllSwept)
}

/// The cleanup id behind a finished detach round `id`: its own ledger row, or
/// else the latest ledger row an earlier detach of `repo_id` at `path` wrote
/// (a `Done` row without a ledger row detached nothing: the repository was
/// already gone, or the row predates FU-16).
async fn resolve_cleanup_id(
    storage: &Storage,
    id: i64,
    repo_id: i64,
    path: &str,
) -> Result<Option<i64>, MegaError> {
    let git_db = storage.git_db_storage();
    let conn = git_db.get_connection();
    if git_db.cleanup_by_id(id, conn).await?.is_some() {
        return Ok(Some(id));
    }
    Ok(git_db
        .latest_cleanup_for(repo_id, path, conn)
        .await?
        .map(|row| row.id))
}

/// Whether `repo_id` is still registered at `path` (exact canonical lookup).
async fn import_repo_is_live(
    storage: &Storage,
    repo_id: i64,
    path: &str,
) -> Result<bool, MegaError> {
    Ok(storage
        .git_db_storage()
        .find_git_repo_exact_match(path)
        .await?
        .is_some_and(|row| row.id == repo_id))
}

/// Client text for a refused detach round: the typed `HAS_CHILDREN` error,
/// or a fixed sentence (the reason stays in the queue row).
fn detach_refusal(path: &str, id: i64, message: &str) -> MegaError {
    let children = ImportRepoError::HasChildren {
        path: path.to_owned(),
    };
    if children.to_string() == message {
        return children.into();
    }
    MegaError::Other(format!(
        "ImportRepo detach failed (push_queue id {id}); retry"
    ))
}

/// Integration-test hook (plan-20260923 FU-16): detach the ImportRepo at
/// `repo_path` through the write queue from a process other than the service,
/// using `config` for the service's database, Redis and object store. No
/// product entry calls it.
#[doc(hidden)]
pub async fn detach_for_integration_test(
    config: crate::config::Config,
    repo_path: &str,
) -> Result<Option<i64>, MegaError> {
    let config = Arc::new(config);
    let connection = crate::jupiter::redis::init_connection(&config.redis).await?;
    let db = crate::jupiter::storage::init::database_connection(&config.database).await?;
    let object_store =
        crate::jupiter::storage::object_storage::build_object_storage(&config.object_storage)
            .await?;
    let storage = Storage::new_with_connection(config, Arc::new(db), object_store).await?;
    let path = crate::common::utils::canonicalize_mono_ref_path(repo_path)?;
    let repo = storage
        .git_db_storage()
        .find_git_repo_exact_match(&path)
        .await?
        .ok_or_else(|| MegaError::Other(format!("no ImportRepo at {path:?}")))?;
    let cache = Arc::new(GitObjectCache {
        connection,
        prefix: "git-object-rkyv:v1".to_owned(),
    });
    detach_import_repo(&storage, cache, repo.id, &path, None).await
}

/// Client text for a refused attach round (plan-20260923 ADR-FU-08 item 5).
/// The queue carries the refusal as text, so return the typed
/// `ImportRepoError` whose text it is (the leaf path or one of this push's
/// branch commands). The materialization precheck keeps its I3 detail (it
/// names monorepo paths only). Anything else can carry storage internals, so
/// the client gets a fixed sentence; the reason stays in the queue row.
fn attach_refusal(payload: &AttachPayload, id: i64, message: &str) -> MegaError {
    let occupied = ImportRepoError::PathOccupied {
        path: payload.repo_path.clone(),
    };
    let removed = ImportRepoError::Removed {
        path: payload.repo_path.clone(),
    };
    let stale = payload.commands.iter().map(|c| ImportRepoError::StaleRef {
        ref_name: c.ref_name.clone(),
        expected: c.old_id.clone(),
    });
    if let Some(typed) = [occupied, removed]
        .into_iter()
        .chain(stale)
        .find(|candidate| candidate.to_string() == message)
    {
        return typed.into();
    }
    if let Some(detail) = message
        .strip_prefix("Other error: ")
        .filter(|detail| detail.starts_with("attach refused: ") && detail.ends_with(" (I3)"))
    {
        return MegaError::Other(format!("ImportRepo attach failed: {detail}"));
    }
    // The reason stays in `push_queue.error_message`; not logged here.
    tracing::warn!(
        id,
        "ImportRepo attach refused without a client-facing reason"
    );
    MegaError::Other(format!(
        "ImportRepo attach failed (push_queue id {id}); retry the push"
    ))
}

async fn process_objects(
    repo_id: i64,
    git_service: GitService,
    storage: GitDbStorage,
    entry_tx: Sender<MetaAttached<Entry, EntryMeta>>,
) -> Result<(), MegaError> {
    let mut commit_stream = storage.get_commits_by_repo_id(repo_id).await?;

    while let Some(model) = commit_stream.next().await {
        match model {
            Ok(m) => {
                let c: Commit = Commit::from_git_model(m);
                let entry = MetaAttached {
                    inner: c.into(),
                    meta: EntryMeta::new(),
                };
                entry_tx.send(entry).await.expect("send error");
            }
            Err(err) => eprintln!("Error: {err:?}"),
        }
    }
    tracing::info!("send commits end");

    let mut tree_stream = storage.get_trees_by_repo_id(repo_id).await?;
    while let Some(model) = tree_stream.next().await {
        match model {
            Ok(m) => {
                let t: Tree = Tree::from_git_model(m);
                let entry = MetaAttached {
                    inner: t.into(),
                    meta: EntryMeta::new(),
                };
                entry_tx.send(entry).await.expect("send error");
            }
            Err(err) => eprintln!("Error: {err:?}"),
        }
    }
    tracing::info!("send trees end");

    let mut bid_stream = storage.get_blobs_by_repo_id(repo_id).await?;
    let mut bids = vec![];
    while let Some(model) = bid_stream.next().await {
        match model {
            Ok(m) => bids.push(m.blob_id),
            Err(err) => eprintln!("Error: {err:?}"),
        }
    }

    let entry_tx = entry_tx.clone();
    git_service
        .get_objects_stream(bids)
        .try_for_each_concurrent(16, |(_, stream, _)| {
            let sender_clone = entry_tx.clone();
            async move {
                let data = stream
                    .try_fold(Vec::new(), |mut acc, bytes| async move {
                        acc.extend_from_slice(&bytes);
                        Ok(acc)
                    })
                    .await?;
                let blob = Blob::from_content_bytes(data);
                sender_clone
                    .send(MetaAttached {
                        inner: blob.into(),
                        meta: EntryMeta::default(),
                    })
                    .await
                    .expect("send error");

                Ok(())
            }
        })
        .await?;

    tracing::info!("send blobs end");

    let tags = storage.get_tags_by_repo_id(repo_id).await?;
    for m in tags.into_iter() {
        let c: Tag = Tag::from_git_model(m);
        let entry = MetaAttached {
            inner: c.into(),
            meta: EntryMeta::new(),
        };
        entry_tx.send(entry).await.expect("send error");
    }
    tracing::info!("sending all object end...");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use git_internal::internal::{
        metadata::{EntryMeta, MetaAttached},
        object::{
            ObjectTrait,
            blob::Blob,
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };
    use sea_orm::{
        ColumnTrait, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter, TransactionTrait,
    };

    use super::{
        ImportRepo, RemoveOutcome, RowSweep, SWEEP_STATEMENT_BUDGET, collect_git_blob_filepaths,
        detach_import_repo, remove_import_repo, sweep_ledger_row,
    };
    use crate::{
        callisto::{
            git_blob, git_tree, import_refs, push_queue, queue_control,
            sea_orm_active_enums::{PushQueueKindEnum, RefTypeEnum},
        },
        ceres::{
            api_service::cache::GitObjectCache,
            pack::RepoHandler,
            protocol::{
                import_refs::{CommandType, RefCommand},
                repo::Repo,
            },
        },
        common::{
            errors::{ImportRepoError, MegaError},
            utils::{ZERO_ID, generate_id},
        },
        config::RedisConfig,
        jupiter::{
            migration::apply_migrations,
            redis::init_connection,
            service::{
                git_service::GitService,
                import_service::ImportService,
                mono_service::MonoService,
                push_queue_service::{
                    AttachCommand, AttachExecContext, AttachOp, AttachPayload, EnqueueRequest,
                    ExecuteOutcome, ExecuteRequest, QueueWaitResult, attach_operation_id,
                    normalize_attach_commands,
                },
            },
            storage::{
                Storage,
                base_storage::{BaseStorage, StorageConnector},
                git_db_storage::GitDbStorage,
                object_storage::mock_object_storage,
                push_queue_storage::EnqueueOutcome,
            },
            tests::{test_db_connection, test_storage},
            utils::converter::{FromGitModel, FromMegaModel},
        },
    };

    async fn wired_storage_with_monorepo(temp: &tempfile::TempDir) -> Storage {
        let mut storage = test_storage(temp.path()).await;
        let git_service = GitService {
            obj_storage: mock_object_storage(),
        };
        storage.git_service = git_service.clone();
        storage.mono_service = MonoService {
            mono_storage: storage.mono_storage(),
            git_service: git_service.clone(),
        };
        storage.import_service = ImportService {
            git_db_storage: storage.git_db_storage(),
            git_service,
        };
        storage
            .mono_service
            .init_monorepo(&storage.config().monorepo)
            .await
            .unwrap();
        storage
    }

    async fn seed_import_repo_with_main_tip(
        storage: &Storage,
        path: &str,
    ) -> (Repo, Commit, RefCommand) {
        seed_import_repo_with_tip_message(storage, path, "\nimport commit").await
    }

    async fn seed_import_repo_with_tip_message(
        storage: &Storage,
        path: &str,
        message: &str,
    ) -> (Repo, Commit, RefCommand) {
        let repo = Repo::new(PathBuf::from(path), false).unwrap();
        let repo_id = repo.repo_id;
        let readme = Blob::from_content("hello from import repo");
        let tree = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: readme.id,
            name: "README.md".to_string(),
        }])
        .unwrap();
        let commit = Commit::from_tree_id(tree.id, vec![], message);
        storage
            .git_db_storage()
            .register_import_repo(repo.clone().into())
            .await
            .unwrap();
        storage
            .import_service
            .save_entry(
                repo_id,
                &repo.repo_path,
                vec![
                    MetaAttached {
                        inner: readme.into(),
                        meta: EntryMeta::new(),
                    },
                    MetaAttached {
                        inner: tree.into(),
                        meta: EntryMeta::new(),
                    },
                    MetaAttached {
                        inner: commit.clone().into(),
                        meta: EntryMeta::new(),
                    },
                ],
            )
            .await
            .unwrap();
        let mut command = RefCommand::new(
            ZERO_ID.to_string(),
            commit.id.to_string(),
            "refs/heads/main".to_string(),
        );
        // Mirror receive-pack: first branch becomes the default (smart.rs).
        command.default_branch = true;
        (repo, commit, command)
    }

    async fn disabled_cache() -> Arc<GitObjectCache> {
        let redis_url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
        let connection = init_connection(&RedisConfig { url: redis_url })
            .await
            .expect("redis connection");
        Arc::new(GitObjectCache {
            connection,
            prefix: "disabled".to_string(),
        })
    }

    #[test]
    pub fn test_recurse_tree() {
        let path = PathBuf::from("/third-party/crates/tokio/tokio-console");
        let ancestors: Vec<_> = path.ancestors().collect();
        for path in ancestors.into_iter() {
            println!("{path:?}");
        }
    }

    #[tokio::test]
    async fn collect_and_batch_update_nested_crate_tree() {
        let dir = tempfile::TempDir::new().unwrap();
        let storage = test_storage(dir.path()).await;
        let stg = storage.git_db_storage();
        let repo_id = 11i64;

        let cargo = Blob::from_content("[package]\nname = \"demo\"\n");
        let lib = Blob::from_content("pub fn f() {}\n");
        let src_tree = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: lib.id,
            name: "lib.rs".into(),
        }])
        .unwrap();
        let root_tree = Tree::from_tree_items(vec![
            TreeItem {
                mode: TreeItemMode::Blob,
                id: cargo.id,
                name: "Cargo.toml".into(),
            },
            TreeItem {
                mode: TreeItemMode::Tree,
                id: src_tree.id,
                name: "src".into(),
            },
        ])
        .unwrap();

        let now = chrono::Utc::now().naive_utc();
        git_blob::Entity::insert_many([
            git_blob::Model {
                id: generate_id(),
                repo_id,
                blob_id: cargo.id.to_string(),
                name: None,
                size: 0,
                created_at: now,
                pack_id: String::new(),
                file_path: String::new(),
                pack_offset: 0,
                is_delta_in_pack: false,
            }
            .into_active_model(),
            git_blob::Model {
                id: generate_id(),
                repo_id,
                blob_id: lib.id.to_string(),
                name: None,
                size: 0,
                created_at: now,
                pack_id: String::new(),
                file_path: String::new(),
                pack_offset: 0,
                is_delta_in_pack: false,
            }
            .into_active_model(),
        ])
        .exec(stg.get_connection())
        .await
        .unwrap();

        git_tree::Entity::insert_many([
            git_tree::Model {
                id: generate_id(),
                repo_id,
                tree_id: src_tree.id.to_string(),
                sub_trees: src_tree.to_data().unwrap(),
                size: 0,
                created_at: now,
                pack_id: String::new(),
                pack_offset: 0,
            }
            .into_active_model(),
            git_tree::Model {
                id: generate_id(),
                repo_id,
                tree_id: root_tree.id.to_string(),
                sub_trees: root_tree.to_data().unwrap(),
                size: 0,
                created_at: now,
                pack_id: String::new(),
                pack_offset: 0,
            }
            .into_active_model(),
        ])
        .exec(stg.get_connection())
        .await
        .unwrap();

        let loaded_root = Tree::from_git_model(
            stg.get_tree_by_hash(repo_id, &root_tree.id.to_string())
                .await
                .unwrap()
                .unwrap(),
        );
        let mut pairs =
            collect_git_blob_filepaths(stg.clone(), repo_id, loaded_root, PathBuf::new())
                .await
                .unwrap();
        pairs.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(
            pairs,
            vec![
                (cargo.id.to_string(), "Cargo.toml".into()),
                (lib.id.to_string(), "src/lib.rs".into()),
            ]
        );

        stg.update_git_blob_filepaths(repo_id, pairs).await.unwrap();

        let cargo_row = git_blob::Entity::find()
            .filter(git_blob::Column::RepoId.eq(repo_id))
            .filter(git_blob::Column::BlobId.eq(cargo.id.to_string()))
            .one(stg.get_connection())
            .await
            .unwrap()
            .unwrap();
        let lib_row = git_blob::Entity::find()
            .filter(git_blob::Column::RepoId.eq(repo_id))
            .filter(git_blob::Column::BlobId.eq(lib.id.to_string()))
            .one(stg.get_connection())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cargo_row.file_path, "Cargo.toml");
        assert_eq!(lib_row.file_path, "src/lib.rs");
    }

    #[tokio::test]
    pub async fn delete_only_branch_ref_persistence_removes_ref_row() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let db = Arc::new(db);
        let base = BaseStorage::new(db.clone());
        let git_db = GitDbStorage { base: base.clone() };
        let repo_id = 1i64;

        let seed = import_refs::Model {
            id: 1,
            repo_id,
            ref_name: String::from("refs/heads/main"),
            ref_git_id: String::from("27dd8d4cf39f3868c6eee38b601bc9e9939304f5"),
            ref_type: RefTypeEnum::Branch,
            default_branch: true,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        };
        git_db.save_ref(repo_id, seed).await.unwrap();
        assert_eq!(git_db.get_ref(repo_id).await.unwrap().len(), 1);

        let txn = db.begin().await.unwrap();
        git_db
            .remove_ref_in_txn(repo_id, "refs/heads/main", &txn)
            .await
            .unwrap();
        txn.commit().await.unwrap();

        assert!(git_db.get_ref(repo_id).await.unwrap().is_empty());
    }

    /// A delete-only batch goes through MonoWriteQueue like any other branch
    /// batch (GC-FU-03) and never moves the monorepo root.
    #[tokio::test]
    async fn fu13_delete_only_applies_through_queue() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo, _commit, create_cmd) =
            seed_import_repo_with_main_tip(&storage, "/third-party/delonly").await;
        let repo_id = repo.repo_id;
        storage
            .git_db_storage()
            .save_ref(repo_id, create_cmd.clone().into())
            .await
            .unwrap();

        let mut delete_cmd = create_cmd.clone();
        delete_cmd.command_type = CommandType::Delete;
        delete_cmd.old_id = create_cmd.new_id.clone();
        delete_cmd.new_id = ZERO_ID.to_string();

        let root_before = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let import_repo = ImportRepo {
            storage: storage.clone(),
            repo,
            command_list: Mutex::new(vec![delete_cmd]),
            git_object_cache: disabled_cache().await,
            receive_pack_extra_timings_ms: Mutex::new(vec![]),
        };
        import_repo.attach_to_monorepo_parent().await.unwrap();

        assert!(
            storage
                .git_db_storage()
                .get_ref(repo_id)
                .await
                .unwrap()
                .is_empty(),
            "delete-only attach must remove the import ref"
        );
        let rows = push_queue::Entity::find()
            .all(storage.git_db_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "delete-only attach is one queue round");
        assert_eq!(
            rows[0].status,
            crate::callisto::sea_orm_active_enums::PushQueueStatusEnum::Done
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

    /// FU-12: an ImportRepo whose tag `refs/tags/v1` points at `"1" * 40`.
    async fn fu12_tag_repo(path: &str) -> (tempfile::TempDir, Storage, ImportRepo) {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo, _commit, _create) = seed_import_repo_with_main_tip(&storage, path).await;
        let mut tag = RefCommand::new(ZERO_ID.to_string(), "1".repeat(40), "refs/tags/v1".into());
        tag.ref_type = RefTypeEnum::Tag;
        storage
            .git_db_storage()
            .save_ref(repo.repo_id, tag.into())
            .await
            .unwrap();
        let import_repo = ImportRepo {
            storage: storage.clone(),
            repo,
            command_list: Mutex::new(vec![]),
            git_object_cache: disabled_cache().await,
            receive_pack_extra_timings_ms: Mutex::new(vec![]),
        };
        (temp, storage, import_repo)
    }

    fn fu12_tag_cmd(
        name: &str,
        command_type: CommandType,
        old_id: &str,
        new_id: &str,
    ) -> RefCommand {
        let mut cmd = RefCommand::new(old_id.to_owned(), new_id.to_owned(), name.to_owned());
        cmd.ref_type = RefTypeEnum::Tag;
        cmd.command_type = command_type;
        cmd
    }

    async fn fu12_tag_id(storage: &Storage, repo_id: i64, name: &str) -> Option<String> {
        storage
            .git_db_storage()
            .get_ref(repo_id)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.ref_name == name)
            .map(|r| r.ref_git_id)
    }

    #[tokio::test]
    async fn fu12_tag_update_stale() {
        let (_temp, storage, repo) = fu12_tag_repo("/third-party/fu12-tag-update").await;
        let stale = fu12_tag_cmd(
            "refs/tags/v1",
            CommandType::Update,
            &"2".repeat(40),
            &"3".repeat(40),
        );
        let err = repo
            .update_refs(&stale)
            .await
            .expect_err("stale tag update");
        assert!(
            err.to_string().starts_with(
                "IMPORT_REPO_STALE_REF: \"refs/tags/v1\" changed since it was advertised"
            ),
            "{err}"
        );
        let id = repo.repo.repo_id;
        assert_eq!(
            fu12_tag_id(&storage, id, "refs/tags/v1").await,
            Some("1".repeat(40))
        );
        // The matching update applies.
        let ok = fu12_tag_cmd(
            "refs/tags/v1",
            CommandType::Update,
            &"1".repeat(40),
            &"3".repeat(40),
        );
        repo.update_refs(&ok).await.expect("matching tag update");
        assert_eq!(
            fu12_tag_id(&storage, id, "refs/tags/v1").await,
            Some("3".repeat(40))
        );
    }

    #[tokio::test]
    async fn fu12_tag_create_existing() {
        let (_temp, storage, repo) = fu12_tag_repo("/third-party/fu12-tag-create").await;
        let existing = fu12_tag_cmd(
            "refs/tags/v1",
            CommandType::Create,
            ZERO_ID,
            &"3".repeat(40),
        );
        let err = repo
            .update_refs(&existing)
            .await
            .expect_err("tag already exists");
        assert!(
            err.to_string()
                .starts_with("IMPORT_REPO_STALE_REF: \"refs/tags/v1\" already exists"),
            "{err}"
        );
        let id = repo.repo.repo_id;
        assert_eq!(
            fu12_tag_id(&storage, id, "refs/tags/v1").await,
            Some("1".repeat(40))
        );
        // A new tag name is created.
        let fresh = fu12_tag_cmd(
            "refs/tags/v2",
            CommandType::Create,
            ZERO_ID,
            &"3".repeat(40),
        );
        repo.update_refs(&fresh).await.expect("new tag");
        assert_eq!(
            fu12_tag_id(&storage, id, "refs/tags/v2").await,
            Some("3".repeat(40))
        );
    }

    #[tokio::test]
    async fn fu12_tag_delete_stale() {
        let (_temp, storage, repo) = fu12_tag_repo("/third-party/fu12-tag-delete").await;
        let stale = fu12_tag_cmd(
            "refs/tags/v1",
            CommandType::Delete,
            &"2".repeat(40),
            ZERO_ID,
        );
        let err = repo
            .update_refs(&stale)
            .await
            .expect_err("stale tag delete");
        assert!(
            err.to_string().starts_with("IMPORT_REPO_STALE_REF: "),
            "{err}"
        );
        let id = repo.repo.repo_id;
        assert_eq!(
            fu12_tag_id(&storage, id, "refs/tags/v1").await,
            Some("1".repeat(40))
        );
        // The matching delete applies.
        let ok = fu12_tag_cmd(
            "refs/tags/v1",
            CommandType::Delete,
            &"1".repeat(40),
            ZERO_ID,
        );
        repo.update_refs(&ok).await.expect("matching tag delete");
        assert_eq!(fu12_tag_id(&storage, id, "refs/tags/v1").await, None);
    }

    #[tokio::test]
    async fn fu12_delete_only_keeps_a_default_branch() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo, commit, main) =
            seed_import_repo_with_main_tip(&storage, "/third-party/fu12-del-default").await;
        let repo_id = repo.repo_id;
        let git_db = storage.git_db_storage();
        git_db.save_ref(repo_id, main.clone().into()).await.unwrap();
        let mut dev = RefCommand::new(
            ZERO_ID.to_string(),
            commit.id.to_string(),
            "refs/heads/dev".into(),
        );
        dev.default_branch = false;
        git_db.save_ref(repo_id, dev.into()).await.unwrap();

        let mut delete_main = main.clone();
        delete_main.command_type = CommandType::Delete;
        delete_main.old_id = main.new_id.clone();
        delete_main.new_id = ZERO_ID.to_string();
        let import_repo = ImportRepo {
            storage: storage.clone(),
            repo,
            command_list: Mutex::new(vec![delete_main]),
            git_object_cache: disabled_cache().await,
            receive_pack_extra_timings_ms: Mutex::new(vec![]),
        };
        import_repo.attach_to_monorepo_parent().await.unwrap();

        let refs = git_db.get_ref(repo_id).await.unwrap();
        assert_eq!(refs.len(), 1, "{refs:?}");
        assert_eq!(refs[0].ref_name, "refs/heads/dev");
        assert!(
            refs[0].default_branch,
            "the remaining branch becomes the default"
        );
    }

    /// FU-13: push `commands` to `repo` through the real attach path.
    async fn fu13_push(
        storage: &Storage,
        repo: &Repo,
        commands: Vec<RefCommand>,
    ) -> Result<(), String> {
        let import_repo = ImportRepo {
            storage: storage.clone(),
            repo: repo.clone(),
            command_list: Mutex::new(commands),
            git_object_cache: disabled_cache().await,
            receive_pack_extra_timings_ms: Mutex::new(vec![]),
        };
        import_repo
            .attach_to_monorepo_parent()
            .await
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn fu13_replayed_attach_rechecks_refs() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo, c1, create) =
            seed_import_repo_with_main_tip(&storage, "/third-party/fu13-replay").await;
        let c2 = Commit::from_tree_id(c1.tree_id, vec![c1.id], "fu13 replay second");
        storage
            .import_service
            .save_entry(
                repo.repo_id,
                &repo.repo_path,
                vec![MetaAttached {
                    inner: c2.clone().into(),
                    meta: EntryMeta::new(),
                }],
            )
            .await
            .unwrap();
        let (c1_id, c2_id) = (c1.id.to_string(), c2.id.to_string());
        let main = |old: &str, new: &str| {
            let mut cmd = RefCommand::new(old.to_owned(), new.to_owned(), "refs/heads/main".into());
            cmd.command_type = CommandType::Update;
            cmd
        };
        fu13_push(&storage, &repo, vec![create])
            .await
            .expect("first push");
        fu13_push(&storage, &repo, vec![main(&c1_id, &c2_id)])
            .await
            .expect("c1 → c2");
        fu13_push(&storage, &repo, vec![main(&c2_id, &c1_id)])
            .await
            .expect("force back to c1");
        // Same commands as the second push: its Done row is replayed, but the
        // ref is not at c2 any more, so the push must be applied again.
        fu13_push(&storage, &repo, vec![main(&c1_id, &c2_id)])
            .await
            .expect("c1 → c2 again");
        let refs = storage
            .git_db_storage()
            .get_ref(repo.repo_id)
            .await
            .unwrap();
        let tip = refs
            .iter()
            .find(|r| r.ref_name == "refs/heads/main")
            .map(|r| r.ref_git_id.clone());
        assert_eq!(tip, Some(c2_id.clone()));
        // A genuine retry (refs already in place) is still an idempotent replay.
        fu13_push(&storage, &repo, vec![main(&c1_id, &c2_id)])
            .await
            .expect("idempotent retry");
        let queued = push_queue::Entity::find()
            .filter(push_queue::Column::Path.eq(repo.repo_path.clone()))
            .count(storage.git_db_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(queued, 4, "the idempotent retry adds no row");
    }

    #[test]
    fn fu13_attach_refusal_rebuilds_typed_errors() {
        use crate::common::errors::{ImportRepoError, MegaError};
        let payload = AttachPayload {
            op: AttachOp::Attach,
            repo_id: 1,
            repo_path: "/third-party/x".to_owned(),
            commands: vec![AttachCommand {
                ref_name: "refs/heads/dev".to_owned(),
                old_id: "3".repeat(40),
                new_id: "4".repeat(40),
                command_type: "Update".to_owned(),
                ref_type: "branch".to_owned(),
                default_branch: false,
            }],
        };
        let occupied = ImportRepoError::PathOccupied {
            path: "/third-party/x".to_owned(),
        };
        let err = super::attach_refusal(&payload, 9, &occupied.to_string());
        assert!(
            matches!(
                err,
                MegaError::ImportRepo(ImportRepoError::PathOccupied { .. })
            ),
            "{err}"
        );
        let stale = ImportRepoError::StaleRef {
            ref_name: "refs/heads/dev".to_owned(),
            expected: "3".repeat(40),
        };
        let err = super::attach_refusal(&payload, 9, &stale.to_string());
        assert_eq!(err.to_string(), stale.to_string());
        // The materialization precheck keeps its I3 detail.
        let precheck = "Other error: attach refused: ancestor '/third-party' already has a materialized main ref (I3)";
        let err = super::attach_refusal(&payload, 9, precheck);
        assert_eq!(
            err.to_string(),
            "Other error: ImportRepo attach failed: attach refused: ancestor '/third-party' already has a materialized main ref (I3)"
        );
        // Anything else never reaches the client verbatim.
        let internal =
            "Database error: connection to postgres://mega2:s3cret@10.0.0.5/mega2 failed";
        let err = super::attach_refusal(&payload, 9, internal).to_string();
        assert_eq!(
            err,
            "Other error: ImportRepo attach failed (push_queue id 9); retry the push"
        );
        assert!(
            !err.contains("s3cret") && !err.contains("postgres://"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn attach_delete_stale_old_id_does_not_remove_moved_ref() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo, _commit, create_cmd) =
            seed_import_repo_with_main_tip(&storage, "/third-party/cas-del").await;
        let repo_id = repo.repo_id;
        let git_db = storage.git_db_storage();
        git_db
            .save_ref(repo_id, create_cmd.clone().into())
            .await
            .unwrap();
        let moved = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        git_db
            .update_ref(repo_id, &create_cmd.ref_name, moved)
            .await
            .unwrap();

        let mut delete_cmd = create_cmd.clone();
        delete_cmd.command_type = CommandType::Delete;
        delete_cmd.old_id = create_cmd.new_id.clone();
        delete_cmd.new_id = ZERO_ID.to_string();

        let import_repo = ImportRepo {
            storage: storage.clone(),
            repo,
            command_list: Mutex::new(vec![delete_cmd]),
            git_object_cache: disabled_cache().await,
            receive_pack_extra_timings_ms: Mutex::new(vec![]),
        };
        let err = import_repo
            .attach_to_monorepo_parent()
            .await
            .expect_err("stale advertised old id must conflict");
        assert!(
            matches!(
                &err,
                crate::common::errors::MegaError::ImportRepo(
                    crate::common::errors::ImportRepoError::StaleRef { .. }
                )
            ),
            "got {err}"
        );
        assert!(
            err.to_string().starts_with("IMPORT_REPO_STALE_REF: "),
            "got {err}"
        );
        let refs = git_db.get_ref(repo_id).await.unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].ref_git_id, moved);
    }

    #[tokio::test]
    async fn attach_materialization_precheck_rejects_target_ancestor_descendant() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let mono = storage.mono_storage();

        // Root attach is allowed (no descendants).
        mono.attach_materialization_precheck("/").await.unwrap();

        let ancestor = crate::callisto::mega_refs::Model::new(
            "/third-party",
            crate::common::utils::MEGA_BRANCH_NAME.to_owned(),
            "a".repeat(40),
            "b".repeat(40),
            false,
        );
        mono.save_refs(ancestor, None).await.unwrap();
        let err = mono
            .attach_materialization_precheck("/third-party/newrepo")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ancestor") && err.contains("I3"),
            "expected ancestor refusal, got {err}"
        );

        mono.save_refs(
            crate::callisto::mega_refs::Model::new(
                "/leaf",
                crate::common::utils::MEGA_BRANCH_NAME.to_owned(),
                "c".repeat(40),
                "d".repeat(40),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let err = mono
            .attach_materialization_precheck("/leaf")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already has a materialized main ref") && err.contains("I3"),
            "expected target refusal, got {err}"
        );

        mono.save_refs(
            crate::callisto::mega_refs::Model::new(
                "/parent/child",
                crate::common::utils::MEGA_BRANCH_NAME.to_owned(),
                "e".repeat(40),
                "f".repeat(40),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let err = mono
            .attach_materialization_precheck("/parent")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("descendant") && err.contains("I3"),
            "expected descendant refusal, got {err}"
        );
    }

    /// Regression for mega@f5d22b9 (#2152): attach must persist the placeholder
    /// `.gitkeep` blob into object storage before the txn commits, so the new
    /// leaf tree never references an object that clone/fetch would 404 on.
    #[tokio::test]
    async fn attach_to_monorepo_parent_persists_gitkeep_blob_in_object_storage() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo, commit, command) =
            seed_import_repo_with_main_tip(&storage, "/third-party/newrepo").await;
        let repo_id = repo.repo_id;

        let import_repo = ImportRepo {
            storage: storage.clone(),
            repo,
            command_list: Mutex::new(vec![command]),
            git_object_cache: disabled_cache().await,
            receive_pack_extra_timings_ms: Mutex::new(vec![]),
        };

        import_repo.attach_to_monorepo_parent().await.unwrap();

        // Default-branch marker must survive queue attach (import API get_default_ref).
        let refs = storage.git_db_storage().get_ref(repo_id).await.unwrap();
        assert!(
            refs.iter().any(|r| r.default_branch),
            "attach must persist default_branch on at least one import ref, got {refs:?}"
        );

        // Walk the attached monorepo tree down to the new leaf.
        let mono = storage.mono_storage();
        let root_ref = mono.get_main_ref("/").await.unwrap().unwrap();
        let root_tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&root_ref.ref_tree_hash)
                .await
                .unwrap()
                .unwrap(),
        );
        let third_party = root_tree
            .tree_items
            .iter()
            .find(|item| item.name == "third-party")
            .expect("root tree must contain third-party");
        let third_party_tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&third_party.id.to_string())
                .await
                .unwrap()
                .unwrap(),
        );
        let newrepo = third_party_tree
            .tree_items
            .iter()
            .find(|item| item.name == "newrepo")
            .expect("third-party tree must contain newrepo after attach");
        let leaf_tree = Tree::from_mega_model(
            mono.get_tree_by_hash(&newrepo.id.to_string())
                .await
                .unwrap()
                .unwrap(),
        );
        let gitkeep = leaf_tree
            .tree_items
            .iter()
            .find(|item| item.name == ".gitkeep")
            .expect("leaf tree must reference a .gitkeep blob");

        // The regression: before the fix the blob was referenced by the tree but
        // never written to object storage, so reads 404'd on it.
        let bytes = storage
            .git_service
            .get_object_as_bytes(&gitkeep.id.to_string())
            .await
            .expect(".gitkeep blob must be readable from object storage");
        assert!(!bytes.is_empty());

        let blob_rows = mono
            .get_mega_blobs_by_hashes(vec![gitkeep.id.to_string()])
            .await
            .unwrap();
        assert_eq!(blob_rows.len(), 1);

        // Fingerprint + landed_commit_id (TP-08 AC).
        let rows = push_queue::Entity::find()
            .all(storage.git_db_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.kind, PushQueueKindEnum::Attach);
        let expected_op = attach_operation_id(
            &repo_id.to_string(),
            &normalize_attach_commands(&[(
                "refs/heads/main".into(),
                "Create".into(),
                ZERO_ID.to_string(),
                commit.id.to_string(),
            )]),
        );
        assert_eq!(row.operation_id, expected_op);
        assert_eq!(
            row.landed_commit_id.as_deref(),
            Some(root_ref.ref_commit_hash.as_str())
        );
    }

    /// FU-02 (#28): the attach root commit takes the imported tip's subject
    /// from its body for any signature kind, framed with the blank line.
    #[tokio::test]
    async fn fu02_attach_root_commit_uses_signed_tip_subject() {
        use git_internal::internal::object::ObjectTrait;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo, _commit, command) = seed_import_repo_with_tip_message(
            &storage,
            "/third-party/fu02-signed",
            "gpgsig -----BEGIN SSH SIGNATURE-----\n U1NIU0lHAAAAAQFU02\n -----END SSH SIGNATURE-----\n\nimport: ssh signed subject\n\nimport body line\n",
        )
        .await;
        let import_repo = ImportRepo {
            storage: storage.clone(),
            repo,
            command_list: Mutex::new(vec![command]),
            git_object_cache: disabled_cache().await,
            receive_pack_extra_timings_ms: Mutex::new(vec![]),
        };
        import_repo.attach_to_monorepo_parent().await.unwrap();

        let mono = storage.mono_storage();
        let root_ref = mono.get_main_ref("/").await.unwrap().unwrap();
        let root_commit = Commit::from_mega_model(
            mono.get_commit_by_hash(&root_ref.ref_commit_hash)
                .await
                .unwrap()
                .expect("attach root commit row"),
        );
        assert_eq!(root_commit.message, "\nimport: ssh signed subject");
        let raw = String::from_utf8(root_commit.to_data().unwrap()).unwrap();
        let (header, body) = raw.split_once("\n\n").expect("header/body blank line");
        assert!(!header.contains("gpgsig"));
        assert!(!raw.contains("SSH SIGNATURE"));
        assert_eq!(body, "import: ssh signed subject");
    }

    /// Prior queue round advances the root while attach is waiting; lock-held
    /// redo must land without stale CAS.
    #[tokio::test]
    async fn attach_succeeds_after_intervening_root_advance() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo_a, commit_a, cmd_a) =
            seed_import_repo_with_main_tip(&storage, "/third-party/first").await;
        let (repo_b, commit_b, cmd_b) =
            seed_import_repo_with_main_tip(&storage, "/third-party/second").await;
        let mono = storage.mono_storage();
        let root_before = mono.get_main_ref("/").await.unwrap().unwrap();

        async fn enqueue_attach(
            storage: &Storage,
            repo: &Repo,
            commit: &Commit,
            command: &RefCommand,
        ) -> i64 {
            let path = repo.repo_path.clone();
            let attach_cmds = vec![AttachCommand {
                ref_name: command.ref_name.clone(),
                old_id: command.old_id.clone(),
                new_id: command.new_id.clone(),
                command_type: "Create".into(),
                ref_type: "branch".into(),
                default_branch: command.default_branch,
            }];
            let payload = AttachPayload {
                op: AttachOp::Attach,
                repo_id: repo.repo_id,
                repo_path: path.clone(),
                commands: attach_cmds.clone(),
            };
            let fingerprint_rows: Vec<_> = attach_cmds
                .iter()
                .map(|c| {
                    (
                        c.ref_name.clone(),
                        c.command_type.clone(),
                        c.old_id.clone(),
                        c.new_id.clone(),
                    )
                })
                .collect();
            let operation_id = attach_operation_id(
                &repo.repo_id.to_string(),
                &normalize_attach_commands(&fingerprint_rows),
            );
            let root = storage
                .mono_storage()
                .get_main_ref("/")
                .await
                .unwrap()
                .unwrap();
            let EnqueueOutcome::Inserted { id } = storage
                .push_queue_service
                .enqueue(EnqueueRequest {
                    kind: PushQueueKindEnum::Attach,
                    operation_id,
                    path,
                    old_id: root.ref_commit_hash,
                    new_id: commit.id.to_string(),
                    requester: None,
                    payload: serde_json::to_value(&payload).unwrap(),
                    ref_name: None,
                    is_delete: false,
                })
                .await
                .unwrap()
            else {
                panic!("attach insert");
            };
            id
        }

        let first_id = enqueue_attach(&storage, &repo_a, &commit_a, &cmd_a).await;
        let second_id = enqueue_attach(&storage, &repo_b, &commit_b, &cmd_b).await;
        // Both captured the same diagnostic old_id at enqueue time.
        let row_a = storage
            .push_queue_service
            .storage()
            .get_by_id(first_id)
            .await
            .unwrap()
            .unwrap();
        let row_b = storage
            .push_queue_service
            .storage()
            .get_by_id(second_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row_a.old_id, root_before.ref_commit_hash);
        assert_eq!(row_b.old_id, root_before.ref_commit_hash);

        assert_eq!(
            storage
                .push_queue_service
                .wait_and_claim(first_id)
                .await
                .unwrap(),
            QueueWaitResult::Ready { id: first_id }
        );
        let ctx = AttachExecContext {
            storage: storage.clone(),
            git_object_cache: disabled_cache().await,
        };
        let first_outcome = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id: first_id,
                    ..Default::default()
                },
                Some(&ctx),
                None,
                None,
            )
            .await
            .unwrap();
        let ExecuteOutcome::Done {
            landed_commit_id: tip_after_first,
            ..
        } = first_outcome
        else {
            panic!("first attach must Done, got {first_outcome:?}");
        };

        let root_mid = mono.get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(root_mid.ref_commit_hash, tip_after_first);

        assert_eq!(
            storage
                .push_queue_service
                .wait_and_claim(second_id)
                .await
                .unwrap(),
            QueueWaitResult::Ready { id: second_id }
        );
        let second_outcome = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id: second_id,
                    ..Default::default()
                },
                Some(&ctx),
                None,
                None,
            )
            .await
            .unwrap();
        let ExecuteOutcome::Done {
            landed_commit_id, ..
        } = second_outcome
        else {
            panic!("second attach must Done after intervening advance, got {second_outcome:?}");
        };

        let root_after = mono.get_main_ref("/").await.unwrap().unwrap();
        assert_eq!(root_after.ref_commit_hash, landed_commit_id);
        assert_ne!(landed_commit_id, tip_after_first);
        let landed = storage
            .mono_storage()
            .get_commit_by_hash(&landed_commit_id)
            .await
            .unwrap()
            .expect("landed commit row");
        let parents: Vec<String> = serde_json::from_value(landed.parents_id.clone()).unwrap();
        assert!(
            parents.contains(&tip_after_first),
            "second attach parents {parents:?} must include intervening tip {tip_after_first}"
        );
    }

    /// AttachFailure must not freeze the queue (hard_stopped stays false).
    #[tokio::test]
    async fn attach_failure_does_not_hard_stop() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let (repo, commit, command) =
            seed_import_repo_with_main_tip(&storage, "/third-party/fail-attach").await;
        let path = repo.repo_path.clone();
        let mono = storage.mono_storage();
        let root = mono.get_main_ref("/").await.unwrap().unwrap();

        let payload = AttachPayload {
            op: AttachOp::Attach,
            repo_id: repo.repo_id,
            repo_path: path.clone(),
            commands: vec![AttachCommand {
                ref_name: command.ref_name.clone(),
                old_id: command.old_id.clone(),
                new_id: command.new_id.clone(),
                command_type: "Create".into(),
                ref_type: "branch".into(),
                default_branch: command.default_branch,
            }],
        };
        let EnqueueOutcome::Inserted { id } = storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Attach,
                operation_id: attach_operation_id(
                    &repo.repo_id.to_string(),
                    &normalize_attach_commands(&[(
                        command.ref_name.clone(),
                        "Create".into(),
                        command.old_id.clone(),
                        commit.id.to_string(),
                    )]),
                ),
                path: path.clone(),
                old_id: root.ref_commit_hash,
                new_id: commit.id.to_string(),
                requester: None,
                payload: serde_json::to_value(&payload).unwrap(),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap()
        else {
            panic!("insert");
        };

        // Materialize the target path after enqueue so B3 lock-held precheck fails.
        mono.save_refs(
            crate::callisto::mega_refs::Model::new(
                &path,
                crate::common::utils::MEGA_BRANCH_NAME.to_owned(),
                "1".repeat(40),
                "2".repeat(40),
                false,
            ),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            storage.push_queue_service.wait_and_claim(id).await.unwrap(),
            QueueWaitResult::Ready { id }
        );
        let ctx = AttachExecContext {
            storage: storage.clone(),
            git_object_cache: disabled_cache().await,
        };
        let outcome = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                Some(&ctx),
                None,
                None,
            )
            .await
            .unwrap();
        match &outcome {
            ExecuteOutcome::Failed { failure, .. } if failure == "AttachFailure" => {}
            other => panic!("expected AttachFailure, got {other:?}"),
        }

        let ctrl = queue_control::Entity::find_by_id(1)
            .one(storage.git_db_storage().get_connection())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !ctrl.hard_stopped,
            "AttachFailure must not freeze the queue"
        );
    }

    /// Non-canonical path aliases must hit the same I3 precheck as the canonical path.
    #[tokio::test]
    async fn attach_precheck_rejects_repeated_slash_alias() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let mono = storage.mono_storage();
        mono.save_refs(
            crate::callisto::mega_refs::Model::new(
                "/third-party/aliased",
                crate::common::utils::MEGA_BRANCH_NAME.to_owned(),
                "a".repeat(40),
                "b".repeat(40),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let err = mono
            .attach_materialization_precheck("/third-party//aliased")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("/third-party/aliased") && err.contains("I3"),
            "alias must canonicalize then refuse, got {err}"
        );
    }

    /// Corrupt attach payload must terminalize AttachFailure, not leave Running.
    #[tokio::test]
    async fn attach_corrupt_payload_terminalizes_not_running() {
        use crate::callisto::sea_orm_active_enums::PushQueueStatusEnum;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let root = storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap();
        let EnqueueOutcome::Inserted { id } = storage
            .push_queue_service
            .enqueue(EnqueueRequest {
                kind: PushQueueKindEnum::Attach,
                operation_id: "corrupt-payload-op".into(),
                path: "/third-party/corrupt".into(),
                old_id: root.ref_commit_hash,
                new_id: "c".repeat(40),
                requester: None,
                payload: serde_json::json!({"not": "an AttachPayload"}),
                ref_name: None,
                is_delete: false,
            })
            .await
            .unwrap()
        else {
            panic!("insert");
        };
        assert_eq!(
            storage.push_queue_service.wait_and_claim(id).await.unwrap(),
            QueueWaitResult::Ready { id }
        );
        let ctx = AttachExecContext {
            storage: storage.clone(),
            git_object_cache: disabled_cache().await,
        };
        let outcome = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                Some(&ctx),
                None,
                None,
            )
            .await
            .unwrap();
        match &outcome {
            ExecuteOutcome::Failed { failure, .. } if failure == "AttachFailure" => {}
            other => panic!("expected AttachFailure, got {other:?}"),
        }
        let row = storage
            .push_queue_service
            .storage()
            .get_by_id(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, PushQueueStatusEnum::Failed);
    }

    #[test]
    fn attach_payload_default_op() {
        // Rows written before FU-16 carry no `op` and are all attaches.
        let legacy = serde_json::json!({
            "repo_id": 7,
            "repo_path": "/third-party/x",
            "commands": [],
        });
        let payload: AttachPayload = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(payload.op, AttachOp::Attach);
        // An attach still serializes without the field.
        assert_eq!(serde_json::to_value(&payload).unwrap(), legacy);
        let detach = AttachPayload {
            op: AttachOp::Detach,
            ..payload
        };
        assert_eq!(serde_json::to_value(&detach).unwrap()["op"], "detach");
    }

    // ---------------------------------------------------------------------
    // plan-20260923 FU-17: sweep, resume and the cleanup entry.
    // ---------------------------------------------------------------------

    async fn fu17_mount(storage: &Storage, path: &str) -> Repo {
        let (repo, _commit, create) = seed_import_repo_with_main_tip(storage, path).await;
        fu13_push(storage, &repo, vec![create]).await.unwrap();
        repo
    }

    async fn fu17_remove(
        storage: &Storage,
        path: &str,
        requester: Option<&str>,
        continuation: Option<i64>,
    ) -> Result<RemoveOutcome, crate::common::errors::MegaError> {
        remove_import_repo(
            storage,
            disabled_cache().await,
            path,
            requester.map(str::to_owned),
            continuation,
        )
        .await
    }

    async fn fu17_audit(
        storage: &Storage,
        repo_id: i64,
        phase: &str,
    ) -> Vec<crate::callisto::audit_logs::Model> {
        use crate::callisto::{audit_logs, sea_orm_active_enums::AuditActionEnum};
        audit_logs::Entity::find()
            .filter(audit_logs::Column::TargetId.eq(repo_id))
            .filter(audit_logs::Column::Action.eq(AuditActionEnum::Delete))
            .all(storage.git_db_storage().get_connection())
            .await
            .unwrap()
            .into_iter()
            .filter(|row| {
                row.metadata
                    .as_ref()
                    .and_then(|m| m.get("phase"))
                    .and_then(|p| p.as_str())
                    == Some(phase)
            })
            .collect()
    }

    async fn fu17_ledger(
        storage: &Storage,
        id: i64,
    ) -> crate::callisto::import_repo_cleanups::Model {
        let git_db = storage.git_db_storage();
        git_db
            .cleanup_by_id(id, git_db.get_connection())
            .await
            .unwrap()
            .expect("ledger row")
    }

    async fn fu17_objects(storage: &Storage, repo_id: i64) -> usize {
        storage
            .git_db_storage()
            .get_obj_count_by_repo_id(repo_id)
            .await
    }

    async fn fu17_count(storage: &Storage, sql: &str) -> i64 {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        storage
            .git_db_storage()
            .get_connection()
            .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap()
    }

    async fn fu17_exec(storage: &Storage, sql: &str) {
        use sea_orm::ConnectionTrait;
        storage
            .git_db_storage()
            .get_connection()
            .execute_unprepared(sql)
            .await
            .unwrap();
    }

    async fn fu17_root(storage: &Storage) -> String {
        storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap()
            .ref_commit_hash
    }

    fn fu17_counts(commit: u64, tree: u64, blob: u64, tag: u64) -> serde_json::Value {
        serde_json::json!({ "git_commit": commit, "git_tree": tree, "git_blob": blob, "git_tag": tag })
    }

    #[tokio::test]
    async fn fu17_swept_ledger_and_audit() {
        use crate::callisto::{
            import_repo_cleanups::CleanupState, sea_orm_active_enums::ActorTypeEnum,
        };

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu17-audit";
        let repo = fu17_mount(&storage, path).await;
        let RemoveOutcome::Removed {
            repo_id,
            cleanup_id,
        } = fu17_remove(&storage, path, Some("ci-token"), None)
            .await
            .unwrap()
        else {
            panic!("expected Removed");
        };
        assert_eq!(repo_id, repo.repo_id);

        let ledger = fu17_ledger(&storage, cleanup_id).await;
        assert_eq!(ledger.state, CleanupState::Swept);
        assert_eq!(ledger.rows_deleted, Some(fu17_counts(1, 1, 1, 0)));
        assert!(ledger.swept_at.is_some());
        assert_eq!(ledger.requester, "ci-token");
        assert_eq!(fu17_objects(&storage, repo_id).await, 0);
        assert!(
            storage
                .git_db_storage()
                .find_git_repo_exact_match(path)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .git_db_storage()
                .get_ref(repo_id)
                .await
                .unwrap()
                .is_empty()
        );

        let detached = fu17_audit(&storage, repo_id, "detached").await;
        assert_eq!(detached.len(), 1, "{detached:?}");
        assert_eq!(
            detached[0].metadata,
            Some(serde_json::json!({
                "kind": "import_repo.remove",
                "cleanup_id": cleanup_id,
                "path": path,
                "requester": "ci-token",
                "phase": "detached",
            }))
        );
        let swept = fu17_audit(&storage, repo_id, "swept").await;
        assert_eq!(swept.len(), 1, "{swept:?}");
        assert_eq!(swept[0].actor_id, 0);
        assert_eq!(swept[0].actor_type, ActorTypeEnum::Human);
        assert_eq!(
            swept[0].metadata,
            Some(serde_json::json!({
                "kind": "import_repo.remove",
                "cleanup_id": cleanup_id,
                "path": path,
                "requester": "ci-token",
                "phase": "swept",
                "rows_deleted": fu17_counts(1, 1, 1, 0),
            }))
        );

        // Idempotent: nothing left to do, and a continuation of a swept row
        // answers without writing.
        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );
        assert_eq!(
            fu17_remove(&storage, path, None, Some(cleanup_id))
                .await
                .unwrap(),
            RemoveOutcome::Removed {
                repo_id,
                cleanup_id
            }
        );
        assert_eq!(fu17_audit(&storage, repo_id, "swept").await.len(), 1);
        assert_eq!(
            fu17_ledger(&storage, cleanup_id).await.swept_at,
            ledger.swept_at
        );
    }

    #[tokio::test]
    async fn fu17_remove_resumes_after_crash() {
        use crate::callisto::{audit_logs, import_repo_cleanups::CleanupState};

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu17-crash";
        let repo = fu17_mount(&storage, path).await;
        // Detach committed, sweep never ran (a crash between the two).
        let cleanup_id =
            detach_import_repo(&storage, disabled_cache().await, repo.repo_id, path, None)
                .await
                .unwrap()
                .expect("cleanup id");
        let ledger = fu17_ledger(&storage, cleanup_id).await;
        assert_eq!(ledger.state, CleanupState::Detached);
        assert_eq!(ledger.requester, "anonymous");
        assert_eq!(fu17_objects(&storage, repo.repo_id).await, 3);
        assert!(
            storage
                .git_db_storage()
                .find_git_repo_exact_match(path)
                .await
                .unwrap()
                .is_none()
        );
        let queue_rows = fu17_count(&storage, "SELECT count(*) AS n FROM push_queue").await;
        // The resume reads the ledger only: audit rows are not consulted.
        audit_logs::Entity::delete_many()
            .filter(audit_logs::Column::TargetId.eq(repo.repo_id))
            .exec(storage.git_db_storage().get_connection())
            .await
            .unwrap();

        assert_eq!(
            fu17_remove(&storage, path, Some("operator-cli"), None)
                .await
                .unwrap(),
            RemoveOutcome::Absent
        );
        assert_eq!(fu17_objects(&storage, repo.repo_id).await, 0);
        let ledger = fu17_ledger(&storage, cleanup_id).await;
        assert_eq!(ledger.state, CleanupState::Swept);
        assert_eq!(ledger.rows_deleted, Some(fu17_counts(1, 1, 1, 0)));
        assert_eq!(
            ledger.requester, "anonymous",
            "the ledger keeps the detacher"
        );
        let swept = fu17_audit(&storage, repo.repo_id, "swept").await;
        assert_eq!(swept.len(), 1);
        assert_eq!(
            swept[0].metadata.as_ref().and_then(|m| m.get("requester")),
            Some(&serde_json::json!("operator-cli")),
            "the audit row names who swept"
        );
        assert_eq!(
            fu17_count(&storage, "SELECT count(*) AS n FROM push_queue").await,
            queue_rows,
            "a resume never enqueues"
        );
        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );
        assert_eq!(fu17_audit(&storage, repo.repo_id, "swept").await.len(), 1);
    }

    #[tokio::test]
    async fn fu17_continuation_never_touches_reimport() {
        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu17-reimport";
        let r1 = fu17_mount(&storage, path).await;
        let c1 = detach_import_repo(&storage, disabled_cache().await, r1.repo_id, path, None)
            .await
            .unwrap()
            .expect("cleanup id");
        // An older pending cleanup of the same path: a continuation of c1
        // handles c1 alone.
        let c0 = c1 - 1;
        let repo0 = fu17_pending_row(&storage, c0, path).await;
        // A re-import at the same path while R1's sweep is pending.
        let r2 = fu17_mount(&storage, path).await;
        assert_ne!(r2.repo_id, r1.repo_id);
        let root = fu17_root(&storage).await;
        let r2_refs = storage.git_db_storage().get_ref(r2.repo_id).await.unwrap();
        assert!(!r2_refs.is_empty());
        let queue_rows = fu17_count(&storage, "SELECT count(*) AS n FROM push_queue").await;

        assert_eq!(
            fu17_remove(&storage, path, Some("operator-cli"), Some(c1))
                .await
                .unwrap(),
            RemoveOutcome::Removed {
                repo_id: r1.repo_id,
                cleanup_id: c1
            }
        );
        assert_eq!(fu17_objects(&storage, r1.repo_id).await, 0);
        assert_eq!(
            fu17_ledger(&storage, c0).await.state,
            CleanupState::Detached
        );
        assert_eq!(fu17_objects(&storage, repo0).await, 1);
        assert!(fu17_audit(&storage, repo0, "swept").await.is_empty());
        assert_eq!(
            fu17_objects(&storage, r2.repo_id).await,
            3,
            "the re-import keeps its rows, including the shared readme blob"
        );
        assert_eq!(
            storage
                .git_db_storage()
                .find_git_repo_exact_match(path)
                .await
                .unwrap()
                .map(|row| row.id),
            Some(r2.repo_id)
        );
        assert_eq!(
            storage.git_db_storage().get_ref(r2.repo_id).await.unwrap(),
            r2_refs
        );
        assert_eq!(fu17_root(&storage).await, root);
        assert_eq!(
            fu17_count(&storage, "SELECT count(*) AS n FROM push_queue").await,
            queue_rows,
            "a continuation never detaches or enqueues"
        );

        // Unknown or foreign cleanup ids are refused without writes.
        let err = fu17_remove(&storage, path, None, Some(999_999))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("IMPORT_REPO_CLEANUP_NOT_FOUND"), "{err}");
        let foreign = generate_id();
        storage
            .git_db_storage()
            .insert_cleanup_in_txn(
                foreign,
                "/third-party/elsewhere",
                1,
                "anonymous",
                storage.git_db_storage().get_connection(),
            )
            .await
            .unwrap();
        let err = fu17_remove(&storage, path, None, Some(foreign))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("IMPORT_REPO_CLEANUP_NOT_FOUND"), "{err}");
        assert_eq!(fu17_objects(&storage, r2.repo_id).await, 3);

        let swept_rows = fu17_audit(&storage, r1.repo_id, "swept").await.len();
        assert_eq!(
            fu17_remove(&storage, path, None, Some(c1)).await.unwrap(),
            RemoveOutcome::Removed {
                repo_id: r1.repo_id,
                cleanup_id: c1
            }
        );
        assert_eq!(
            fu17_audit(&storage, r1.repo_id, "swept").await.len(),
            swept_rows
        );
        assert_eq!(
            fu17_ledger(&storage, c0).await.state,
            CleanupState::Detached
        );
        // A path-only request now removes the re-import under a new id, and
        // resumes c0 as an older row of the path.
        let RemoveOutcome::Removed {
            repo_id,
            cleanup_id,
        } = fu17_remove(&storage, path, None, None).await.unwrap()
        else {
            panic!("expected Removed");
        };
        assert_eq!(repo_id, r2.repo_id);
        assert_ne!(cleanup_id, c1);
        assert_eq!(fu17_ledger(&storage, c0).await.state, CleanupState::Swept);
        assert_eq!(fu17_objects(&storage, repo0).await, 0);
    }

    async fn fu17_write_snapshot(
        storage: &Storage,
        repo_id: i64,
    ) -> (i64, i64, i64, i64, usize, String, usize) {
        (
            fu17_count(storage, "SELECT count(*) AS n FROM push_queue").await,
            fu17_count(storage, "SELECT count(*) AS n FROM audit_logs").await,
            fu17_count(storage, "SELECT count(*) AS n FROM import_repo_cleanups").await,
            fu17_count(storage, "SELECT count(*) AS n FROM git_repo").await,
            storage
                .git_db_storage()
                .get_ref(repo_id)
                .await
                .unwrap()
                .len(),
            fu17_root(storage).await,
            fu17_objects(storage, repo_id).await,
        )
    }

    #[tokio::test]
    async fn fu17_remove_has_children_no_writes() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu17-parent";
        let parent = fu17_mount(&storage, path).await;
        // A registered child (no attach yet) and an alias row of the parent.
        let (child, _, _) =
            seed_import_repo_with_main_tip(&storage, "/third-party/fu17-parent/child").await;
        storage
            .git_db_storage()
            .save_git_repo(crate::callisto::git_repo::Model {
                id: generate_id(),
                repo_path: "/third-party/fu17-parent/".to_owned(),
                repo_name: "alias".to_owned(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
            })
            .await
            .unwrap();
        let before = fu17_write_snapshot(&storage, parent.repo_id).await;
        let err = fu17_remove(&storage, path, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("IMPORT_REPO_HAS_CHILDREN"), "{err}");
        assert_eq!(
            fu17_write_snapshot(&storage, parent.repo_id).await,
            before,
            "no write of any kind"
        );

        fu17_exec(
            &storage,
            &format!("DELETE FROM git_repo WHERE id = {}", child.repo_id),
        )
        .await;
        assert!(matches!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Removed { repo_id, .. } if repo_id == parent.repo_id
        ));
        assert_eq!(
            fu17_count(
                &storage,
                "SELECT count(*) AS n FROM git_repo WHERE repo_path = '/third-party/fu17-parent/'"
            )
            .await,
            1,
            "the alias row is not a child and is left alone"
        );
    }

    #[tokio::test]
    async fn fu17_remove_keeps_shared_blob_and_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        use futures::StreamExt;

        let a = fu17_mount(&storage, "/third-party/fu17-a").await;
        let b = fu17_mount(&storage, "/third-party/fu17-ab").await;
        // The Monorepo's own placeholder blob, also registered under A: the
        // sweep drops A's row, the Monorepo keeps its row and the bytes.
        let gitkeep_id = Blob::from_content("Placeholder file for /third-party directory")
            .id
            .to_string();
        assert_eq!(
            storage
                .mono_storage()
                .get_mega_blobs_by_hashes(vec![gitkeep_id.clone()])
                .await
                .unwrap()
                .len(),
            1,
            "init_monorepo wrote the placeholder"
        );
        let readme_id = Blob::from_content("hello from import repo").id.to_string();
        storage
            .git_db_storage()
            .batch_save_model::<git_blob::Entity, git_blob::ActiveModel>(vec![
                git_blob::Model {
                    id: generate_id(),
                    repo_id: a.repo_id,
                    blob_id: gitkeep_id.clone(),
                    name: None,
                    size: 0,
                    created_at: chrono::Utc::now().naive_utc(),
                    pack_id: String::new(),
                    file_path: String::new(),
                    pack_offset: 0,
                    is_delta_in_pack: false,
                }
                .into_active_model(),
            ])
            .await
            .unwrap();
        let mega_blobs = fu17_count(&storage, "SELECT count(*) AS n FROM mega_blob").await;
        let root_before = fu17_root(&storage).await;
        let b_refs = storage.git_db_storage().get_ref(b.repo_id).await.unwrap();

        let RemoveOutcome::Removed {
            repo_id: a_removed,
            cleanup_id: a_cleanup,
        } = fu17_remove(&storage, "/third-party/fu17-a", None, None)
            .await
            .unwrap()
        else {
            panic!("expected Removed");
        };
        assert_eq!(a_removed, a.repo_id);
        // The Monorepo side is untouched: its blob rows (and the object
        // store) keep every shared byte; the sibling that shares the readme
        // content and the path prefix keeps its rows, refs and registration.
        assert_eq!(
            fu17_count(&storage, "SELECT count(*) AS n FROM mega_blob").await,
            mega_blobs
        );
        assert_ne!(
            fu17_root(&storage).await,
            root_before,
            "the mount was removed"
        );
        assert_eq!(fu17_objects(&storage, a.repo_id).await, 0);
        assert_eq!(fu17_objects(&storage, b.repo_id).await, 3);
        assert_eq!(
            fu17_ledger(&storage, a_cleanup).await.rows_deleted,
            Some(fu17_counts(1, 1, 2, 0)),
            "A's two blob rows (readme and placeholder) were the ones deleted"
        );
        // Shared bytes stay readable through the Monorepo and the object store.
        assert_eq!(
            storage
                .mono_storage()
                .get_mega_blobs_by_hashes(vec![gitkeep_id.clone()])
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            storage
                .git_service
                .get_object_as_bytes(&gitkeep_id)
                .await
                .is_ok()
        );
        assert!(
            storage
                .git_service
                .get_object_as_bytes(&readme_id)
                .await
                .is_ok()
        );
        // The sibling still serves a clone: its commit and its blob rows,
        // including the readme it shares with the removed repository, are
        // what upload-pack streams.
        let b_commits: Vec<_> = storage
            .git_db_storage()
            .get_commits_by_repo_id(b.repo_id)
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(b_commits.len(), 1);
        let b_blobs: Vec<String> = storage
            .git_db_storage()
            .get_blobs_by_repo_id(b.repo_id)
            .await
            .unwrap()
            .filter_map(|row| async move { row.ok().map(|m| m.blob_id) })
            .collect()
            .await;
        assert!(b_blobs.contains(&readme_id), "{b_blobs:?}");
        let a_blobs: Vec<_> = storage
            .git_db_storage()
            .get_blobs_by_repo_id(a.repo_id)
            .await
            .unwrap()
            .collect()
            .await;
        assert!(a_blobs.is_empty());
        assert_eq!(
            storage.git_db_storage().get_ref(b.repo_id).await.unwrap(),
            b_refs
        );
        assert!(
            storage
                .git_db_storage()
                .find_git_repo_exact_match("/third-party/fu17-ab")
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            fu17_count(
                &storage,
                &format!(
                    "SELECT count(*) AS n FROM import_repo_cleanups WHERE repo_id = {}",
                    b.repo_id
                )
            )
            .await,
            0
        );
        assert!(matches!(
            fu17_remove(&storage, "/third-party/fu17-ab", None, None)
                .await
                .unwrap(),
            RemoveOutcome::Removed { repo_id, .. } if repo_id == b.repo_id
        ));
    }

    #[tokio::test]
    async fn fu17_pending_rechecks_ledger_before_answering() {
        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let git_db = storage.git_db_storage();
        let path = "/third-party/fu17-race";
        let id = generate_id();
        let repo_id = fu17_pending_row(&storage, id, path).await;
        let stale = fu17_ledger(&storage, id).await;
        assert_eq!(stale.state, CleanupState::Detached);
        // Another sweeper finishes the row while this request still holds
        // the snapshot it read; this request has no budget left.
        assert!(
            git_db
                .mark_cleanup_swept(id, fu17_counts(0, 0, 1, 0), git_db.get_connection())
                .await
                .unwrap()
        );
        let mut budget = 0u32;
        assert_eq!(
            sweep_ledger_row(&git_db, &stale, "late", &mut budget)
                .await
                .unwrap(),
            RowSweep::Swept,
            "a pending answer never names a swept row"
        );
        assert_eq!(
            fu17_objects(&storage, repo_id).await,
            1,
            "no budget, no delete"
        );
        assert_eq!(
            fu17_ledger(&storage, id).await.rows_deleted,
            Some(fu17_counts(0, 0, 1, 0)),
            "the other sweeper's counts stand"
        );
        assert!(fu17_audit(&storage, repo_id, "swept").await.is_empty());
    }

    async fn fu17_pending_row(storage: &Storage, id: i64, path: &str) -> i64 {
        let repo_id = generate_id();
        let git_db = storage.git_db_storage();
        git_db
            .insert_cleanup_in_txn(id, path, repo_id, "anonymous", git_db.get_connection())
            .await
            .unwrap();
        git_db
            .batch_save_model::<git_blob::Entity, git_blob::ActiveModel>(vec![
                git_blob::Model {
                    id: generate_id(),
                    repo_id,
                    blob_id: format!("{id:040x}"),
                    name: None,
                    size: 0,
                    created_at: chrono::Utc::now().naive_utc(),
                    pack_id: String::new(),
                    file_path: String::new(),
                    pack_offset: 0,
                    is_delta_in_pack: false,
                }
                .into_active_model(),
            ])
            .await
            .unwrap();
        repo_id
    }

    #[tokio::test]
    async fn fu17_resume_budget_paginates() {
        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let git_db = storage.git_db_storage();
        let path = "/third-party/fu17-page";
        let base = generate_id();
        let mut repos = Vec::new();
        for i in 1..=17 {
            repos.push(fu17_pending_row(&storage, base + i, path).await);
        }
        // Decoys: swept rows on the path, pending rows elsewhere.
        for i in 18..=20 {
            fu17_pending_row(&storage, base + i, path).await;
            assert!(
                git_db
                    .mark_cleanup_swept(base + i, fu17_counts(0, 0, 1, 0), git_db.get_connection())
                    .await
                    .unwrap()
            );
        }
        for i in 21..=22 {
            fu17_pending_row(&storage, base + i, "/third-party/fu17-other").await;
        }
        let detached_on = |path: &'static str| {
            format!(
                "SELECT count(*) AS n FROM import_repo_cleanups WHERE path = '{path}' AND state = 'detached'"
            )
        };

        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Pending {
                repo_id: repos[16],
                cleanup_id: base + 17
            }
        );
        for i in 1..=16 {
            assert_eq!(
                fu17_ledger(&storage, base + i).await.state,
                CleanupState::Swept,
                "{i}"
            );
        }
        assert_eq!(
            fu17_ledger(&storage, base + 17).await.state,
            CleanupState::Detached
        );
        assert_eq!(fu17_objects(&storage, repos[16]).await, 1);
        assert_eq!(fu17_count(&storage, &detached_on(path)).await, 1);
        assert_eq!(
            fu17_count(&storage, &detached_on("/third-party/fu17-other")).await,
            2
        );

        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );
        assert_eq!(
            fu17_ledger(&storage, base + 17).await.state,
            CleanupState::Swept
        );
        assert_eq!(fu17_objects(&storage, repos[16]).await, 0);
        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );

        // Exactly one full page: no spurious Pending.
        let path2 = "/third-party/fu17-page2";
        let base2 = generate_id();
        for i in 1..=16 {
            fu17_pending_row(&storage, base2 + i, path2).await;
        }
        assert_eq!(
            fu17_remove(&storage, path2, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );
        assert_eq!(fu17_count(&storage, &detached_on(path2)).await, 0);
    }

    #[tokio::test]
    async fn fu17_sweep_budget_pending() {
        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu17-budget";
        let repo = fu17_mount(&storage, path).await;
        // 100 500 more blobs: more than the 100 statements of one request.
        let base = generate_id();
        fu17_exec(
            &storage,
            &format!(
                "INSERT INTO git_blob (id, repo_id, blob_id, name, size, created_at, pack_id, file_path, pack_offset, is_delta_in_pack) \
                 SELECT {base} + g, {}, 'f' || lpad(g::text, 39, '0'), NULL, 0, now(), '', '', 0, false \
                 FROM generate_series(1, 100500) g",
                repo.repo_id
            ),
        )
        .await;
        fu17_exec(&storage, "ANALYZE git_blob").await;
        let queue_rows = fu17_count(&storage, "SELECT count(*) AS n FROM push_queue").await;

        let RemoveOutcome::Pending {
            repo_id,
            cleanup_id,
        } = fu17_remove(&storage, path, Some("operator-cli"), None)
            .await
            .unwrap()
        else {
            panic!("expected Pending");
        };
        assert_eq!(repo_id, repo.repo_id);
        // 1 (commit) + 1 (tree) + 98 (blob batches) statements.
        assert_eq!(fu17_objects(&storage, repo_id).await, 2501);
        let ledger = fu17_ledger(&storage, cleanup_id).await;
        assert_eq!(ledger.state, CleanupState::Detached);
        assert_eq!(ledger.rows_deleted, Some(fu17_counts(1, 1, 98_000, 0)));
        assert!(fu17_audit(&storage, repo_id, "swept").await.is_empty());
        assert!(
            storage
                .git_db_storage()
                .find_git_repo_exact_match(path)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fu17_count(&storage, "SELECT count(*) AS n FROM push_queue").await,
            queue_rows + 1
        );

        assert_eq!(
            fu17_remove(&storage, path, Some("operator-cli"), Some(cleanup_id))
                .await
                .unwrap(),
            RemoveOutcome::Removed {
                repo_id,
                cleanup_id
            }
        );
        let ledger = fu17_ledger(&storage, cleanup_id).await;
        assert_eq!(ledger.state, CleanupState::Swept);
        assert_eq!(ledger.rows_deleted, Some(fu17_counts(1, 1, 100_501, 0)));
        assert!(ledger.swept_at.is_some());
        let swept = fu17_audit(&storage, repo_id, "swept").await;
        assert_eq!(swept.len(), 1);
        assert_eq!(
            swept[0]
                .metadata
                .as_ref()
                .and_then(|m| m.get("rows_deleted")),
            Some(&fu17_counts(1, 1, 100_501, 0))
        );
        assert_eq!(fu17_objects(&storage, repo_id).await, 0);
        assert_eq!(
            fu17_count(&storage, "SELECT count(*) AS n FROM push_queue").await,
            queue_rows + 1,
            "a continuation never enqueues"
        );
        assert_eq!(
            fu17_remove(&storage, path, None, Some(cleanup_id))
                .await
                .unwrap(),
            RemoveOutcome::Removed {
                repo_id,
                cleanup_id
            }
        );
        assert_eq!(fu17_audit(&storage, repo_id, "swept").await.len(), 1);
        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );
    }

    #[tokio::test]
    async fn fu17_fresh_detach_resumes_older_rows() {
        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        // (a) A pending cleanup of an earlier import at the path: the request
        // that removes the re-import sweeps its own row and then the older one.
        let path = "/third-party/fu17-older";
        let r1 = fu17_mount(&storage, path).await;
        let c1 = detach_import_repo(&storage, disabled_cache().await, r1.repo_id, path, None)
            .await
            .unwrap()
            .expect("cleanup id");
        let r2 = fu17_mount(&storage, path).await;
        let RemoveOutcome::Removed {
            repo_id,
            cleanup_id,
        } = fu17_remove(&storage, path, None, None).await.unwrap()
        else {
            panic!("expected Removed");
        };
        assert_eq!(repo_id, r2.repo_id);
        assert_ne!(cleanup_id, c1);
        assert_eq!(fu17_ledger(&storage, c1).await.state, CleanupState::Swept);
        assert_eq!(fu17_objects(&storage, r1.repo_id).await, 0);
        assert_eq!(fu17_objects(&storage, r2.repo_id).await, 0);

        // (b) The 16-row cap counts the fresh detach: 16 older rows leave one.
        let path2 = "/third-party/fu17-older16";
        let fresh = fu17_mount(&storage, path2).await;
        let base = generate_id();
        let mut repos = Vec::new();
        for i in 1..=16 {
            repos.push(fu17_pending_row(&storage, base + i, path2).await);
        }
        let RemoveOutcome::Removed {
            repo_id,
            cleanup_id,
        } = fu17_remove(&storage, path2, None, None).await.unwrap()
        else {
            panic!("expected Removed");
        };
        assert_eq!(repo_id, fresh.repo_id);
        assert_eq!(
            fu17_ledger(&storage, cleanup_id).await.state,
            CleanupState::Swept
        );
        for i in 1..=15 {
            assert_eq!(
                fu17_ledger(&storage, base + i).await.state,
                CleanupState::Swept,
                "{i}"
            );
        }
        assert_eq!(
            fu17_ledger(&storage, base + 16).await.state,
            CleanupState::Detached
        );
        assert_eq!(fu17_objects(&storage, repos[15]).await, 1);
        assert_eq!(
            fu17_remove(&storage, path2, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );
        assert_eq!(
            fu17_ledger(&storage, base + 16).await.state,
            CleanupState::Swept
        );
        assert_eq!(fu17_objects(&storage, repos[15]).await, 0);
    }

    #[tokio::test]
    async fn fu17_swept_by_another_request_writes_no_second_audit() {
        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let git_db = storage.git_db_storage();
        let path = "/third-party/fu17-loser";
        let id = generate_id();
        let repo_id = fu17_pending_row(&storage, id, path).await;
        let stale = fu17_ledger(&storage, id).await;
        assert_eq!(stale.state, CleanupState::Detached);
        // Another request marks the row swept while this one still sweeps
        // from its snapshot: this one loses the mark and writes no audit row.
        assert!(
            git_db
                .mark_cleanup_swept(id, fu17_counts(0, 0, 9, 0), git_db.get_connection())
                .await
                .unwrap()
        );
        let mut budget = SWEEP_STATEMENT_BUDGET;
        assert_eq!(
            sweep_ledger_row(&git_db, &stale, "loser", &mut budget)
                .await
                .unwrap(),
            RowSweep::Swept
        );
        assert_eq!(
            fu17_objects(&storage, repo_id).await,
            0,
            "the sweep itself ran"
        );
        assert!(fu17_audit(&storage, repo_id, "swept").await.is_empty());
        assert_eq!(
            fu17_ledger(&storage, id).await.rows_deleted,
            Some(fu17_counts(0, 0, 9, 0)),
            "the winner's counts stand"
        );
    }

    #[tokio::test]
    async fn fu17_budget_exhausted_between_rows() {
        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let git_db = storage.git_db_storage();
        let path = "/third-party/fu17-between";
        // Row 1 needs exactly the whole budget; row 2 is next in line.
        let (id1, repo1) = (generate_id(), generate_id());
        git_db
            .insert_cleanup_in_txn(id1, path, repo1, "anonymous", git_db.get_connection())
            .await
            .unwrap();
        let base = generate_id();
        fu17_exec(
            &storage,
            &format!(
                "INSERT INTO git_blob (id, repo_id, blob_id, name, size, created_at, pack_id, file_path, pack_offset, is_delta_in_pack) \
                 SELECT {base} + g, {repo1}, 'e' || lpad(g::text, 39, '0'), NULL, 0, now(), '', '', 0, false \
                 FROM generate_series(1, 100000) g"
            ),
        )
        .await;
        fu17_exec(&storage, "ANALYZE git_blob").await;
        let id2 = id1 + 1;
        let repo2 = fu17_pending_row(&storage, id2, path).await;

        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Pending {
                repo_id: repo2,
                cleanup_id: id2
            }
        );
        let row1 = fu17_ledger(&storage, id1).await;
        assert_eq!(
            row1.state,
            CleanupState::Swept,
            "exactly 100 statements finish row 1"
        );
        assert_eq!(row1.rows_deleted, Some(fu17_counts(0, 0, 100_000, 0)));
        assert_eq!(fu17_objects(&storage, repo1).await, 0);
        let row2 = fu17_ledger(&storage, id2).await;
        assert_eq!(row2.state, CleanupState::Detached);
        assert_eq!(row2.rows_deleted, None, "no budget left: no progress write");
        assert_eq!(fu17_objects(&storage, repo2).await, 1);

        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );
        assert_eq!(fu17_ledger(&storage, id2).await.state, CleanupState::Swept);
        assert_eq!(fu17_objects(&storage, repo2).await, 0);
    }

    #[tokio::test]
    async fn fu17_b3_refusal_surfaces_typed_error() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu17-b3";
        let parent = fu17_mount(&storage, path).await;
        seed_import_repo_with_main_tip(&storage, "/third-party/fu17-b3/child").await;
        // Past the entry's pre-check (a child registered in between): B3's
        // own re-check refuses and the refusal reaches the caller typed.
        let err = detach_import_repo(&storage, disabled_cache().await, parent.repo_id, path, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("IMPORT_REPO_HAS_CHILDREN"), "{err}");
        assert!(
            storage
                .git_db_storage()
                .find_git_repo_exact_match(path)
                .await
                .unwrap()
                .is_some_and(|row| row.id == parent.repo_id)
        );
        assert_eq!(
            fu17_count(
                &storage,
                &format!(
                    "SELECT count(*) AS n FROM import_repo_cleanups WHERE repo_id = {}",
                    parent.repo_id
                )
            )
            .await,
            0
        );
        assert_eq!(
            fu17_count(
                &storage,
                "SELECT count(*) AS n FROM push_queue WHERE status = 'Failed'"
            )
            .await,
            1,
            "the refused round is a Failed queue row"
        );
    }

    /// One budget of 100 statements per request, shared by the request's own
    /// detach and the older rows of the path it resumes.
    #[tokio::test]
    async fn fu17_budget_shared_with_older_rows() {
        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu17-shared";
        // The fresh repository costs exactly 90 statements (one commit batch,
        // one tree batch, 88 blob batches for 87 501 blobs); 11 older rows of
        // one blob each follow: 10 fit the request, the 11th does not.
        let fresh = fu17_mount(&storage, path).await;
        let base = generate_id();
        fu17_exec(
            &storage,
            &format!(
                "INSERT INTO git_blob (id, repo_id, blob_id, name, size, created_at, pack_id, file_path, pack_offset, is_delta_in_pack) \
                 SELECT {base} + g, {}, 'd' || lpad(g::text, 39, '0'), NULL, 0, now(), '', '', 0, false \
                 FROM generate_series(1, 87500) g",
                fresh.repo_id
            ),
        )
        .await;
        fu17_exec(&storage, "ANALYZE git_blob").await;
        let ledger_base = generate_id();
        let mut older = Vec::new();
        for i in 1..=11 {
            older.push(fu17_pending_row(&storage, ledger_base + i, path).await);
        }

        let RemoveOutcome::Removed {
            repo_id,
            cleanup_id,
        } = fu17_remove(&storage, path, None, None).await.unwrap()
        else {
            panic!("expected Removed");
        };
        assert_eq!(repo_id, fresh.repo_id);
        let own = fu17_ledger(&storage, cleanup_id).await;
        assert_eq!(own.state, CleanupState::Swept);
        assert_eq!(own.rows_deleted, Some(fu17_counts(1, 1, 87_501, 0)));
        for i in 1..=10 {
            assert_eq!(
                fu17_ledger(&storage, ledger_base + i).await.state,
                CleanupState::Swept,
                "{i}"
            );
        }
        assert_eq!(
            fu17_ledger(&storage, ledger_base + 11).await.state,
            CleanupState::Detached,
            "the 100th statement went to the 10th older row"
        );
        assert_eq!(fu17_objects(&storage, older[10]).await, 1);
        assert_eq!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Absent
        );
        assert_eq!(
            fu17_ledger(&storage, ledger_base + 11).await.state,
            CleanupState::Swept
        );
        assert_eq!(fu17_objects(&storage, older[10]).await, 0);
    }

    /// The repository disappears between the entry's write-free pre-check and
    /// B3 (here: a concurrent delete of its row, which B3's row lock waits
    /// for): the detach round is `Done` without a ledger row, and the entry
    /// answers from the path's ledger.
    #[tokio::test]
    async fn fu17_detach_lost_race_falls_back_to_ledger() {
        use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};

        use crate::callisto::import_repo_cleanups::CleanupState;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu17-lost-race";
        let live = fu17_mount(&storage, path).await;
        let older = generate_id();
        let older_repo = fu17_pending_row(&storage, older, path).await;
        let rounds = format!("SELECT count(*) AS n FROM push_queue WHERE path = '{path}'");
        let before = fu17_count(&storage, &rounds).await;

        let conn = storage.git_db_storage().get_connection().clone();
        let deleting = conn.begin().await.unwrap();
        deleting
            .execute_unprepared(&format!("DELETE FROM git_repo WHERE id = {}", live.repo_id))
            .await
            .unwrap();
        let entry_storage = storage.clone();
        let removing =
            tokio::spawn(async move { fu17_remove(&entry_storage, path, None, None).await });
        let mut waited = false;
        for _ in 0..200 {
            // Inside a transaction `pg_stat_activity` is a snapshot taken at
            // its first read: discard it before every poll.
            deleting
                .execute_unprepared("SELECT pg_stat_clear_snapshot()")
                .await
                .unwrap();
            let row = deleting
                .query_one_raw(Statement::from_string(
                    DbBackend::Postgres,
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                     WHERE datname = current_database() AND wait_event_type = 'Lock' \
                     AND pg_backend_pid() = ANY(pg_blocking_pids(pid))) AS v",
                ))
                .await
                .unwrap()
                .unwrap();
            if row.try_get::<bool>("", "v").unwrap() {
                waited = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(waited, "B3 waits on the row being deleted");
        deleting.commit().await.unwrap();

        assert_eq!(
            removing.await.unwrap().unwrap(),
            RemoveOutcome::Absent,
            "nothing detached: the ledger answers"
        );
        assert_eq!(
            fu17_count(
                &storage,
                &format!(
                    "SELECT count(*) AS n FROM import_repo_cleanups WHERE repo_id = {}",
                    live.repo_id
                )
            )
            .await,
            0
        );
        assert_eq!(fu17_count(&storage, &rounds).await, before + 1);
        assert_eq!(
            fu17_count(
                &storage,
                &format!("{rounds} AND status = 'Done' AND kind = 'attach'")
            )
            .await,
            before + 1,
            "the detach round is Done"
        );
        assert_eq!(
            fu17_ledger(&storage, older).await.state,
            CleanupState::Swept
        );
        assert_eq!(fu17_objects(&storage, older_repo).await, 0);
    }

    // ---------------------------------------------------------------------
    // plan-20260923 FU-18: the receive-pack write fence.
    // ---------------------------------------------------------------------

    async fn fu18_import_repo(
        storage: &Storage,
        repo: &Repo,
        commands: Vec<RefCommand>,
    ) -> ImportRepo {
        ImportRepo {
            storage: storage.clone(),
            repo: repo.clone(),
            command_list: Mutex::new(commands),
            git_object_cache: disabled_cache().await,
            receive_pack_extra_timings_ms: Mutex::new(vec![]),
        }
    }

    /// A registered repository with `main` mounted at `path`; its tip.
    async fn fu18_mount(storage: &Storage, path: &str) -> (Repo, String) {
        let (repo, c1, create) = seed_import_repo_with_main_tip(storage, path).await;
        fu13_push(storage, &repo, vec![create]).await.unwrap();
        (repo, c1.id.to_string())
    }

    fn fu18_removed(path: &str) -> String {
        ImportRepoError::Removed {
            path: path.to_owned(),
        }
        .to_string()
    }

    async fn fu18_file_paths(storage: &Storage, repo_id: i64) -> Vec<(String, String)> {
        use sea_orm::{ConnectionTrait, DbBackend, Statement};
        storage
            .git_db_storage()
            .get_connection()
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                format!(
                    "SELECT blob_id, file_path FROM git_blob WHERE repo_id = {repo_id} ORDER BY blob_id"
                ),
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row.try_get("", "blob_id").unwrap(),
                    row.try_get("", "file_path").unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn fu18_receive_pack_writes_fenced() {
        use crate::jupiter::storage::git_db_storage::fu18_support::import_refs_count;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let git_db = storage.git_db_storage();
        let conn = git_db.get_connection().clone();
        let path = "/third-party/fu18-rp";
        let (repo, c1) = fu18_mount(&storage, path).await;
        let branch = |old: &str, new: &str, name: &str| {
            RefCommand::new(old.to_owned(), new.to_owned(), name.to_owned())
        };
        // While live: a delete-only round (Done, replayable later) and a tag.
        fu13_push(
            &storage,
            &repo,
            vec![branch(ZERO_ID, &c1, "refs/heads/topic")],
        )
        .await
        .unwrap();
        fu13_push(
            &storage,
            &repo,
            vec![branch(&c1, ZERO_ID, "refs/heads/topic")],
        )
        .await
        .unwrap();
        let ir = fu18_import_repo(&storage, &repo, vec![]).await;
        ir.update_refs(&branch(ZERO_ID, &c1, "refs/tags/v1"))
            .await
            .unwrap();

        // Detached, not swept: the objects are still there.
        let cleanup_id =
            detach_import_repo(&storage, disabled_cache().await, repo.repo_id, path, None)
                .await
                .unwrap()
                .expect("cleanup id");
        let root = fu17_root(&storage).await;
        let file_paths = fu18_file_paths(&storage, repo.repo_id).await;
        let audits = format!(
            "SELECT count(*) AS n FROM audit_logs WHERE target_id = {}",
            repo.repo_id
        );
        let audit_rows = fu17_count(&storage, &audits).await;
        let rounds = "SELECT count(*) AS n FROM push_queue";
        let removed = fu18_removed(path);

        // Tag writes, one transaction each.
        for tag in [
            branch(ZERO_ID, &c1, "refs/tags/v2"),
            branch(&c1, &"2".repeat(40), "refs/tags/v1"),
            branch(&c1, ZERO_ID, "refs/tags/v1"),
        ] {
            assert_eq!(ir.update_refs(&tag).await.unwrap_err().to_string(), removed);
        }
        // A fresh delete-only round reaches B3's fence.
        assert_eq!(
            fu13_push(
                &storage,
                &repo,
                vec![branch(&c1, ZERO_ID, "refs/heads/main")]
            )
            .await
            .unwrap_err(),
            removed
        );
        // The replay of the Done delete-only round answers without B3.
        let before = fu17_count(&storage, rounds).await;
        assert_eq!(
            fu13_push(
                &storage,
                &repo,
                vec![branch(&c1, ZERO_ID, "refs/heads/topic")]
            )
            .await
            .unwrap_err(),
            removed
        );
        assert_eq!(fu17_count(&storage, rounds).await, before, "no new round");
        // A stale first mount of another branch.
        assert_eq!(
            fu13_push(
                &storage,
                &repo,
                vec![branch(ZERO_ID, &c1, "refs/heads/topic2")]
            )
            .await
            .unwrap_err(),
            removed
        );
        // The file_path step: pre-check and the fenced chunk writer.
        let finalize =
            fu18_import_repo(&storage, &repo, vec![branch(&c1, &c1, "refs/heads/main")]).await;
        assert_eq!(
            finalize
                .traverses_tree_and_update_filepath()
                .await
                .unwrap_err()
                .to_string(),
            removed
        );
        let readme = Blob::from_content("hello from import repo").id.to_string();
        assert_eq!(
            git_db
                .update_import_blob_filepaths_fenced(repo.repo_id, path, vec![(readme, "x".into())])
                .await
                .unwrap_err()
                .to_string(),
            removed
        );

        assert_eq!(import_refs_count(&conn, repo.repo_id).await, 0);
        assert_eq!(
            fu17_root(&storage).await,
            root,
            "the leaf is not mounted again"
        );
        assert_eq!(fu17_count(&storage, &audits).await, audit_rows);
        assert_eq!(fu18_file_paths(&storage, repo.repo_id).await, file_paths);

        // Swept: the objects are gone, the file_path step still answers
        // REMOVED instead of failing on a missing commit.
        assert!(matches!(
            fu17_remove(&storage, path, None, Some(cleanup_id)).await.unwrap(),
            RemoveOutcome::Removed { repo_id, .. } if repo_id == repo.repo_id
        ));
        assert_eq!(fu17_objects(&storage, repo.repo_id).await, 0);
        assert_eq!(
            finalize
                .traverses_tree_and_update_filepath()
                .await
                .unwrap_err()
                .to_string(),
            removed
        );
    }

    #[tokio::test]
    async fn fu18_tag_lock_serializes_with_detach() {
        use sea_orm::TransactionTrait;

        use crate::jupiter::storage::git_db_storage::fu18_support::{
            blocked_by_me, import_refs_count, park_detach, single_connection,
        };

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let git_db = storage.git_db_storage();
        let conn = git_db.get_connection().clone();

        // The tag first: the detach waits for it, then deletes the tag with
        // the repository's other refs.
        let path = "/third-party/fu18-tag";
        let (repo, c1) = fu18_mount(&storage, path).await;
        let ir = fu18_import_repo(&storage, &repo, vec![]).await;
        let tag = conn.begin().await.unwrap();
        assert!(
            ir.write_tag_ref_in_txn(
                &RefCommand::new(ZERO_ID.to_owned(), c1.clone(), "refs/tags/held".to_owned()),
                &tag,
            )
            .await
            .unwrap()
        );
        let detach_storage = storage.clone();
        let repo_id = repo.repo_id;
        let detaching = tokio::spawn(async move {
            detach_import_repo(
                &detach_storage,
                disabled_cache().await,
                repo_id,
                "/third-party/fu18-tag",
                None,
            )
            .await
        });
        assert!(
            blocked_by_me(&tag, false).await,
            "the detach waits on the tag"
        );
        tag.commit().await.unwrap();
        assert!(detaching.await.unwrap().unwrap().is_some());
        assert_eq!(import_refs_count(&conn, repo.repo_id).await, 0);
        assert!(
            git_db
                .find_git_repo_exact_match(path)
                .await
                .unwrap()
                .is_none()
        );

        // The detach first (parked at its import_refs delete): the tag waits
        // on it and then lands nothing.
        let path2 = "/third-party/fu18-tag2";
        let (repo2, c2) = fu18_mount(&storage, path2).await;
        let single = single_connection(&conn).await;
        let parked = park_detach(&single, repo2.repo_id).await;
        let detach_storage = storage.clone();
        let repo2_id = repo2.repo_id;
        let detaching = tokio::spawn(async move {
            detach_import_repo(
                &detach_storage,
                disabled_cache().await,
                repo2_id,
                "/third-party/fu18-tag2",
                None,
            )
            .await
        });
        assert!(
            blocked_by_me(&parked, false).await,
            "the detach parks at its import_refs delete"
        );
        let tag_storage = storage.clone();
        let tag_repo = repo2.clone();
        let tagging = tokio::spawn(async move {
            fu18_import_repo(&tag_storage, &tag_repo, vec![])
                .await
                .update_refs(&RefCommand::new(
                    ZERO_ID.to_owned(),
                    c2,
                    "refs/tags/late".to_owned(),
                ))
                .await
                .map_err(|e| e.to_string())
        });
        assert!(
            blocked_by_me(&parked, true).await,
            "the tag waits on the detach"
        );
        parked.commit().await.unwrap();
        assert!(detaching.await.unwrap().unwrap().is_some());
        assert_eq!(tagging.await.unwrap().unwrap_err(), fu18_removed(path2));
        assert_eq!(import_refs_count(&conn, repo2.repo_id).await, 0);
    }

    #[tokio::test]
    async fn fu18_unpack_refusal_keeps_code() {
        use git_internal::hash::ObjectHash;
        use tokio::sync::mpsc::unbounded_channel;

        use crate::jupiter::storage::git_db_storage::fu18_support::object_counts;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu18-unpack";
        let (repo, _) = fu18_mount(&storage, path).await;
        assert!(matches!(
            fu17_remove(&storage, path, None, None).await.unwrap(),
            RemoveOutcome::Removed { .. }
        ));
        // A stale unpack: the batch is refused with its stable code, not
        // wrapped as a generic save failure.
        let ir = Arc::new(fu18_import_repo(&storage, &repo, vec![]).await);
        let (entries, rx) = unbounded_channel();
        let (_pack_ids, rx_pack) = unbounded_channel::<ObjectHash>();
        entries
            .send(MetaAttached {
                inner: Blob::from_content("fu18 unpack").into(),
                meta: EntryMeta::new(),
            })
            .unwrap();
        drop(entries);
        let refused = ir.receiver_handler(rx, rx_pack).await.unwrap_err();
        assert!(
            matches!(
                refused,
                MegaError::ImportRepo(ImportRepoError::Removed { .. })
            ),
            "{refused:?}"
        );
        assert_eq!(refused.to_string(), fu18_removed(path));
        assert_eq!(
            object_counts(storage.git_db_storage().get_connection(), repo.repo_id).await,
            [0, 0, 0, 0]
        );
    }

    #[tokio::test]
    async fn fu18_filepath_errors_not_panics() {
        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let readme = Blob::from_content("hello from import repo").id.to_string();

        // Live and complete: the step writes as before.
        let (live, c1) = fu18_mount(&storage, "/third-party/fu18-fp-live").await;
        let main = |old: &str, new: &str| {
            RefCommand::new(old.to_owned(), new.to_owned(), "refs/heads/main".to_owned())
        };
        fu18_import_repo(&storage, &live, vec![main(&c1, &c1)])
            .await
            .traverses_tree_and_update_filepath()
            .await
            .unwrap();
        assert!(
            fu18_file_paths(&storage, live.repo_id)
                .await
                .contains(&(readme.clone(), "README.md".to_owned()))
        );

        // Live but the head commit is unknown: an error naming it, no panic.
        let (repo, c1) = fu18_mount(&storage, "/third-party/fu18-fp").await;
        let unknown = "e".repeat(40);
        let missing = fu18_import_repo(&storage, &repo, vec![main(&c1, &unknown)])
            .await
            .traverses_tree_and_update_filepath()
            .await
            .unwrap_err();
        assert!(matches!(missing, MegaError::NotFound(_)), "{missing:?}");
        assert!(missing.to_string().contains(&unknown), "{missing}");

        // Live, the head commit present but a tree missing (the root, then a
        // subtree): errors naming the tree, no panic.
        let (broken, b1) = fu18_mount(&storage, "/third-party/fu18-fp-trees").await;
        let ghost = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: Blob::from_content("fu18 ghost").id,
            name: "ghost.txt".to_string(),
        }])
        .unwrap();
        let root = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Tree,
            id: ghost.id,
            name: "sub".to_string(),
        }])
        .unwrap();
        let with_ghost = Commit::from_tree_id(root.id, vec![], "fu18 ghost subtree");
        storage
            .import_service
            .save_entry(
                broken.repo_id,
                &broken.repo_path,
                vec![
                    MetaAttached {
                        inner: root.into(),
                        meta: EntryMeta::new(),
                    },
                    MetaAttached {
                        inner: with_ghost.clone().into(),
                        meta: EntryMeta::new(),
                    },
                ],
            )
            .await
            .unwrap();
        let subtree = fu18_import_repo(
            &storage,
            &broken,
            vec![main(&b1, &with_ghost.id.to_string())],
        )
        .await
        .traverses_tree_and_update_filepath()
        .await
        .unwrap_err();
        assert!(matches!(subtree, MegaError::NotFound(_)), "{subtree:?}");
        assert!(
            subtree.to_string().contains(&ghost.id.to_string()),
            "{subtree}"
        );
        let b1_tree = Commit::from_git_model(
            storage
                .git_db_storage()
                .get_commit_by_hash(broken.repo_id, &b1)
                .await
                .unwrap()
                .unwrap(),
        )
        .tree_id
        .to_string();
        fu17_exec(
            &storage,
            &format!(
                "DELETE FROM git_tree WHERE repo_id = {} AND tree_id = '{b1_tree}'",
                broken.repo_id
            ),
        )
        .await;
        let root_missing = fu18_import_repo(&storage, &broken, vec![main(&b1, &b1)])
            .await
            .traverses_tree_and_update_filepath()
            .await
            .unwrap_err();
        assert!(
            matches!(root_missing, MegaError::NotFound(_)),
            "{root_missing:?}"
        );
        assert!(
            root_missing.to_string().contains(&b1_tree),
            "{root_missing}"
        );

        // Detached: a missing object reads as REMOVED.
        detach_import_repo(
            &storage,
            disabled_cache().await,
            repo.repo_id,
            &repo.repo_path,
            None,
        )
        .await
        .unwrap()
        .expect("cleanup id");
        let ir = fu18_import_repo(&storage, &repo, vec![]).await;
        assert_eq!(
            ir.removed_or(MegaError::NotFound("tree x".into()))
                .await
                .to_string(),
            fu18_removed(&repo.repo_path)
        );

        // Detached: the step answers before reading any object. With the
        // commit and tree tables locked by another transaction, a read would
        // wait; the liveness check reads only `git_repo`.
        use sea_orm::{ConnectionTrait, TransactionTrait};
        let conn = storage.git_db_storage().get_connection().clone();
        let holder = conn.begin().await.unwrap();
        holder
            .execute_unprepared("LOCK TABLE git_commit, git_tree IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let finalize = fu18_import_repo(&storage, &repo, vec![main(&c1, &c1)]).await;
        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            finalize.traverses_tree_and_update_filepath(),
        )
        .await;
        holder.rollback().await.unwrap();
        assert_eq!(
            answered
                .expect("no object read before the liveness check")
                .unwrap_err()
                .to_string(),
            fu18_removed(&repo.repo_path)
        );
    }

    #[test]
    fn fu18_attach_refusal_rebuilds_removed() {
        let payload = AttachPayload {
            op: AttachOp::Attach,
            repo_id: 1,
            repo_path: "/third-party/x".to_owned(),
            commands: vec![],
        };
        let refused = super::attach_refusal(&payload, 9, &fu18_removed("/third-party/x"));
        assert!(
            matches!(
                refused,
                MegaError::ImportRepo(ImportRepoError::Removed { ref path })
                    if path == "/third-party/x"
            ),
            "{refused:?}"
        );
    }

    /// The repository goes away after the step's liveness check but before
    /// its object reads (here: while they wait on a table lock): the missing
    /// commit reads as REMOVED, not as a missing object.
    #[tokio::test]
    async fn fu18_filepath_removed_between_check_and_reads() {
        use sea_orm::{ConnectionTrait, TransactionTrait};

        use crate::jupiter::storage::git_db_storage::fu18_support::blocked_by_me;

        let temp = tempfile::tempdir().unwrap();
        let storage = wired_storage_with_monorepo(&temp).await;
        let path = "/third-party/fu18-fp-race";
        let (repo, c1) = fu18_mount(&storage, path).await;
        let conn = storage.git_db_storage().get_connection().clone();
        let holder = conn.begin().await.unwrap();
        holder
            .execute_unprepared("LOCK TABLE git_commit IN ACCESS EXCLUSIVE MODE")
            .await
            .unwrap();
        let step_storage = storage.clone();
        let step_repo = repo.clone();
        let step = tokio::spawn(async move {
            let head = RefCommand::new(c1.clone(), c1, "refs/heads/main".to_owned());
            fu18_import_repo(&step_storage, &step_repo, vec![head])
                .await
                .traverses_tree_and_update_filepath()
                .await
                .map_err(|e| e.to_string())
        });
        assert!(
            blocked_by_me(&holder, false).await,
            "the step passed its liveness check and waits on the commit read"
        );
        // What a detach and a sweep delete, committed while the step waits.
        for sql in [
            format!("DELETE FROM git_repo WHERE id = {}", repo.repo_id),
            format!("DELETE FROM git_commit WHERE repo_id = {}", repo.repo_id),
        ] {
            holder.execute_unprepared(&sql).await.unwrap();
        }
        holder.commit().await.unwrap();
        assert_eq!(step.await.unwrap().unwrap_err(), fu18_removed(path));
    }
}
