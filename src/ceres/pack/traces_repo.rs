use std::collections::HashMap;

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
    },
    common::errors::MegaError,
    contract::git_protocol::path::{TracesLocator, TracesObjectFormat},
    orbit_api::object_storage::MultiObjectByteStream,
};

const CODE_WRITE_REFUSED: &str = "trace repository does not write code objects";

/// Virtual repository selected before Native and ImportRepo.
///
/// Discovery of a legal locator returns an empty ref list. This handler does
/// not create `git_repo` or `.gitkeep`, and it does not call MonoWriteQueue,
/// PushChain, or CL writers.
pub struct TraceRepo {
    locator: TracesLocator,
    hash_kind: HashKind,
}

impl TraceRepo {
    pub fn new(locator: TracesLocator, session_hash_kind: HashKind) -> Self {
        let hash_kind = match &locator {
            TracesLocator::ByPath { hash_kind, .. } => match hash_kind {
                TracesObjectFormat::Sha1 => HashKind::Sha1,
                TracesObjectFormat::Sha256 => HashKind::Sha256,
            },
            TracesLocator::ByScope { .. } => session_hash_kind,
        };
        Self { locator, hash_kind }
    }

    fn zero_head(&self) -> String {
        match &self.locator {
            TracesLocator::ByPath { hash_kind, .. } => hash_kind.zero_id(),
            TracesLocator::ByScope { .. } => "0".repeat(self.hash_kind.hex_len()),
        }
    }
}

fn refused_mega() -> MegaError {
    MegaError::Other(CODE_WRITE_REFUSED.to_owned())
}

fn refused_git() -> GitError {
    GitError::CustomError(CODE_WRITE_REFUSED.to_owned())
}

#[async_trait]
impl RepoHandler for TraceRepo {
    fn is_monorepo(&self) -> bool {
        false
    }

    fn is_trace_repo(&self) -> bool {
        true
    }

    fn object_hash_kind(&self) -> Result<HashKind, MegaError> {
        Ok(self.hash_kind)
    }

    fn bind_tip_after_receive(&self) -> bool {
        false
    }

    async fn refs_with_head_hash(&self) -> Result<(String, Vec<Refs>), MegaError> {
        // A legal but absent scope advertises no refs and does not invent
        // `HEAD -> refs/heads/main`. Capability lines stay with the protocol.
        Ok((self.zero_head(), Vec::new()))
    }

    async fn finalize_receive_pack(&self) -> Result<(), MegaError> {
        Err(refused_mega())
    }

    async fn save_entry(
        &self,
        _entry_list: Vec<MetaAttached<Entry, EntryMeta>>,
    ) -> Result<(), MegaError> {
        Err(refused_mega())
    }

    async fn update_pack_id(&self, _temp_pack_id: &str, _pack_id: &str) -> Result<(), MegaError> {
        Err(refused_mega())
    }

    async fn check_entry(&self, _entry: &Entry) -> Result<(), GitError> {
        Err(refused_git())
    }

    async fn full_pack(&self, _want: Vec<String>) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        Err(refused_git())
    }

    async fn incremental_pack(
        &self,
        _want: Vec<String>,
        _have: Vec<String>,
    ) -> Result<ReceiverStream<Vec<u8>>, GitError> {
        Err(refused_git())
    }

    async fn get_trees_by_hashes(&self, _hashes: Vec<String>) -> Result<Vec<Tree>, MegaError> {
        Err(refused_mega())
    }

    async fn get_blobs_by_hashes(
        &self,
        _hashes: Vec<String>,
    ) -> Result<MultiObjectByteStream<'_>, MegaError> {
        Err(refused_mega())
    }

    async fn get_blob_metadata_by_hashes(
        &self,
        _hashes: Vec<String>,
    ) -> Result<HashMap<String, EntryMeta>, MegaError> {
        Err(refused_mega())
    }

    async fn update_refs(&self, _refs: &RefCommand) -> Result<(), GitError> {
        Err(refused_git())
    }

    async fn check_commit_exist(&self, _hash: &str) -> bool {
        false
    }

    async fn check_object_exist(&self, _hash: &str) -> bool {
        false
    }

    async fn check_default_branch(&self) -> bool {
        false
    }

    async fn traverses_tree_and_update_filepath(&self) -> Result<(), MegaError> {
        Err(refused_mega())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    use tempfile::TempDir;

    use super::*;
    use crate::{
        callisto::{git_repo, mega_blob, mega_cl, push_queue, trace_stream, trace_target},
        ceres::{
            api_service::{cache::GitObjectCache, state::ProtocolApiState},
            protocol::{ServiceType, SmartSession, TransportProtocol},
        },
        common::utils::is_protocol_zero_id,
        config::testing::isolated_config,
        contract::{
            git_protocol::path::{TracesLocator, TracesObjectFormat},
            policy::entitystore::SharedEntityStore,
        },
        jupiter::{storage::base_storage::StorageConnector, tests::test_storage_with_config},
    };

    const STREAM_ID: &str = "abcdef01-2345-4678-89ab-cdef01234567";

    struct WriterCounts {
        git_repo: u64,
        gitkeep: u64,
        push_queue: u64,
        mega_cl: u64,
        trace_stream: u64,
        trace_target: u64,
    }

    async fn protocol_state(temp: &TempDir) -> ProtocolApiState {
        let mut config = isolated_config(temp.path().join("cfg"));
        config.monorepo.import_dir = std::path::PathBuf::from("/.traces");
        let storage = test_storage_with_config(temp.path(), config).await;
        let redis_url = std::env::var("MEGA_REDIS__URL")
            .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
        ProtocolApiState {
            storage,
            git_object_cache: Arc::new(GitObjectCache {
                connection: crate::jupiter::redis::init_connection(&crate::config::RedisConfig {
                    url: redis_url,
                })
                .await
                .expect("redis connection"),
                prefix: "disabled".to_string(),
            }),
            entity_store: Arc::new(SharedEntityStore::new()),
        }
    }

    async fn writer_counts(state: &ProtocolApiState) -> WriterCounts {
        let git_db = state.storage.git_db_storage();
        let db = git_db.get_connection();
        WriterCounts {
            git_repo: git_repo::Entity::find().count(db).await.unwrap(),
            gitkeep: mega_blob::Entity::find()
                .filter(mega_blob::Column::Name.eq(".gitkeep"))
                .count(db)
                .await
                .unwrap(),
            push_queue: push_queue::Entity::find().count(db).await.unwrap(),
            mega_cl: mega_cl::Entity::find().count(db).await.unwrap(),
            trace_stream: trace_stream::Entity::find().count(db).await.unwrap(),
            trace_target: trace_target::Entity::find().count(db).await.unwrap(),
        }
    }

    fn assert_unchanged(before: &WriterCounts, after: &WriterCounts) {
        assert_eq!(before.git_repo, after.git_repo);
        assert_eq!(before.gitkeep, after.gitkeep);
        assert_eq!(before.push_queue, after.push_queue);
        assert_eq!(before.mega_cl, after.mega_cl);
        assert_eq!(before.trace_stream, after.trace_stream);
        assert_eq!(before.trace_target, after.trace_target);
        assert_eq!(before.git_repo, 0);
        assert_eq!(before.gitkeep, 0);
        assert_eq!(before.push_queue, 0);
        assert_eq!(before.mega_cl, 0);
        assert_eq!(before.trace_stream, 0);
        assert_eq!(before.trace_target, 0);
    }

    #[tokio::test]
    async fn trace_locator_dispatch_discovery_does_not_write_code_rows() {
        let temp = tempfile::tempdir().unwrap();
        let state = protocol_state(&temp).await;
        let path = format!("/.traces/by-scope/{STREAM_ID}");
        let session = SmartSession::from_state(
            std::path::PathBuf::from(&path),
            ServiceType::ReceivePack,
            TransportProtocol::Http,
            &state,
        )
        .unwrap();
        let before = writer_counts(&state).await;
        let handler = session
            .repo_handler_with_commands(&state, Vec::new())
            .await
            .unwrap();
        assert!(handler.is_trace_repo());
        assert!(!handler.is_monorepo());
        assert!(!handler.bind_tip_after_receive());
        let (head, refs) = handler.refs_with_head_hash().await.unwrap();
        assert!(is_protocol_zero_id(&head));
        assert!(refs.is_empty());
        assert!(
            refs.iter()
                .all(|git_ref| git_ref.ref_name != "refs/heads/main")
        );
        let save_err = handler.save_entry(Vec::new()).await.unwrap_err();
        assert!(save_err.to_string().contains(CODE_WRITE_REFUSED));
        let unpack_err = handler
            .unpack_stream(
                &state.storage.config().pack,
                Box::pin(futures::stream::empty()),
            )
            .await
            .expect_err("TraceRepo must refuse a pack before decoding it");
        assert!(matches!(
            unpack_err,
            crate::common::errors::ProtocolError::PackRejected(_)
        ));
        let finalize_err = handler.finalize_receive_pack().await.unwrap_err();
        assert!(finalize_err.to_string().contains(CODE_WRITE_REFUSED));
        let update_err = handler
            .update_refs(&RefCommand::new(
                "0".repeat(40),
                "1".repeat(40),
                "refs/heads/main".to_owned(),
            ))
            .await
            .unwrap_err();
        assert!(update_err.to_string().contains(CODE_WRITE_REFUSED));
        let tree_err = handler
            .traverses_tree_and_update_filepath()
            .await
            .unwrap_err();
        assert!(tree_err.to_string().contains(CODE_WRITE_REFUSED));
        assert!(!handler.check_default_branch().await);
        assert_unchanged(&before, &writer_counts(&state).await);

        let sha256 = TraceRepo::new(
            TracesLocator::ByPath {
                target_path: "/project/foo".to_owned(),
                source_id: "legacy/repo id".to_owned(),
                epoch: "22222222-2222-4222-8222-222222222222".to_owned(),
                hash_kind: TracesObjectFormat::Sha256,
            },
            HashKind::Sha1,
        );
        let (head, refs) = sha256.refs_with_head_hash().await.unwrap();
        assert_eq!(head.len(), 64);
        assert!(refs.is_empty());
        assert_eq!(sha256.object_hash_kind().unwrap(), HashKind::Sha256);
        assert_unchanged(&before, &writer_counts(&state).await);
    }
}
