use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request},
    response::Response,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use futures::StreamExt;
use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::object::{
        commit::Commit,
        tree::{Tree, TreeItem, TreeItemMode},
    },
};
use mst2_codec::{
    chunkmap::{CHUNK_SIZE, ChunkLeaf, verify_leaf},
    treeframe::{Frame, parse_stream},
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, QueryFilter, Set,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::{
    api::{
        MonoApiServiceState, oauth::api_store::BrowserSessionStore,
        router::snapshot_router::routers,
    },
    callisto::{mega_refs, mst2_verified_object, sea_orm_active_enums::PushQueueKindEnum},
    ceres::{
        api_service::{ApiHandler, cache::GitObjectCache, mono_api_service::MonoApiService},
        snapshot::{
            error::SnapshotErrorCode,
            pages::{MetadataWalkOutcome, hex_of, resolve_abs_metadata},
            resolver::FsKind,
        },
    },
    common::utils::MEGA_BRANCH_NAME,
    config::{PushPolicy, testing::isolated_config},
    jupiter::{
        service::{
            git_service::GitService,
            push_queue_service::{
                EnqueueRequest, ExecuteOutcome, ExecuteRequest, PushExecContext, PushPayload,
                push_operation_id,
            },
        },
        storage::{
            base_storage::StorageConnector,
            mono_storage::MST2_VERIFICATION_VERSION,
            object_storage::{MegaObjectStorageWrapper, build_object_storage},
            push_queue_storage::{ClaimOutcome, EnqueueOutcome},
        },
        tests::{
            TestSchemaGuard, test_db_config, test_redis_manager, test_storage_with_config,
            with_test_vault,
        },
    },
    orbit_api::{
        error::OrbitResult,
        log_storage::{LogManifest, LogStorage},
        object_storage::{
            MegaObjectStorage, ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace,
        },
    },
};

const TOKEN: &str = "mst2-fixed-content-test";

#[path = "snapshot_objects_bounded_tests.rs"]
mod bounded_objects;

#[path = "snapshot_chunks_bounded_tests.rs"]
mod bounded_chunks;

#[path = "snapshot_persisted_chunk_map_tests.rs"]
mod persisted_chunk_maps;

#[path = "snapshot_chunk_map_retention_tests.rs"]
mod chunk_map_retention;

#[path = "snapshot_raw_blob_tests.rs"]
mod raw_blob;

#[path = "snapshot_session_tests.rs"]
mod durable_sessions;

#[path = "snapshot_generation_upgrade_tests.rs"]
mod generation_upgrade;

#[path = "snapshot_generation_history_fixture.rs"]
mod generation_history_fixture;

#[path = "snapshot_generation_qualified_fixture.rs"]
mod generation_qualified_fixture;

#[path = "snapshot_install_capability_fixture.rs"]
mod install_capability_fixture;

type ReceiptWriteHold = (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);
type ReadOpenHold = (
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    Arc<AtomicUsize>,
);

struct ReadOpenDrop(Arc<AtomicUsize>);

impl Drop for ReadOpenDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn await_read_open_hold(hold: Option<ReadOpenHold>) {
    if let Some((entered, release, drops)) = hold {
        let _owner = ReadOpenDrop(drops);
        entered.notify_one();
        release.notified().await;
    }
}

struct NoRootedReuse;

#[async_trait::async_trait]
impl crate::ceres::snapshot::rooted_metadata_projection::RootedReuseLookup for NoRootedReuse {
    async fn lookup_reuse(
        &self,
        _tree_oid: &str,
        _identity: &crate::ceres::snapshot::metadata_install::MetadataInstallIdentity,
    ) -> Result<
        Option<crate::ceres::snapshot::rooted_metadata_projection::CertifiedReusableDirectory>,
        crate::ceres::snapshot::error::SnapshotError,
    > {
        Ok(None)
    }
}
#[path = "snapshot_rooted_metadata_tests.rs"]
mod rooted_metadata;

#[derive(Default)]
struct ReadCounts {
    whole: AtomicUsize,
    range: AtomicUsize,
    bytes: AtomicUsize,
    object_fault: std::sync::Mutex<Option<bounded_objects::StreamFault>>,
    object_size_override: std::sync::Mutex<Option<i64>>,
    chunk_faults: std::sync::Mutex<Vec<bounded_chunks::ChunkFault>>,
    receipt_reads: AtomicUsize,
    receipt_writes: AtomicUsize,
    receipt_write_fail_after_create: AtomicBool,
    receipt_read_failure: AtomicBool,
    receipt_read_corruption: std::sync::Mutex<Option<Bytes>>,
    receipt_read_meta_size: std::sync::atomic::AtomicI64,
    receipt_read_late_error: AtomicBool,
    receipt_write_holds: std::sync::Mutex<Option<ReceiptWriteHold>>,
    receipt_write_wait_drops: Arc<AtomicUsize>,
    receipt_late_create_holds: std::sync::Mutex<Option<ReceiptWriteHold>>,
    receipt_inventory_holds: std::sync::Mutex<Option<ReceiptWriteHold>>,
    receipt_inventory_calls: AtomicUsize,
    receipt_retention_unsupported: AtomicBool,
    receipt_deletes: AtomicUsize,
    whole_open_holds: std::sync::Mutex<Option<ReadOpenHold>>,
    range_open_holds: std::sync::Mutex<Option<ReadOpenHold>>,
    receipt_open_holds: std::sync::Mutex<Option<ReadOpenHold>>,
}

impl ReadCounts {
    fn reset(&self) {
        self.whole.store(0, Ordering::SeqCst);
        self.range.store(0, Ordering::SeqCst);
        self.bytes.store(0, Ordering::SeqCst);
        self.receipt_reads.store(0, Ordering::SeqCst);
        self.receipt_writes.store(0, Ordering::SeqCst);
    }

    fn assert(&self, whole: usize, bytes: usize) {
        assert_eq!(self.whole.load(Ordering::SeqCst), whole);
        assert_eq!(self.range.load(Ordering::SeqCst), 0);
        assert_eq!(self.bytes.load(Ordering::SeqCst), bytes);
    }
}

struct CountingStorage {
    inner: MegaObjectStorageWrapper,
    counts: Arc<ReadCounts>,
}

#[async_trait::async_trait]
impl MegaObjectStorage for CountingStorage {
    fn supports_chunk_map_retention(&self) -> bool {
        !self
            .counts
            .receipt_retention_unsupported
            .load(Ordering::SeqCst)
            && self.inner.inner.supports_chunk_map_retention()
    }

    async fn chunk_map_receipt_inventory(
        &self,
    ) -> OrbitResult<crate::orbit_api::object_storage::ChunkMapReceiptInventory> {
        self.counts
            .receipt_inventory_calls
            .fetch_add(1, Ordering::SeqCst);
        if self
            .counts
            .receipt_retention_unsupported
            .load(Ordering::SeqCst)
        {
            return Err(crate::orbit_api::error::IoOrbitError::ChunkMapRetentionUnsupported);
        }
        let inventory = self.inner.inner.chunk_map_receipt_inventory().await?;
        let holds = self.counts.receipt_inventory_holds.lock().unwrap().clone();
        if let Some((entered, release)) = holds {
            entered.notify_one();
            release.notified().await;
        }
        Ok(inventory)
    }

    async fn delete_chunk_map_receipt(
        &self,
        authority: &crate::orbit_api::object_storage::ChunkMapReceiptDeletion,
    ) -> OrbitResult<bool> {
        let deleted = self.inner.inner.delete_chunk_map_receipt(authority).await?;
        if deleted {
            self.counts.receipt_deletes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(deleted)
    }

    async fn put_metadata_atomic_create(
        &self,
        key: &ObjectKey,
        bytes: Bytes,
        meta: ObjectMeta,
    ) -> OrbitResult<()> {
        let late = if key.namespace == ObjectNamespace::ChunkMapReceipt {
            self.counts
                .receipt_late_create_holds
                .lock()
                .unwrap()
                .clone()
        } else {
            None
        };
        if let Some((entered, release)) = late {
            let inner = self.inner.clone();
            let key = key.clone();
            let counts = self.counts.clone();
            return tokio::spawn(async move {
                entered.notify_one();
                release.notified().await;
                inner
                    .inner
                    .put_metadata_atomic_create(&key, bytes, meta)
                    .await?;
                counts.receipt_writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        }
        self.inner
            .inner
            .put_metadata_atomic_create(key, bytes, meta)
            .await?;
        if key.namespace == ObjectNamespace::ChunkMapReceipt {
            self.counts.receipt_writes.fetch_add(1, Ordering::SeqCst);
            let holds = self.counts.receipt_write_holds.lock().unwrap().clone();
            if let Some((entered, release)) = holds {
                let _hold = ReadOpenDrop(self.counts.receipt_write_wait_drops.clone());
                entered.notify_one();
                release.notified().await;
            }
            if self
                .counts
                .receipt_write_fail_after_create
                .swap(false, Ordering::SeqCst)
            {
                return Err(crate::orbit_api::error::IoOrbitError::Other(
                    "injected post-create receipt failure".into(),
                ));
            }
        }
        Ok(())
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        meta: ObjectMeta,
    ) -> OrbitResult<()> {
        self.inner.inner.put_stream(key, data, meta).await
    }

    async fn get_stream(&self, key: &ObjectKey) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
        if key.namespace == ObjectNamespace::ChunkMapReceipt {
            self.counts.receipt_reads.fetch_add(1, Ordering::SeqCst);
            let holds = self.counts.receipt_open_holds.lock().unwrap().clone();
            await_read_open_hold(holds).await;
            if self.counts.receipt_read_failure.load(Ordering::SeqCst) {
                return Err(
                    crate::orbit_api::error::IoOrbitError::object_store_not_found(
                        key.default_sharding(),
                    ),
                );
            }
            let bad = self.counts.receipt_read_corruption.lock().unwrap().clone();
            if let Some(bytes) = bad {
                let declared = self.counts.receipt_read_meta_size.load(Ordering::SeqCst);
                let size = if declared > 0 {
                    declared
                } else {
                    bytes.len() as i64
                };
                let mut parts = vec![Ok(bytes)];
                if self.counts.receipt_read_late_error.load(Ordering::SeqCst) {
                    parts.push(Err(std::io::Error::other(
                        "injected late receipt read error",
                    )));
                }
                return Ok((
                    Box::pin(futures::stream::iter(parts)),
                    ObjectMeta {
                        size,
                        ..Default::default()
                    },
                ));
            }
            return self.inner.inner.get_stream(key).await;
        }
        self.counts.whole.fetch_add(1, Ordering::SeqCst);
        let holds = self.counts.whole_open_holds.lock().unwrap().clone();
        await_read_open_hold(holds).await;
        let chunk_fault = self
            .counts
            .chunk_faults
            .lock()
            .unwrap()
            .iter()
            .find(|fault| fault.oid == key.key)
            .cloned();
        if let Some(fault) = chunk_fault {
            let counts = self.counts.clone();
            let size = fault.size;
            let stream = fault.full_stream().map(move |part| {
                if let Ok(bytes) = &part {
                    counts.bytes.fetch_add(bytes.len(), Ordering::SeqCst);
                }
                part
            });
            return Ok((
                Box::pin(stream),
                ObjectMeta {
                    size: size as i64,
                    ..Default::default()
                },
            ));
        }
        let (stream, mut meta) = self.inner.inner.get_stream(key).await?;
        if let Some(size) = *self.counts.object_size_override.lock().unwrap() {
            meta.size = size;
        }
        let fault = self.counts.object_fault.lock().unwrap().clone();
        let stream = match fault {
            Some(fault) if fault.oid == key.key => fault.stream(),
            _ => stream,
        };
        let counts = self.counts.clone();
        let stream = stream.map(move |part| {
            if let Ok(bytes) = &part {
                counts.bytes.fetch_add(bytes.len(), Ordering::SeqCst);
            }
            part
        });
        Ok((Box::pin(stream), meta))
    }

    async fn get_range_stream(
        &self,
        key: &ObjectKey,
        start: u64,
        end: Option<u64>,
    ) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
        self.counts.range.fetch_add(1, Ordering::SeqCst);
        self.inner.inner.get_range_stream(key, start, end).await
    }

    async fn get_range_stream_exact(
        &self,
        key: &ObjectKey,
        start: u64,
        end: u64,
    ) -> OrbitResult<Option<(ObjectByteStream, ObjectMeta)>> {
        self.counts.range.fetch_add(1, Ordering::SeqCst);
        let holds = self.counts.range_open_holds.lock().unwrap().clone();
        await_read_open_hold(holds).await;
        let fault = self
            .counts
            .chunk_faults
            .lock()
            .unwrap()
            .iter()
            .find(|fault| fault.oid == key.key)
            .cloned();
        if let Some(fault) = fault {
            let range = fault.range_stream(start, end)?;
            return Ok(range.map(|(stream, meta)| {
                let counts = self.counts.clone();
                let stream = stream.map(move |part| {
                    if let Ok(bytes) = &part {
                        counts.bytes.fetch_add(bytes.len(), Ordering::SeqCst);
                    }
                    part
                });
                (Box::pin(stream) as ObjectByteStream, meta)
            }));
        }
        let result = self
            .inner
            .inner
            .get_range_stream_exact(key, start, end)
            .await?;
        let fault = self.counts.object_fault.lock().unwrap().clone();
        Ok(result.map(|(stream, meta)| {
            let stream = match fault {
                Some(fault) if fault.oid == key.key => fault.stream(),
                _ => stream,
            };
            let counts = self.counts.clone();
            let stream = stream.map(move |part| {
                if let Ok(bytes) = &part {
                    counts.bytes.fetch_add(bytes.len(), Ordering::SeqCst);
                }
                part
            });
            (Box::pin(stream) as ObjectByteStream, meta)
        }))
    }

    async fn exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
        self.inner.inner.exists(key).await
    }

    async fn signed_url(
        &self,
        key: &ObjectKey,
        method: Method,
        expires_in: Duration,
    ) -> OrbitResult<Option<String>> {
        self.inner.inner.signed_url(key, method, expires_in).await
    }

    async fn delete(&self, key: &ObjectKey) -> OrbitResult<()> {
        self.inner.inner.delete(key).await
    }
}

#[async_trait::async_trait]
impl LogStorage for CountingStorage {
    async fn append(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        meta: ObjectMeta,
    ) -> OrbitResult<()> {
        self.inner.inner.append(key, data, meta).await
    }

    async fn read_range(
        &self,
        key: &ObjectKey,
        offset: u64,
        length: u64,
    ) -> OrbitResult<ObjectByteStream> {
        self.inner.inner.read_range(key, offset, length).await
    }

    async fn read_lines_range(
        &self,
        key: &ObjectKey,
        start_line: u64,
        end_line: u64,
    ) -> OrbitResult<ObjectByteStream> {
        self.inner
            .inner
            .read_lines_range(key, start_line, end_line)
            .await
    }

    async fn append_concurrently(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        meta: ObjectMeta,
    ) -> OrbitResult<()> {
        self.inner.inner.append_concurrently(key, data, meta).await
    }

    async fn load_manifest(&self, key: &ObjectKey) -> OrbitResult<LogManifest> {
        self.inner.inner.load_manifest(key).await
    }

    async fn log_exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
        self.inner.inner.log_exists(key).await
    }
}

struct Fixture {
    app: Router,
    state: MonoApiServiceState,
    generic_history: bool,
    snapshot: String,
    lease: String,
    oid: String,
    raw: Vec<u8>,
    digest: [u8; 32],
    counts: Arc<ReadCounts>,
    _temp: tempfile::TempDir,
    _schema: Option<TestSchemaGuard>,
}

struct InitialFact {
    path: &'static str,
    size: u64,
    digest: [u8; 32],
}

#[derive(Default)]
struct FixtureOptions {
    initial_facts: Vec<InitialFact>,
    initial_chunk_faults: Vec<(&'static str, bounded_chunks::ChunkFault)>,
    lease_seconds: Option<u64>,
}

fn tree(items: Vec<TreeItem>) -> Tree {
    crate::ceres::view::tree_source::build_tree(HashKind::Sha1, items).unwrap()
}

fn item(mode: TreeItemMode, oid: ObjectHash, name: &str) -> TreeItem {
    TreeItem::new(mode, oid, name.to_string())
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

async fn fixture_source_oid(handler: &MonoApiService, project: &Tree, path: &str) -> String {
    match resolve_abs_metadata(handler, project, path).await.unwrap() {
        MetadataWalkOutcome::FoundFile { oid, .. } => oid,
        other => panic!("initial fixture fact path did not resolve: {path}: {other:?}"),
    }
}

async fn publish_native_push(
    storage: &crate::jupiter::storage::Storage,
    path: &str,
    old: ObjectHash,
    new: &Commit,
) {
    let old_id = old.to_string();
    let new_id = new.id.to_string();
    let payload = PushPayload {
        commits: vec![new_id.clone()],
        fork_base: Some(old_id.clone()),
        n: 1,
    };
    let outcome = storage
        .push_queue_service
        .enqueue(EnqueueRequest {
            kind: PushQueueKindEnum::Push,
            operation_id: push_operation_id(&old_id, &new_id),
            path: path.to_string(),
            old_id,
            new_id,
            requester: None,
            payload: payload.to_json(),
            ref_name: Some(MEGA_BRANCH_NAME.to_string()),
            is_delete: false,
        })
        .await
        .unwrap();
    let EnqueueOutcome::Inserted { id } = outcome else {
        panic!("fresh fixture push must insert: {outcome:?}");
    };
    assert_eq!(
        storage
            .push_queue_service
            .storage()
            .claim_for_execution(id)
            .await
            .unwrap(),
        ClaimOutcome::Claimed
    );
    let context = PushExecContext {
        storage: storage.clone(),
        git_object_cache: Arc::new(GitObjectCache {
            connection: test_redis_manager().await,
            prefix: String::new(),
        }),
        pre_apply_enter_barrier: None,
        pre_apply_release_barrier: None,
    };
    let outcome = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id,
                ..Default::default()
            },
            None,
            None,
            Some(&context),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ExecuteOutcome::Done {
            root_cas_writes: 1,
            ..
        }
    ));
}

impl Fixture {
    async fn new() -> Self {
        Self::new_with_pg_config(false).await
    }

    async fn new_with_pg_config(rebuildable: bool) -> Self {
        Self::new_with_pg_config_and_directories(rebuildable, 0).await
    }

    async fn new_with_pg_config_and_directories(rebuildable: bool, directory_count: usize) -> Self {
        Self::new_with_pg_config_directories_and_objects(rebuildable, directory_count, &[]).await
    }

    async fn new_with_pg_config_directories_and_objects(
        rebuildable: bool,
        directory_count: usize,
        objects: &[(String, Vec<u8>)],
    ) -> Self {
        Self::new_in_metadata_family(rebuildable, directory_count, objects, false).await
    }

    async fn new_generic_history_with_pg_config(rebuildable: bool) -> Self {
        Self::new_generic_history_with_pg_config_and_directories(rebuildable, 0).await
    }

    async fn new_generic_history_with_pg_config_and_directories(
        rebuildable: bool,
        directory_count: usize,
    ) -> Self {
        Self::new_in_metadata_family(rebuildable, directory_count, &[], true).await
    }

    async fn new_in_metadata_family(
        rebuildable: bool,
        directory_count: usize,
        objects: &[(String, Vec<u8>)],
        generic_history: bool,
    ) -> Self {
        Self::new_in_publication_mode(rebuildable, directory_count, objects, generic_history, true)
            .await
    }

    async fn new_without_publication() -> Self {
        Self::new_in_publication_mode(true, 0, &[], false, false).await
    }

    async fn new_in_publication_mode(
        rebuildable: bool,
        directory_count: usize,
        objects: &[(String, Vec<u8>)],
        generic_history: bool,
        publication_enabled: bool,
    ) -> Self {
        Self::new_in_publication_mode_with_options(
            rebuildable,
            directory_count,
            objects,
            generic_history,
            publication_enabled,
            FixtureOptions::default(),
        )
        .await
    }

    async fn new_rooted_with_options(
        objects: &[(String, Vec<u8>)],
        options: FixtureOptions,
    ) -> Self {
        Self::new_in_publication_mode_with_options(false, 0, objects, false, true, options).await
    }

    async fn new_in_publication_mode_with_options(
        rebuildable: bool,
        directory_count: usize,
        objects: &[(String, Vec<u8>)],
        generic_history: bool,
        publication_enabled: bool,
        options: FixtureOptions,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let mut config = isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.mst2.enabled = true;
        config.mst2.publication_enabled = publication_enabled;
        config.mst2.instance_uuid = Some(uuid::Uuid::new_v4().to_string());
        config.mst2.auth_token = Some(TOKEN.to_string());
        let backend = build_object_storage(&config.object_storage).await.unwrap();
        let counts = Arc::new(ReadCounts::default());
        let (mut storage, schema) = if rebuildable || !generic_history {
            let (database, schema) = test_db_config(temp.path()).await;
            config.database = database;
            let assembly = async {
                let connection =
                    crate::jupiter::storage::init::database_connection(&config.database)
                        .await
                        .unwrap();
                crate::jupiter::storage::Storage::new_with_connection(
                    Arc::new(config),
                    Arc::new(connection),
                    backend.clone(),
                )
                .await
                .unwrap()
            };
            let storage = if generic_history {
                crate::jupiter::storage::init::with_generic_history_bootstrap(assembly).await
            } else {
                assembly.await
            };
            (storage, Some(schema))
        } else {
            (test_storage_with_config(temp.path(), config).await, None)
        };
        if generic_history {
            let q_rows:i64=storage.mono_storage().get_connection().query_one_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Postgres,"SELECT count(*) FROM mst2_metadata_namespace WHERE graph_domain='qualified-v1'"))
                .await.unwrap().unwrap().try_get_by_index(0).unwrap();
            assert_eq!(
                q_rows, 0,
                "G history fixtures must not create or erase actual Q ownership"
            );
        }
        storage.git_service = GitService {
            obj_storage: MegaObjectStorageWrapper::new(Arc::new(CountingStorage {
                inner: backend,
                counts: counts.clone(),
            })),
        };
        let storage = with_test_vault(storage, temp.path()).await;
        // Unique bytes prevent another test's process-wide digest cache from
        // turning a required cold read into a hit. These bytes also look like
        // a Git object header, and contain NUL and non-UTF-8 content.
        let prefix = format!("blob 3\0abc\0{}", uuid::Uuid::new_v4());
        let mut raw = vec![0xff; CHUNK_SIZE as usize + 113];
        raw[..prefix.len()].copy_from_slice(prefix.as_bytes());
        let oid = storage
            .git_service
            .save_object_from_raw(Bytes::copy_from_slice(&raw))
            .await
            .unwrap();
        let blob_oid = ObjectHash::from_hex_for_kind(HashKind::Sha1, &oid).unwrap();
        let link_oid = storage
            .git_service
            .save_object_from_raw(Bytes::from_static(b"file"))
            .await
            .unwrap();
        let link_oid = ObjectHash::from_hex_for_kind(HashKind::Sha1, &link_oid).unwrap();
        let empty_oid = storage
            .git_service
            .save_object_from_raw(Bytes::new())
            .await
            .unwrap();
        let empty_oid = ObjectHash::from_hex_for_kind(HashKind::Sha1, &empty_oid).unwrap();
        let empty_dir = tree(vec![]);
        let nested = tree(vec![item(TreeItemMode::Blob, blob_oid, "file")]);
        let mut project_items = vec![
            item(TreeItemMode::Blob, blob_oid, "alias"),
            item(TreeItemMode::Tree, empty_dir.id, "directory"),
            item(TreeItemMode::Blob, empty_oid, "empty"),
            item(TreeItemMode::BlobExecutable, blob_oid, "executable"),
            item(TreeItemMode::Blob, blob_oid, "file"),
            item(TreeItemMode::Link, link_oid, "link"),
            item(TreeItemMode::Tree, nested.id, "nested"),
        ];
        let mut extra_trees = Vec::new();
        for index in 0..directory_count {
            // Distinct names make distinct canonical pages even though all
            // directories share the same already-verified immutable blob.
            let child = tree(vec![item(
                TreeItemMode::Blob,
                blob_oid,
                &format!("file-{index:03}"),
            )]);
            project_items.push(item(
                TreeItemMode::Tree,
                child.id,
                &format!("wide-{index:03}"),
            ));
            extra_trees.push(child);
        }
        for (name, raw) in objects {
            let oid = storage
                .git_service
                .save_object_from_raw(Bytes::copy_from_slice(raw))
                .await
                .unwrap();
            let mut oid = ObjectHash::from_hex_for_kind(HashKind::Sha1, &oid).unwrap();
            let mut mode = TreeItemMode::Blob;
            let components: Vec<_> = name.split('/').collect();
            for index in (1..components.len()).rev() {
                let child = tree(vec![item(mode, oid, components[index])]);
                oid = child.id;
                mode = TreeItemMode::Tree;
                extra_trees.push(child);
            }
            project_items.push(item(mode, oid, components[0]));
        }
        let project = tree(project_items);
        let old_tip = Commit::from_tree_id_with_kind(
            HashKind::Sha1,
            project.id,
            vec![],
            "fixed content initial path tip",
        )
        .unwrap();
        let new_tip = Commit::from_tree_id_with_kind(
            HashKind::Sha1,
            project.id,
            vec![old_tip.id],
            "fixed content published path tip",
        )
        .unwrap();
        let root = tree(vec![
            item(TreeItemMode::Blob, blob_oid, "outside"),
            item(TreeItemMode::Tree, project.id, "project"),
        ]);
        let commit = Commit::from_tree_id_with_kind(
            HashKind::Sha1,
            root.id,
            vec![],
            "fixed content HTTP test",
        )
        .unwrap();
        let mono = storage.mono_storage();
        let mut trees = vec![empty_dir, nested, project.clone(), root.clone()];
        trees.extend(extra_trees);
        mono.save_mega_trees(trees, commit.id, None).await.unwrap();
        mono.save_mega_commits(vec![commit.clone(), old_tip.clone(), new_tip.clone()], None)
            .await
            .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_string(),
                commit.id.to_string(),
                root.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                "/project",
                MEGA_BRANCH_NAME.to_string(),
                old_tip.id.to_string(),
                old_tip.tree_id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let state = MonoApiServiceState {
            entity_store: storage.entity_store.clone(),
            storage: storage.clone(),
            session_store: BrowserSessionStore::Anonymous,
            git_object_cache: Arc::new(GitObjectCache {
                connection: test_redis_manager().await,
                prefix: String::new(),
            }),
            listen_addr: "127.0.0.1:0".to_string(),
        };
        let handler = MonoApiService::from(&state);
        for fact in options.initial_facts {
            let oid = fixture_source_oid(&handler, &project, fact.path).await;
            let db = mono.get_connection();
            let current = mst2_verified_object::Entity::find()
                .filter(mst2_verified_object::Column::StorageDomain.eq("git"))
                .filter(mst2_verified_object::Column::ObjectKind.eq("blob"))
                .filter(mst2_verified_object::Column::GitOid.eq(&oid))
                .one(db)
                .await
                .unwrap();
            if let Some(current) = current {
                let mut current = current.into_active_model();
                current.size = Set(i64::try_from(fact.size).unwrap());
                current.raw_sha256 = Set(fact.digest.to_vec());
                current.update(db).await.unwrap();
            } else {
                mst2_verified_object::Entity::insert(mst2_verified_object::ActiveModel {
                    id: sea_orm::ActiveValue::NotSet,
                    storage_domain: Set("git".into()),
                    git_oid: Set(oid),
                    object_kind: Set("blob".into()),
                    raw_sha256: Set(fact.digest.to_vec()),
                    size: Set(i64::try_from(fact.size).unwrap()),
                    verification_version: Set(MST2_VERIFICATION_VERSION),
                    state: Set("VERIFIED".into()),
                    created_at: Set(chrono::Utc::now().fixed_offset()),
                })
                .exec(db)
                .await
                .unwrap();
            }
        }
        if publication_enabled {
            mono.initialize_native_publication(
                storage.config().mst2.instance_uuid.as_deref().unwrap(),
            )
            .await
            .unwrap();
            publish_native_push(&storage, "/project", old_tip.id, &new_tip).await;
            let head = mono
                .read_native_publication_head(
                    storage.config().mst2.instance_uuid.as_deref().unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(head.token.sequence, 1);
            assert!(head.token.certificate.is_some());
            assert_eq!(head.root.commit, commit.id.to_string());
            assert_eq!(head.root.tree, root.id.to_string());
        }
        for (path, mut fault) in options.initial_chunk_faults {
            fault.oid = fixture_source_oid(&handler, &project, path).await;
            counts.chunk_faults.lock().unwrap().push(fault);
        }
        let routes = if generic_history {
            crate::api::router::snapshot_router::generic_history_routers(state.clone())
        } else {
            routers(state.clone())
        };
        let app = Router::new().nest("/api/v2", routes.with_state(state.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/snapshots/resolve")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"target":{"kind":"latest"},"scope":"/project","lease_seconds":options.lease_seconds.unwrap_or(600)}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let resolved = success_json(response).await;
        assert!(counts.whole.load(Ordering::SeqCst) > 0);
        assert!(
            !mono
                .get_verified_blobs(vec![oid.clone()])
                .await
                .unwrap()
                .is_empty()
        );
        counts.reset();
        Self {
            app,
            state,
            snapshot: resolved["descriptor"]["snapshot_id"]
                .as_str()
                .unwrap()
                .to_string(),
            lease: resolved["lease_id"].as_str().unwrap().to_string(),
            oid,
            digest: Sha256::digest(&raw).into(),
            raw,
            counts,
            generic_history,
            _temp: temp,
            _schema: schema,
        }
    }

    fn request(&self, method: &str, suffix: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(format!("/api/v2/snapshots/{}/{suffix}", self.snapshot))
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("x-mega-snapshot-lease", &self.lease)
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    async fn send(&self, method: &str, suffix: &str, body: Body) -> Response {
        self.app
            .clone()
            .oneshot(self.request(method, suffix, body))
            .await
            .unwrap()
    }

    fn digest_string(&self) -> String {
        format!("sha256:{}", hex_of(&self.digest))
    }

    async fn map(&self, path: &str) -> Value {
        success_json(
            self.send("GET", &format!("chunk-map?path={path}"), Body::empty())
                .await,
        )
        .await
    }

    async fn fact(&self) -> mst2_verified_object::Model {
        self.state
            .storage
            .mono_storage()
            .get_verified_blobs(vec![self.oid.clone()])
            .await
            .unwrap()
            .remove(&self.oid)
            .unwrap()
    }

    async fn delete_fact(&self) {
        mst2_verified_object::Entity::delete_many()
            .filter(mst2_verified_object::Column::GitOid.eq(&self.oid))
            .exec(self.state.storage.mono_storage().get_connection())
            .await
            .unwrap();
    }

    async fn replace_fact(&self, fact: mst2_verified_object::Model) {
        self.delete_fact().await;
        mst2_verified_object::Entity::insert(fact.into_active_model())
            .exec(self.state.storage.mono_storage().get_connection())
            .await
            .unwrap();
    }

    async fn write_raw(&self, bytes: Vec<u8>) {
        // Git SinglePut is create-if-absent. Remove only this isolated
        // fixture's object so corruption and repair actually change its bytes.
        let objects = &self.state.storage.git_service.obj_storage.inner;
        let key = ObjectKey {
            namespace: ObjectNamespace::Git,
            key: self.oid.clone(),
        };
        if objects.exists(&key).await.unwrap() {
            objects.delete(&key).await.unwrap();
        }
        self.state
            .storage
            .git_service
            .save_object_from_model(bytes.clone(), &self.oid)
            .await
            .unwrap();
        assert_eq!(
            self.state
                .storage
                .git_service
                .get_object_as_bytes(&self.oid)
                .await
                .unwrap(),
            bytes
        );
        // Fault setup is outside the HTTP read measurement window.
        self.counts.reset();
    }

    fn chunk_body(&self, path: &str, map_id: &str, index: &str) -> Value {
        json!({"items":[{
            "path":path,"expected_digest":self.digest_string(),
            "map_id":map_id,"chunk_index":index
        }],"encoding":"identity"})
    }
}

async fn success_json(response: Response) -> Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(status.as_u16(), 200, "{}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}

async fn error(response: Response, status: u16, code: &str, retryable: bool) {
    let actual_status = response.status().as_u16();
    assert_eq!(response.headers()["content-type"], "application/json");
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(!request_id.is_empty());
    let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        actual_status, status,
        "error code: {}, message: {}",
        value["error"]["code"], value["error"]["message"]
    );
    assert_eq!(value["error"]["code"], code);
    assert_eq!(value["error"]["retryable"], retryable);
    assert_eq!(value["error"]["request_id"], request_id);
}

#[tokio::test]
async fn mst2_rooted_streamed_fact_persists_one_source_pass_and_reuses_warm_alias_facts() {
    use crate::ceres::snapshot::rooted_metadata_projection::prepare_rooted_native_metadata;

    let fixture = Fixture::new().await;
    let handler = MonoApiService::from(&fixture.state);
    let mut raw = vec![0xff; crate::orbit_api::object_storage::OBJECT_STREAM_ITEM_BYTES + 113];
    let prefix = format!("blob 3\0abc\0{}", uuid::Uuid::new_v4());
    raw[..prefix.len()].copy_from_slice(prefix.as_bytes());
    let oid = fixture
        .state
        .storage
        .git_service
        .save_object_from_raw(Bytes::copy_from_slice(&raw))
        .await
        .unwrap();
    let source = ObjectHash::from_hex_for_kind(HashKind::Sha1, &oid).unwrap();
    let root = tree(vec![
        item(TreeItemMode::Blob, source, "alias"),
        item(TreeItemMode::Blob, source, "file"),
    ]);
    let mono = fixture.state.storage.mono_storage();
    assert!(
        mono.get_verified_blobs(vec![oid.clone()])
            .await
            .unwrap()
            .is_empty()
    );
    fixture.counts.reset();
    let first = prepare_rooted_native_metadata(&handler, &root, "/", &NoRootedReuse)
        .await
        .unwrap();
    fixture.counts.assert(1, raw.len());
    assert_eq!(first.work.blob_fetches, 1);
    assert_eq!(first.work.raw_bytes_fetched, raw.len() as u64);
    assert_eq!(first.work.raw_bytes_hashed, raw.len() as u64);
    assert_eq!(first.work.verified_blob_persistence_batches, 1);
    let stored = mono.get_verified_blobs(vec![oid.clone()]).await.unwrap();
    assert_eq!(stored[&oid].size, raw.len() as i64);
    assert_eq!(stored[&oid].raw_sha256, Sha256::digest(&raw).to_vec());
    fixture.counts.reset();
    let warm = prepare_rooted_native_metadata(&handler, &root, "/", &NoRootedReuse)
        .await
        .unwrap();
    fixture.counts.assert(0, 0);
    assert_eq!(warm.plan.identity, first.plan.identity);
    assert_eq!(warm.plan.root, first.plan.root);
    assert_eq!(warm.work.blob_fetches, 0);
    assert_eq!(warm.work.raw_bytes_fetched, 0);
    assert_eq!(warm.work.raw_bytes_hashed, 0);
    assert_eq!(warm.work.verified_blob_persistence_batches, 0);
    raw.push(b'2');
    let next_oid = fixture
        .state
        .storage
        .git_service
        .save_object_from_raw(Bytes::copy_from_slice(&raw))
        .await
        .unwrap();
    let next_source = ObjectHash::from_hex_for_kind(HashKind::Sha1, &next_oid).unwrap();
    let next_root = tree(vec![
        item(TreeItemMode::Blob, source, "alias"),
        item(TreeItemMode::Blob, next_source, "file"),
    ]);
    fixture.counts.reset();
    let next = prepare_rooted_native_metadata(&handler, &next_root, "/", &NoRootedReuse)
        .await
        .unwrap();
    fixture.counts.assert(1, raw.len());
    assert_ne!(
        next.plan.identity.tagged_root_tree_oid,
        first.plan.identity.tagged_root_tree_oid
    );
    assert_ne!(next.plan.root, first.plan.root);
    assert_eq!(next.work.blob_fetches, 1);
    assert_eq!(next.work.raw_bytes_fetched, raw.len() as u64);
    assert_eq!(next.work.raw_bytes_hashed, raw.len() as u64);
    assert_eq!(next.work.verified_blob_persistence_batches, 1);
    assert_eq!(next.work.verified_blob_facts_loaded, 2);
}

#[tokio::test]
async fn mst2_rooted_streamed_fact_never_persists_truncated_or_late_failed_sources() {
    use crate::ceres::snapshot::rooted_metadata_projection::prepare_rooted_native_metadata;

    let fixture = Fixture::new().await;
    let handler = MonoApiService::from(&fixture.state);
    for late_error in [false, true] {
        let raw = Bytes::from(format!("raw\0{}", uuid::Uuid::new_v4()));
        let oid = fixture
            .state
            .storage
            .git_service
            .save_object_from_raw(raw.clone())
            .await
            .unwrap();
        let source = ObjectHash::from_hex_for_kind(HashKind::Sha1, &oid).unwrap();
        let root = tree(vec![item(TreeItemMode::Blob, source, "file")]);
        *fixture.counts.object_fault.lock().unwrap() = Some(bounded_objects::StreamFault {
            oid: oid.clone(),
            kind: if late_error {
                bounded_objects::FaultKind::LateError(raw.clone())
            } else {
                bounded_objects::FaultKind::Parts(vec![raw.slice(..raw.len() - 1)])
            },
        });
        fixture.counts.reset();
        let error = prepare_rooted_native_metadata(&handler, &root, "/", &NoRootedReuse)
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            if late_error {
                SnapshotErrorCode::ObjectUnavailable
            } else {
                SnapshotErrorCode::IntegrityError
            }
        );
        fixture
            .counts
            .assert(1, raw.len() - usize::from(!late_error));
        assert!(
            fixture
                .state
                .storage
                .mono_storage()
                .get_verified_blobs(vec![oid])
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn mst2_capabilities_use_actual_backend_without_inventory_and_preserve_metadata_and_objects()
{
    let fixture = Fixture::new().await;
    let inventory_before = fixture
        .counts
        .receipt_inventory_calls
        .load(Ordering::SeqCst);
    let get_capabilities = || async {
        success_json(
            fixture
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/v2/snapshots/capabilities")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await
    };
    let supported = get_capabilities().await;
    for feature in ["raw_blob", "small_objects", "chunk_reads", "full_hydration"] {
        assert_eq!(supported["features"][feature], true);
    }
    fixture
        .counts
        .receipt_retention_unsupported
        .store(true, Ordering::SeqCst);
    let mut unsupported = supported.clone();
    for feature in ["raw_blob", "chunk_reads", "full_hydration"] {
        unsupported["features"][feature] = json!(false);
    }
    for _ in 0..3 {
        assert_eq!(get_capabilities().await, unsupported);
    }
    fixture.counts.assert(0, 0);
    assert_eq!(
        fixture
            .counts
            .receipt_inventory_calls
            .load(Ordering::SeqCst),
        inventory_before
    );
    assert_eq!(fixture.counts.receipt_reads.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    let head = fixture.send("HEAD", "blob?path=/file", Body::empty()).await;
    assert_eq!(head.status(), 200);
    assert_eq!(
        head.headers()["content-length"],
        fixture.raw.len().to_string()
    );
    assert_eq!(
        head.headers()["etag"],
        format!("\"{}\"", fixture.digest_string())
    );
    assert!(to_bytes(head.into_body(), 1024).await.unwrap().is_empty());
    success_json(fixture.send("GET", "directory?path=/", Body::empty()).await).await;
    fixture.counts.assert(0, 0);
    for path in ["blob?path=/file", "chunk-map?path=/file"] {
        error(
            fixture.send("GET", path, Body::empty()).await,
            503,
            "TEMPORARY_UNAVAILABLE",
            true,
        )
        .await;
    }
    fixture.counts.assert(0, 0);
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 0);
    let link_digest = digest(b"file");
    let request = json!({"items":[{"path":"/link", "expected_digest":format!("sha256:{}", hex_of(&link_digest))}], "encoding":"identity"}).to_string();
    let response = fixture
        .send("POST", "objects", Body::from(request.clone()))
        .await;
    assert_eq!(response.status(), 200);
    let wire = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let frames = parse_stream(&wire).unwrap();
    let [Frame::Object(object), Frame::End(end)] = frames.as_slice() else {
        panic!("expected OBJECT and terminal END");
    };
    assert_eq!(object.objects, vec![(link_digest, b"file".to_vec())]);
    assert_eq!(end.request_item_count, 1);
    assert_eq!(end.logical_bytes, 4);
    assert_eq!(
        end.request_body_sha256,
        <[u8; 32]>::from(Sha256::digest(request.as_bytes()))
    );
    fixture.counts.assert(1, 4);
    fixture
        .counts
        .receipt_retention_unsupported
        .store(false, Ordering::SeqCst);
    assert_eq!(get_capabilities().await, supported);
    fixture.counts.assert(1, 4);
}

#[tokio::test]
async fn mst2_fixed_head_uses_verified_facts_without_body_reads_and_preserves_raw_bytes() {
    let fixture = Fixture::new().await;
    for (path, kind, size, digest) in [
        (
            "/file",
            "regular",
            fixture.raw.len(),
            fixture.digest_string(),
        ),
        (
            "/alias",
            "regular",
            fixture.raw.len(),
            fixture.digest_string(),
        ),
        (
            "/executable",
            "executable",
            fixture.raw.len(),
            fixture.digest_string(),
        ),
        (
            "/empty",
            "regular",
            0,
            format!("sha256:{}", hex_of(&digest(&[]))),
        ),
        (
            "/link",
            "symlink",
            4,
            format!("sha256:{}", hex_of(&digest(b"file"))),
        ),
    ] {
        let response = fixture
            .send("HEAD", &format!("blob?path={path}"), Body::empty())
            .await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-length"], size.to_string());
        assert_eq!(response.headers()["x-mega-content-size"], size.to_string());
        assert_eq!(response.headers()["x-mega-fs-kind"], kind);
        assert_eq!(response.headers()["etag"], format!("\"{digest}\""));
        assert_eq!(response.headers()["vary"], "Authorization, Accept");
        assert_eq!(
            response.headers()["cache-control"],
            "private, no-cache, no-transform"
        );
        assert!(
            to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty()
        );
        fixture.counts.assert(0, 0);
    }
    let response = fixture.send("GET", "blob?path=/file", Body::empty()).await;
    assert_eq!(response.status(), 200);
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), fixture.raw);
    fixture.counts.assert(2, 2 * fixture.raw.len());
    assert_eq!(fixture.counts.receipt_writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mst2_publication_disabled_resolve_preserves_head_body_object_map_page_and_chunk_content() {
    let fixture = Fixture::new_without_publication().await;
    assert!(!fixture.state.storage.config().mst2.publication_enabled);
    assert_eq!(
        fixture
            .state
            .storage
            .snapshot_metadata_family(&fixture.lease, true)
            .await
            .unwrap(),
        None
    );
    let routes: i64 = fixture
        .state
        .storage
        .mono_storage()
        .get_connection()
        .query_one_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Postgres,
            "SELECT count(*) FROM mst2_snapshot_storage_route",
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap();
    assert_eq!(
        routes, 0,
        "runtime resolve does not create a permanent SID route"
    );
    let head = fixture.send("HEAD", "blob?path=/file", Body::empty()).await;
    assert_eq!(head.status(), 200);
    assert_eq!(
        head.headers()["content-length"],
        fixture.raw.len().to_string()
    );
    assert_eq!(
        head.headers()["etag"],
        format!("\"{}\"", fixture.digest_string())
    );
    assert!(to_bytes(head.into_body(), 1024).await.unwrap().is_empty());
    fixture.counts.assert(0, 0);
    let body = fixture.send("GET", "blob?path=/file", Body::empty()).await;
    assert_eq!(body.status(), 200);
    assert_eq!(
        to_bytes(body.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw
    );
    let link_digest = digest(b"file");
    let request=json!({"items":[{"path":"/link","expected_digest":format!("sha256:{}",hex_of(&link_digest))}],"encoding":"identity"}).to_string();
    let objects = fixture
        .send("POST", "objects", Body::from(request.clone()))
        .await;
    assert_eq!(objects.status(), 200);
    let wire = to_bytes(objects.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    let frames = parse_stream(&wire).unwrap();
    let [Frame::Object(object), Frame::End(end)] = frames.as_slice() else {
        panic!("expected OBJECT and terminal END");
    };
    assert_eq!(object.objects, vec![(link_digest, b"file".to_vec())]);
    assert_eq!(end.request_item_count, 1);
    assert_eq!(end.logical_bytes, 4);
    assert_eq!(
        end.request_body_sha256,
        <[u8; 32]>::from(Sha256::digest(request.as_bytes()))
    );
    let map = fixture.map("/file").await;
    let map_id = map["map"]["map_id"].as_str().unwrap();
    assert_eq!(map["map"]["file_content_id"], fixture.digest_string());
    let page = success_json(
        fixture
            .send(
                "GET",
                &format!("chunk-map/pages?path=/file&map_id={map_id}&page_index=0"),
                Body::empty(),
            )
            .await,
    )
    .await;
    let leaf = ChunkLeaf::decode(
        &STANDARD
            .decode(page["leaf_base64"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let hashes: Vec<[u8; 32]> = fixture
        .raw
        .chunks(CHUNK_SIZE as usize)
        .map(|bytes| Sha256::digest(bytes).into())
        .collect();
    assert_eq!(leaf.chunk_sha256, hashes);
    let chunks = fixture
        .send(
            "POST",
            "chunks",
            Body::from(fixture.chunk_body("/file", map_id, "0").to_string()),
        )
        .await;
    assert_eq!(chunks.status(), 200);
    let wire = to_bytes(chunks.into_body(), 2 * 1024 * 1024).await.unwrap();
    let frames = parse_stream(&wire).unwrap();
    let [Frame::Chunk(chunk), Frame::End(end)] = frames.as_slice() else {
        panic!("expected CHUNK and terminal END");
    };
    assert_eq!(chunk.chunk_bytes, &fixture.raw[..CHUNK_SIZE as usize]);
    assert_eq!(chunk.file_content_id, fixture.digest);
    assert_eq!(chunk.chunk_index, 0);
    assert_eq!(end.request_item_count, 1);
    assert_eq!(end.logical_bytes, u64::from(CHUNK_SIZE));
}

#[tokio::test]
async fn mst2_fixed_warm_map_and_leaf_aliases_skip_body_reads_and_chunks_use_current_ranges() {
    let fixture = Fixture::new().await;
    let initial = fixture.map("/file").await;
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(initial["map"]["file_content_id"], fixture.digest_string());
    assert_eq!(initial["map"]["file_size"], fixture.raw.len().to_string());
    assert_eq!(initial["map"]["chunk_count"], "2");
    let map_id = initial["map"]["map_id"].as_str().unwrap();
    fixture.counts.reset();
    for path in ["/file", "/alias", "/executable", "/nested/file"] {
        let cached = fixture.map(path).await;
        assert_eq!(cached["path"], path);
        assert_eq!(cached["map"], initial["map"]);
        let leaf_response = fixture
            .send(
                "GET",
                &format!("chunk-map/pages?path={path}&map_id={map_id}&page_index=0"),
                Body::empty(),
            )
            .await;
        let value = success_json(leaf_response).await;
        let leaf = ChunkLeaf::decode(
            &STANDARD
                .decode(value["leaf_base64"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(leaf.page_index, 0);
        let hashes: Vec<[u8; 32]> = fixture
            .raw
            .chunks(CHUNK_SIZE as usize)
            .map(|bytes| Sha256::digest(bytes).into())
            .collect();
        assert_eq!(leaf.chunk_sha256, hashes);
        assert_eq!(value["proof"], json!([]));
        assert_eq!(
            initial["map"]["pages_root"],
            format!("sha256:{}", hex_of(&leaf.leaf_hash().unwrap()))
        );
        verify_leaf(
            1,
            0,
            leaf.leaf_hash().unwrap(),
            &[],
            leaf.leaf_hash().unwrap(),
        )
        .unwrap();
        for index in 0..2 {
            let body = fixture
                .chunk_body(path, map_id, &index.to_string())
                .to_string();
            let response = fixture
                .send("POST", "chunks", Body::from(body.clone()))
                .await;
            assert_eq!(response.status(), 200);
            let wire = to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap();
            let frames = parse_stream(&wire).unwrap();
            let [Frame::Chunk(chunk), Frame::End(end)] = frames.as_slice() else {
                panic!("expected one CHUNK and authenticated END");
            };
            let begin = index as usize * CHUNK_SIZE as usize;
            let want = &fixture.raw[begin..fixture.raw.len().min(begin + CHUNK_SIZE as usize)];
            assert_eq!(chunk.chunk_index, index);
            assert_eq!(chunk.file_content_id, fixture.digest);
            assert_eq!(format!("sha256:{}", hex_of(&chunk.map_id)), map_id);
            assert_eq!(chunk.chunk_bytes, want);
            assert_eq!(end.request_item_count, 1);
            assert_eq!(end.unique_unit_count, 1);
            assert_eq!(end.logical_bytes, want.len() as u64);
            assert_eq!(
                end.request_body_sha256,
                <[u8; 32]>::from(Sha256::digest(body.as_bytes()))
            );
        }
        assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 2);
        assert_eq!(
            fixture.counts.bytes.load(Ordering::SeqCst),
            fixture.raw.len()
        );
        fixture.counts.reset();
    }
    fixture
        .state
        .storage
        .git_service
        .obj_storage
        .inner
        .delete(&ObjectKey {
            namespace: ObjectNamespace::Git,
            key: fixture.oid.clone(),
        })
        .await
        .unwrap();
    assert_eq!(fixture.map("/alias").await["map"], initial["map"]);
    fixture.counts.assert(0, 0);
    error(
        fixture
            .send(
                "POST",
                "chunks",
                Body::from(fixture.chunk_body("/alias", map_id, "0").to_string()),
            )
            .await,
        503,
        "OBJECT_UNAVAILABLE",
        false,
    )
    .await;
    assert_eq!(fixture.counts.whole.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.counts.range.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mst2_fixed_missing_and_legacy_facts_never_backfill_or_use_warm_cache() {
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    let original = fixture.fact().await;
    fixture.counts.reset();
    fixture.delete_fact().await;
    error(
        fixture
            .send("GET", "chunk-map?path=/file", Body::empty())
            .await,
        503,
        "METADATA_NOT_READY",
        true,
    )
    .await;
    let response = fixture.send("HEAD", "blob?path=/file", Body::empty()).await;
    assert_eq!(response.status(), 503);
    assert!(
        to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty()
    );
    fixture.counts.assert(0, 0);
    for (domain, kind, generation) in [
        ("git", "blob", 1),
        ("other", "blob", MST2_VERIFICATION_VERSION),
        ("git", "tree", MST2_VERIFICATION_VERSION),
    ] {
        let mut fact = original.clone();
        fact.storage_domain = domain.to_string();
        fact.object_kind = kind.to_string();
        fact.verification_version = generation;
        fixture.replace_fact(fact).await;
        error(
            fixture
                .send("GET", "chunk-map?path=/alias", Body::empty())
                .await,
            503,
            "METADATA_NOT_READY",
            true,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    fixture.replace_fact(original).await;
    fixture.map("/file").await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_fixed_invalid_facts_and_cached_size_conflict_fail_before_body_read() {
    let fixture = Fixture::new().await;
    fixture.map("/file").await;
    let original = fixture.fact().await;
    fixture.counts.reset();
    for case in 0..6 {
        let mut fact = original.clone();
        match case {
            0 => fact.state = "PENDING".to_string(),
            1 => fact.verification_version = MST2_VERIFICATION_VERSION + 1,
            2 => fact.size = -1,
            3 => fact.size = 8_796_093_022_209,
            4 => {
                fact.raw_sha256.pop().unwrap();
            }
            5 => {
                fact.size += 1;
            }
            _ => unreachable!(),
        };
        fixture.replace_fact(fact).await;
        error(
            fixture
                .send("GET", "chunk-map?path=/file", Body::empty())
                .await,
            502,
            "INTEGRITY_ERROR",
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    fixture.replace_fact(original).await;
    fixture.map("/file").await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_fixed_verified_fact_catalog_drift_fails_closed_without_body_read() {
    let fixture = Fixture::new().await;
    let mono = fixture.state.storage.mono_storage();
    mono.get_connection()
        .execute_unprepared(
            "ALTER TABLE mst2_verified_object RENAME TO mst2_verified_object_unavailable",
        )
        .await
        .unwrap();
    error(
        fixture
            .send("GET", "chunk-map?path=/file", Body::empty())
            .await,
        502,
        "INTEGRITY_ERROR",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    mono.get_connection()
        .execute_unprepared(
            "ALTER TABLE mst2_verified_object_unavailable RENAME TO mst2_verified_object",
        )
        .await
        .unwrap();
    fixture.map("/file").await;
    fixture.counts.assert(1, fixture.raw.len());
}

#[tokio::test]
async fn mst2_fixed_symlink_facts_outside_profile_fail_without_body_reads() {
    let fixture = Fixture::new().await;
    let mono = fixture.state.storage.mono_storage();
    let main = mono.get_main_ref("/").await.unwrap().unwrap();
    let handler = MonoApiService::from(&fixture.state);
    let root = handler.get_tree_by_hash(&main.ref_tree_hash).await.unwrap();
    let MetadataWalkOutcome::FoundFile { oid, .. } =
        resolve_abs_metadata(&handler, &root, "/project/link")
            .await
            .unwrap()
    else {
        panic!("fixture link must be a fixed file");
    };
    let original = mono
        .get_verified_blobs(vec![oid.clone()])
        .await
        .unwrap()
        .remove(&oid)
        .unwrap();
    for size in [0, 4096] {
        let mut fact = original.clone().into_active_model();
        fact.size = Set(size);
        fact.update(mono.get_connection()).await.unwrap();
        error(
            fixture
                .send("GET", "chunk-map?path=/link", Body::empty())
                .await,
            502,
            "INTEGRITY_ERROR",
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    let mut restored = original.into_active_model();
    restored.size = Set(4);
    restored.update(mono.get_connection()).await.unwrap();
    let response = fixture.send("HEAD", "blob?path=/link", Body::empty()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-mega-content-size"], "4");
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_fixed_digest_and_path_semantics_are_checked_before_projection_cache() {
    let fixture = Fixture::new().await;
    let wrong = format!("sha256:{}", "0".repeat(64));
    error(
        fixture
            .send(
                "GET",
                &format!("chunk-map?path=/file&expected_digest={wrong}"),
                Body::empty(),
            )
            .await,
        409,
        "EXPECTED_DIGEST_MISMATCH",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
    fixture.map("/file").await;
    fixture.counts.reset();
    for (path, status, code) in [
        ("/absent", 404, "PATH_NOT_FOUND"),
        ("/outside", 404, "PATH_NOT_FOUND"),
        ("/directory", 409, "NOT_DIRECTORY"),
        ("/file/child", 409, "NOT_DIRECTORY"),
        ("/link/child", 409, "SYMLINK_TRAVERSAL"),
        ("/../outside", 400, "SCOPE_INVALID"),
    ] {
        error(
            fixture
                .send(
                    "GET",
                    &format!(
                        "chunk-map?path={path}&expected_digest={}",
                        fixture.digest_string()
                    ),
                    Body::empty(),
                )
                .await,
            status,
            code,
            false,
        )
        .await;
        fixture.counts.assert(0, 0);
    }
    error(
        fixture
            .send(
                "GET",
                &format!("chunk-map?path=/alias&expected_digest={wrong}"),
                Body::empty(),
            )
            .await,
        409,
        "EXPECTED_DIGEST_MISMATCH",
        false,
    )
    .await;
    let mut request = fixture.request("HEAD", "blob?path=/file", Body::empty());
    request
        .headers_mut()
        .insert("range", "bytes=0-9".parse().unwrap());
    assert_eq!(
        fixture.app.clone().oneshot(request).await.unwrap().status(),
        400
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_fixed_cold_corrupt_or_missing_bodies_publish_no_projection_and_retry() {
    for mode in 0..3 {
        let fixture = Fixture::new().await;
        match mode {
            0 => {
                let mut corrupt = fixture.raw.clone();
                corrupt[0] ^= 1;
                fixture.write_raw(corrupt).await;
            }
            1 => {
                fixture
                    .write_raw(fixture.raw[..fixture.raw.len() - 1].to_vec())
                    .await
            }
            2 => fixture
                .state
                .storage
                .git_service
                .obj_storage
                .inner
                .delete(&ObjectKey {
                    namespace: ObjectNamespace::Git,
                    key: fixture.oid.clone(),
                })
                .await
                .unwrap(),
            _ => unreachable!(),
        }
        let (status, code, bytes) = match mode {
            0 => (502, "INTEGRITY_ERROR", fixture.raw.len()),
            1 => (502, "INTEGRITY_ERROR", fixture.raw.len() - 1),
            2 => (503, "OBJECT_UNAVAILABLE", 0),
            _ => unreachable!(),
        };
        error(
            fixture
                .send("GET", "chunk-map?path=/file", Body::empty())
                .await,
            status,
            code,
            false,
        )
        .await;
        fixture.counts.assert(1, bytes);
        fixture.write_raw(fixture.raw.clone()).await;
        fixture.counts.reset();
        fixture.map("/file").await;
        fixture.counts.assert(1, fixture.raw.len());
        fixture.counts.reset();
        fixture.map("/alias").await;
        fixture.counts.assert(0, 0);
    }
}

#[tokio::test]
async fn mst2_fixed_warm_map_still_checks_map_indices_batches_and_http_lease() {
    let fixture = Fixture::new().await;
    let map = fixture.map("/file").await;
    let map_id = map["map"]["map_id"].as_str().unwrap();
    let wrong_map = format!("sha256:{}", "1".repeat(64));
    fixture.counts.reset();
    error(
        fixture
            .send(
                "GET",
                &format!("chunk-map/pages?path=/file&map_id={wrong_map}&page_index=0"),
                Body::empty(),
            )
            .await,
        409,
        "EXPECTED_DIGEST_MISMATCH",
        false,
    )
    .await;
    error(
        fixture
            .send(
                "GET",
                &format!("chunk-map/pages?path=/file&map_id={map_id}&page_index=1"),
                Body::empty(),
            )
            .await,
        404,
        "PATH_NOT_FOUND",
        false,
    )
    .await;
    for (id, index, status, code) in [
        (wrong_map.as_str(), "0", 409, "EXPECTED_DIGEST_MISMATCH"),
        (map_id, "2", 400, "SCOPE_INVALID"),
    ] {
        error(
            fixture
                .send(
                    "POST",
                    "chunks",
                    Body::from(fixture.chunk_body("/file", id, index).to_string()),
                )
                .await,
            status,
            code,
            false,
        )
        .await;
    }
    let mut duplicate = fixture.chunk_body("/file", map_id, "0");
    let entry = duplicate["items"][0].clone();
    duplicate["items"].as_array_mut().unwrap().push(entry);
    error(
        fixture
            .send("POST", "chunks", Body::from(duplicate.to_string()))
            .await,
        400,
        "SCOPE_INVALID",
        false,
    )
    .await;
    let mut request = fixture.request("GET", "chunk-map?path=/file", Body::empty());
    request.headers_mut().remove("authorization");
    error(
        fixture.app.clone().oneshot(request).await.unwrap(),
        401,
        "UNAUTHENTICATED",
        false,
    )
    .await;
    let mut request = fixture.request("GET", "chunk-map?path=/file", Body::empty());
    request.headers_mut().remove("x-mega-snapshot-lease");
    error(
        fixture.app.clone().oneshot(request).await.unwrap(),
        401,
        "UNAUTHENTICATED",
        false,
    )
    .await;
    let response = fixture
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v2/snapshots/leases/{}", fixture.lease))
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    error(
        fixture
            .send("GET", "chunk-map?path=/file", Body::empty())
            .await,
        410,
        "LEASE_EXPIRED",
        false,
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn mst2_fixed_old_snapshot_reads_its_original_oid_after_real_ref_advances() {
    let fixture = Fixture::new().await;
    let mono = fixture.state.storage.mono_storage();
    let main = mono.get_main_ref("/").await.unwrap().unwrap();
    let project = mono.get_main_ref("/project").await.unwrap().unwrap();
    let old_commit =
        ObjectHash::from_hex_for_kind(HashKind::Sha1, &project.ref_commit_hash).unwrap();
    let blob_oid = ObjectHash::from_hex_for_kind(HashKind::Sha1, &fixture.oid).unwrap();
    let new_tree = tree(vec![item(TreeItemMode::Blob, blob_oid, "new-only")]);
    let new_commit = Commit::from_tree_id_with_kind(
        HashKind::Sha1,
        new_tree.id,
        vec![old_commit],
        "advanced project without old paths",
    )
    .unwrap();
    mono.save_mega_trees(vec![new_tree.clone()], new_commit.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![new_commit.clone()], None)
        .await
        .unwrap();
    publish_native_push(&fixture.state.storage, "/project", old_commit, &new_commit).await;
    let head = mono
        .read_native_publication_head(
            fixture
                .state
                .storage
                .config()
                .mst2
                .instance_uuid
                .as_deref()
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.token.sequence, 2);
    assert!(head.token.certificate.is_some());
    assert_ne!(head.root.commit, main.ref_commit_hash);
    assert_ne!(head.root.tree, main.ref_tree_hash);
    let published = mono.get_main_ref("/").await.unwrap().unwrap();
    assert_eq!(head.root.commit, published.ref_commit_hash);
    assert_eq!(head.root.tree, published.ref_tree_hash);
    let published_project = mono.get_main_ref("/project").await.unwrap().unwrap();
    assert_eq!(published_project.ref_commit_hash, new_commit.id.to_string());
    assert_eq!(published_project.ref_tree_hash, new_tree.id.to_string());
    let response = fixture.send("HEAD", "blob?path=/file", Body::empty()).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["etag"],
        format!("\"{}\"", fixture.digest_string())
    );
    fixture.counts.assert(0, 0);
    let map = fixture.map("/file").await;
    assert_eq!(map["map"]["file_content_id"], fixture.digest_string());
    fixture.counts.assert(1, fixture.raw.len());
}

#[tokio::test]
async fn mst2_fixed_metadata_walker_rejects_real_tree_gitlinks_without_body_reads() {
    let fixture = Fixture::new().await;
    let handler = MonoApiService::from(&fixture.state);
    let commit_oid = ObjectHash::from_hex_for_kind(
        HashKind::Sha1,
        &fixture
            .state
            .storage
            .mono_storage()
            .get_main_ref("/")
            .await
            .unwrap()
            .unwrap()
            .ref_commit_hash,
    )
    .unwrap();
    let root = tree(vec![item(TreeItemMode::Commit, commit_oid, "gitlink")]);
    fixture
        .state
        .storage
        .mono_storage()
        .save_mega_trees(vec![root.clone()], commit_oid, None)
        .await
        .unwrap();
    let error = resolve_abs_metadata(&handler, &root, "/gitlink")
        .await
        .unwrap_err();
    assert_eq!(error.code, SnapshotErrorCode::UnsupportedEntry);
    let file_oid = ObjectHash::from_hex_for_kind(HashKind::Sha1, &fixture.oid).unwrap();
    let file_root = tree(vec![item(TreeItemMode::Blob, file_oid, "file")]);
    match resolve_abs_metadata(&handler, &file_root, "/file")
        .await
        .unwrap()
    {
        MetadataWalkOutcome::FoundFile { fs_kind, oid } => {
            assert_eq!(fs_kind, FsKind::Regular);
            assert_eq!(oid, fixture.oid);
        }
        other => panic!("unexpected fixed outcome: {other:?}"),
    }
    assert!(matches!(
        resolve_abs_metadata(&handler, &file_root, "/missing/deep")
            .await
            .unwrap(),
        MetadataWalkOutcome::Absent
    ));
    fixture.counts.assert(0, 0);
}

#[path = "snapshot_storage_route_tests.rs"]
mod storage_routes;

#[path = "snapshot_storage_route_fixture.rs"]
mod storage_route_fixture;
