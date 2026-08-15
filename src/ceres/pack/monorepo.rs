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
        object::{
            ObjectTrait, blob::Blob, commit::Commit, signature::Signature, tree::Tree,
            types::ObjectType,
        },
        pack::{encode::PackEncoder, entry::Entry},
    },
};
use orbit_api::{error::IoOrbitError, object_storage::MultiObjectByteStream};
use sea_orm::DatabaseTransaction;
use tokio::sync::{RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    bellatrix::Bellatrix,
    callisto::{
        entity_ext::generate_link,
        mega_cl, mega_code_review_anchor, mega_commit, mega_refs,
        sea_orm_active_enums::{PositionStatusEnum, RefTypeEnum},
    },
    ceres::{
        api_service::{ApiHandler, cache::GitObjectCache, mono_api_service::MonoApiService},
        code_edit::{on_push::OnpushCodeEdit, utils::get_changed_files},
        model::change_list::ClDiffFile,
        pack::RepoHandler,
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
    jupiter::{storage::Storage, utils::converter::FromMegaModel},
};

pub struct MonoRepo {
    pub storage: Storage,
    pub git_object_cache: Arc<GitObjectCache>,
    pub path: PathBuf,
    pub base_branch: String,
    pub from_hash: String,
    pub to_hash: String,
    // current_commit only exists when an unpack operation occurs.
    // When only a branch is updated and the pack file is empty, this value will be None.
    pub current_commit: Arc<RwLock<Option<Commit>>>,
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

                let new_mega_ref = mega_refs::Model::new(
                    &self.path,
                    root_ref.ref_name.clone(),
                    c.id.to_string(),
                    c.tree_id.to_string(),
                    false,
                );

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
        self.persist_mono_branch_cl_mega_refs_transaction().await?;
        self.run_mono_post_push_pipeline().await
    }

    async fn save_entry(
        &self,
        entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<(), MegaError> {
        let current_commit = self.current_commit.read().await;
        let commit_id = if let Some(commit) = &*current_commit {
            commit.id.to_string()
        } else {
            String::new()
        };
        let commit_models = self
            .storage
            .mono_service
            .save_entry(&commit_id, entry_list)
            .await?;

        if !commit_models.is_empty() {
            let commits_to_process: Result<Vec<(String, String)>, MegaError> = commit_models
                .into_iter()
                .map(|c| {
                    let model: mega_commit::Model = c.try_into()?;
                    let author_bytes = model.author.as_deref().unwrap_or("").as_bytes();
                    let signature = Signature::from_data(author_bytes.to_vec())?;
                    Ok((model.commit_id, signature.email))
                })
                .collect();

            self.storage
                .mono_storage()
                .process_commit_bindings(&commits_to_process?, self.username.clone().as_deref())
                .await?;
        }
        Ok(())
    }

    async fn update_pack_id(&self, temp_pack_id: &str, pack_id: &str) -> Result<(), MegaError> {
        let storage = self.storage.mono_storage();
        storage.update_pack_id(temp_pack_id, pack_id).await
    }

    async fn check_entry(&self, entry: &Entry) -> Result<(), GitError> {
        if self.current_commit.read().await.is_none() {
            if entry.obj_type == ObjectType::Commit {
                let commit = Commit::from_bytes(&entry.data, entry.hash).unwrap();
                let mut current = self.current_commit.write().await;
                *current = Some(commit);
            }
        } else if entry.obj_type == ObjectType::Commit {
            return Err(GitError::CustomError(
                "only single commit support in each push".to_string(),
            ));
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
            .unwrap()
            .is_some()
    }

    async fn check_default_branch(&self) -> bool {
        true
    }

    async fn traverses_tree_and_update_filepath(&self) -> Result<(), MegaError> {
        let commit_guard = self.current_commit.read().await;
        let commit_opt = match commit_guard.as_ref() {
            Some(commit) => commit,
            None => {
                tracing::info!(
                    "Skipping file path update: no current commit available. \
                     This typically occurs when only updating references or pushing empty pack files."
                );
                return Ok(());
            }
        };

        let tree_hashes = vec![commit_opt.tree_id.to_string()];
        let trees = self
            .storage
            .mono_storage()
            .get_trees_by_hashes(tree_hashes)
            .await
            .map_err(|e| {
                MegaError::Other(format!(
                    "Failed to retrieve root tree for commit {}: {}",
                    commit_opt.id, e
                ))
            })?;

        if trees.is_empty() {
            return Err(MegaError::Other(format!(
                "Root tree {} not found for commit {}",
                commit_opt.tree_id, commit_opt.id
            )));
        }

        let root_tree = Tree::from_mega_model(trees[0].clone());

        tracing::info!(
            "Starting file path update for commit {} with root tree {}",
            commit_opt.id,
            commit_opt.tree_id
        );

        self.traverses_and_update_filepath(root_tree, PathBuf::new())
            .await
            .map_err(|e| {
                MegaError::Other(format!(
                    "Failed to update file paths for commit {}: {}",
                    commit_opt.id, e
                ))
            })?;

        tracing::info!(
            "Successfully completed file path update for commit {}",
            commit_opt.id
        );

        Ok(())
    }
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
            if cmd.ref_type == RefTypeEnum::Branch {
                self.apply_cl_mega_ref_for_push_command(cmd, Some(&txn))
                    .await?;
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
            let existing = match txn {
                Some(t) => storage.get_ref_by_name_in_txn(&cmd.ref_name, t).await?,
                None => storage.get_ref_by_name(&cmd.ref_name).await?,
            };
            if let Some(existing) = existing {
                storage.remove_ref(existing).await?;
            }
            return Ok(());
        }

        let current_commit = self.current_commit.read().await;
        let Some(c) = &*current_commit else {
            return Ok(());
        };
        let from_hash = Self::effective_from_hash(&self.from_hash, c)?;
        let cl_link = self.fetch_or_new_cl_link(&from_hash).await?;
        let ref_name = utils::cl_ref_name(&cl_link);

        let existing = match txn {
            Some(t) => storage.get_ref_by_name_in_txn(&ref_name, t).await?,
            None => storage.get_ref_by_name(&ref_name).await?,
        };

        if let Some(mut cl_ref) = existing {
            cl_ref.ref_commit_hash = cmd.new_id.clone();
            cl_ref.ref_tree_hash = c.tree_id.to_string();
            storage.update_ref(cl_ref, txn).await?;
        } else {
            let new_ref = mega_refs::Model::new(
                &self.path,
                ref_name,
                cmd.new_id.clone(),
                c.tree_id.to_string(),
                true,
            );
            storage.save_refs(new_ref, txn).await?;
        }
        Ok(())
    }

    /// CL / conversations / build / code-review hooks after branch `mega_refs` are committed.
    async fn run_mono_post_push_pipeline(&self) -> Result<(), MegaError> {
        let cmds = self
            .command_list
            .lock()
            .expect("command_list lock poisoned")
            .clone();
        if !cmds.iter().any(|cmd| cmd.ref_type == RefTypeEnum::Branch) {
            return Ok(());
        }
        let current_commit = self.current_commit.read().await;
        let Some(current_commit) = &*current_commit else {
            return Ok(());
        };
        let from_hash = Self::effective_from_hash(&self.from_hash, current_commit)?;
        let username = self.username();
        let mono_api_service = self.into();
        let editor = OnpushCodeEdit::from(
            self.path.to_str().unwrap(),
            &self.base_branch,
            &from_hash,
            &mono_api_service,
        );
        let cl = editor
            .update_or_create_cl(&self.storage, &from_hash, &self.to_hash, &username)
            .await?;
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
        self.reanchor_code_review_threads(&cl).await
    }

    #[async_recursion]
    async fn traverses_and_update_filepath(
        &self,
        tree: Tree,
        path: PathBuf,
    ) -> Result<(), MegaError> {
        for item in tree.tree_items {
            let item_path = path.join(&item.name);

            if item.is_tree() {
                let tree_hash = item.id.to_string();
                let trees = self
                    .storage
                    .mono_storage()
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

                self.traverses_and_update_filepath(child_tree, item_path.clone())
                    .await
                    .map_err(|e| {
                        MegaError::Other(format!(
                            "Failed to process subtree {} at path '{}': {}",
                            tree_hash,
                            item_path.display(),
                            e
                        ))
                    })?;
            } else {
                let blob_id = item.id.to_string();
                let file_path_str = item_path.to_str().ok_or_else(|| {
                    MegaError::Other(format!(
                        "Invalid UTF-8 path for blob {}: '{}'",
                        blob_id,
                        item_path.display()
                    ))
                })?;

                self.storage
                    .mono_storage()
                    .update_blob_filepath(&blob_id, file_path_str)
                    .await
                    .map_err(|e| {
                        MegaError::Other(format!(
                            "Failed to update file path for blob {} at '{}': {}",
                            blob_id, file_path_str, e
                        ))
                    })?;

                tracing::debug!(
                    "Updated file path for blob {} to '{}'",
                    blob_id,
                    file_path_str
                );
            }
        }

        Ok(())
    }
    fn effective_from_hash(from_hash: &str, current_commit: &Commit) -> Result<String, MegaError> {
        if from_hash != ZERO_ID {
            return Ok(from_hash.to_owned());
        }
        current_commit
            .parent_commit_ids
            .first()
            .map(ToString::to_string)
            .ok_or_else(|| {
                MegaError::Other("Can not init directory under monorepo directory!".to_string())
            })
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
    pub async fn reanchor_code_review_threads(&self, cl: &mega_cl::Model) -> Result<(), MegaError> {
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
                let to_hash = self.to_hash.clone();

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
                                &self.to_hash,
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use git_internal::{
        hash::ObjectHash,
        internal::object::{
            commit::Commit,
            signature::{Signature, SignatureType},
        },
    };

    use super::MonoRepo;
    use crate::common::utils::ZERO_ID;

    fn test_signature(signature_type: SignatureType) -> Signature {
        Signature::new(
            signature_type,
            "Monoengine Test".to_string(),
            "monoengine-test@example.invalid".to_string(),
        )
    }

    fn test_commit(parent_commit_ids: Vec<ObjectHash>) -> Commit {
        let tree_id = ObjectHash::from_str("27dd8d4cf39f3868c6eee38b601bc9e9939304f5").unwrap();
        Commit::new(
            test_signature(SignatureType::Author),
            test_signature(SignatureType::Committer),
            tree_id,
            parent_commit_ids,
            "test commit",
        )
    }

    #[test]
    fn effective_from_hash_keeps_existing_ref_old_id() {
        let old_id = "119bc457cb05b52dfb0d6b14f66d9a8a52d09e25";
        let parent = ObjectHash::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let commit = test_commit(vec![parent]);

        let effective = MonoRepo::effective_from_hash(old_id, &commit).unwrap();

        assert_eq!(effective, old_id);
    }

    #[test]
    fn effective_from_hash_uses_first_parent_for_new_branch_push() {
        let parent = ObjectHash::from_str("119bc457cb05b52dfb0d6b14f66d9a8a52d09e25").unwrap();
        let commit = test_commit(vec![parent]);

        let effective = MonoRepo::effective_from_hash(ZERO_ID, &commit).unwrap();

        assert_eq!(effective, parent.to_string());
    }

    #[test]
    fn effective_from_hash_rejects_orphan_new_branch_push() {
        let commit = test_commit(Vec::new());

        let err = MonoRepo::effective_from_hash(ZERO_ID, &commit).unwrap_err();

        assert!(
            err.to_string()
                .contains("Can not init directory under monorepo directory")
        );
    }
}
