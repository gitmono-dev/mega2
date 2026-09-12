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
use tokio::sync::mpsc::{self, Sender};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    callisto::sea_orm_active_enums::RefTypeEnum,
    ceres::{
        api_service::cache::GitObjectCache,
        pack::RepoHandler,
        protocol::{
            import_refs::{CommandType, RefCommand, Refs},
            repo::Repo,
        },
    },
    common::{
        errors::MegaError,
        utils::{ZERO_ID, is_protocol_zero_id},
    },
    jupiter::{
        service::{
            git_service::GitService,
            push_queue_service::{
                AttachCommand, AttachExecContext, AttachPayload, EnqueueRequest, ExecuteOutcome,
                ExecuteRequest, QueueWaitResult, attach_operation_id, normalize_attach_commands,
            },
        },
        storage::{Storage, base_storage::StorageConnector, git_db_storage::GitDbStorage},
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
            .save_entry(self.repo.repo_id, entry_list)
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
        let storage = self.storage.git_db_storage();
        match refs.command_type {
            CommandType::Create => {
                storage
                    .save_ref(self.repo.repo_id, refs.clone().into())
                    .await
                    .map_err(|e| GitError::CustomError(e.to_string()))?;
            }
            CommandType::Delete => {
                let deleted = storage
                    .remove_ref_if_unchanged(
                        self.repo.repo_id,
                        &refs.ref_name,
                        &refs.old_id,
                        storage.get_connection(),
                    )
                    .await
                    .map_err(|e| GitError::CustomError(e.to_string()))?;
                if !deleted {
                    return Err(GitError::CustomError(format!(
                        "tag {} moved since advertisement (expected {})",
                        refs.ref_name, refs.old_id
                    )));
                }
            }
            CommandType::Update => {
                storage
                    .update_ref(self.repo.repo_id, &refs.ref_name, &refs.new_id)
                    .await
                    .map_err(|e| GitError::CustomError(e.to_string()))?;
            }
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
        let commit = Commit::from_git_model(
            self.storage
                .git_db_storage()
                .get_commit_by_hash(self.repo.repo_id, &current_head)
                .await?
                .unwrap(),
        );

        let root_tree = Tree::from_git_model(
            self.storage
                .git_db_storage()
                .get_tree_by_hash(self.repo.repo_id, &commit.tree_id.to_string())
                .await?
                .unwrap()
                .clone(),
        );
        let pairs = collect_git_blob_filepaths(
            self.storage.git_db_storage(),
            self.repo.repo_id,
            root_tree,
            PathBuf::new(),
        )
        .await?;
        self.storage
            .git_db_storage()
            .update_git_blob_filepaths(self.repo.repo_id, pairs)
            .await?;
        Ok(())
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
                    .unwrap()
                    .clone(),
            );
            pairs.extend(
                collect_git_blob_filepaths(storage.clone(), repo_id, child, path.join(item.name))
                    .await?,
            );
        } else {
            pairs.push((
                item.id.to_string(),
                path.join(item.name).to_str().unwrap().to_string(),
            ));
        }
    }
    Ok(pairs)
}

impl ImportRepo {
    // attach import repo to monorepo parent tree via MonoWriteQueue (TP-08).
    pub(crate) async fn attach_to_monorepo_parent(&self) -> Result<(), MegaError> {
        // Snapshot commands without holding the mutex across await (Send + avoids deadlocks).
        let commands_snapshot: Vec<RefCommand> = self
            .command_list
            .lock()
            .expect("command_list lock poisoned")
            .clone();
        // Pure delete-only attach: no non-zero branch tip → do not enqueue.
        if !commands_snapshot.iter().any(|c| {
            c.status == "ok" && c.ref_type == RefTypeEnum::Branch && !is_protocol_zero_id(&c.new_id)
        }) {
            let txn = self.storage.begin_db_transaction().await?;
            let git_db = self.storage.git_db_storage();
            for cmd in &commands_snapshot {
                if cmd.status != "ok" || cmd.ref_type != RefTypeEnum::Branch {
                    continue;
                }
                if let CommandType::Delete = cmd.command_type {
                    let deleted = git_db
                        .remove_ref_if_unchanged(
                            self.repo.repo_id,
                            &cmd.ref_name,
                            &cmd.old_id,
                            &txn,
                        )
                        .await?;
                    if !deleted {
                        return Err(MegaError::Other(format!(
                            "ref {} moved since advertisement (expected {})",
                            cmd.ref_name, cmd.old_id
                        )));
                    }
                }
            }
            txn.commit().await.map_err(MegaError::Db)?;
            return Ok(());
        }

        let commit_id = commands_snapshot
            .iter()
            .find(|c| {
                c.status == "ok"
                    && c.ref_type == RefTypeEnum::Branch
                    && !is_protocol_zero_id(&c.new_id)
            })
            .map(|c| c.new_id.clone())
            .ok_or_else(|| MegaError::Other("attach: no branch tip".into()))?;

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
        let operation_id = attach_operation_id(
            &self.repo.repo_id.to_string(),
            &normalize_attach_commands(&fingerprint_rows),
        );
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
                operation_id,
                path,
                old_id,
                new_id: commit_id,
                requester: None,
                payload: serde_json::to_value(&payload)
                    .map_err(|e| MegaError::Other(format!("attach payload encode: {e}")))?,
                ref_name: None,
                is_delete: false,
            })
            .await?;

        match wait {
            QueueWaitResult::Replayed { .. } => Ok(()),
            QueueWaitResult::Abandoned { id } => Err(MegaError::Other(format!(
                "attach wait abandoned for push_queue id {id}"
            ))),
            QueueWaitResult::Rejected { id, message } => Err(MegaError::Other(format!(
                "attach rejected for push_queue id {id}: {message}"
            ))),
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
                    other => Err(MegaError::Other(format!(
                        "attach B3 did not complete successfully: {other:?}"
                    ))),
                }
            }
        }
    }
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

    use super::{ImportRepo, collect_git_blob_filepaths};
    use crate::{
        callisto::{
            git_blob, git_tree, import_refs, push_queue, queue_control,
            sea_orm_active_enums::{PushQueueKindEnum, RefTypeEnum},
        },
        ceres::{
            api_service::cache::GitObjectCache,
            protocol::{
                import_refs::{CommandType, RefCommand},
                repo::Repo,
            },
        },
        common::utils::{ZERO_ID, generate_id},
        config::RedisConfig,
        jupiter::{
            migration::apply_migrations,
            redis::init_connection,
            service::{
                git_service::GitService,
                import_service::ImportService,
                mono_service::MonoService,
                push_queue_service::{
                    AttachCommand, AttachExecContext, AttachPayload, EnqueueRequest,
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
        let repo = Repo::new(PathBuf::from(path), false).unwrap();
        let repo_id = repo.repo_id;
        let readme = Blob::from_content("hello from import repo");
        let tree = Tree::from_tree_items(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: readme.id,
            name: "README.md".to_string(),
        }])
        .unwrap();
        let commit = Commit::from_tree_id(tree.id, vec![], "\nimport commit");
        storage
            .import_service
            .save_entry(
                repo_id,
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

    /// Pure delete-only attach must not enqueue into MonoWriteQueue.
    #[tokio::test]
    async fn attach_delete_only_does_not_enqueue() {
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
        let queued = push_queue::Entity::find()
            .count(storage.git_db_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(queued, 0, "delete-only attach must not enqueue");
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
            err.to_string().contains("moved since advertisement"),
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
}
