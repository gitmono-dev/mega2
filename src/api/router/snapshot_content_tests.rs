use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
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
    config::{model::PushPolicy, testing::isolated_config},
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
        tests::{test_redis_manager, test_storage_with_config, with_test_vault},
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

#[derive(Default)]
struct ReadCounts {
    whole: AtomicUsize,
    range: AtomicUsize,
    bytes: AtomicUsize,
}

impl ReadCounts {
    fn reset(&self) {
        self.whole.store(0, Ordering::SeqCst);
        self.range.store(0, Ordering::SeqCst);
        self.bytes.store(0, Ordering::SeqCst);
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
    async fn put_stream(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        meta: ObjectMeta,
    ) -> OrbitResult<()> {
        self.inner.inner.put_stream(key, data, meta).await
    }

    async fn get_stream(&self, key: &ObjectKey) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
        self.counts.whole.fetch_add(1, Ordering::SeqCst);
        let (stream, meta) = self.inner.inner.get_stream(key).await?;
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
    snapshot: String,
    lease: String,
    oid: String,
    raw: Vec<u8>,
    digest: [u8; 32],
    counts: Arc<ReadCounts>,
    _temp: tempfile::TempDir,
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
        let temp = tempfile::tempdir().unwrap();
        let mut config = isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.mst2.enabled = true;
        config.mst2.publication_enabled = true;
        config.mst2.instance_uuid = Some(uuid::Uuid::new_v4().to_string());
        config.mst2.auth_token = Some(TOKEN.to_string());
        let backend = build_object_storage(&config.object_storage).await.unwrap();
        let counts = Arc::new(ReadCounts::default());
        let mut storage = test_storage_with_config(temp.path(), config).await;
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
        let project = tree(vec![
            item(TreeItemMode::Blob, blob_oid, "alias"),
            item(TreeItemMode::Tree, empty_dir.id, "directory"),
            item(TreeItemMode::Blob, empty_oid, "empty"),
            item(TreeItemMode::BlobExecutable, blob_oid, "executable"),
            item(TreeItemMode::Blob, blob_oid, "file"),
            item(TreeItemMode::Link, link_oid, "link"),
            item(TreeItemMode::Tree, nested.id, "nested"),
        ]);
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
        mono.save_mega_trees(
            vec![empty_dir, nested, project, root.clone()],
            commit.id,
            None,
        )
        .await
        .unwrap();
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
                "/project".to_string(),
                MEGA_BRANCH_NAME.to_string(),
                old_tip.id.to_string(),
                old_tip.tree_id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        mono.initialize_native_publication(storage.config().mst2.instance_uuid.as_deref().unwrap())
            .await
            .unwrap();
        publish_native_push(&storage, "/project", old_tip.id, &new_tip).await;
        let head = mono
            .read_native_publication_head(storage.config().mst2.instance_uuid.as_deref().unwrap())
            .await
            .unwrap();
        assert_eq!(head.token.sequence, 1);
        assert!(head.token.certificate.is_some());
        assert_eq!(head.root.commit, commit.id.to_string());
        assert_eq!(head.root.tree, root.id.to_string());
        let state = MonoApiServiceState {
            entity_store: storage.entity_store.clone(),
            storage,
            session_store: BrowserSessionStore::Anonymous,
            git_object_cache: Arc::new(GitObjectCache {
                connection: test_redis_manager().await,
                prefix: String::new(),
            }),
            listen_addr: "127.0.0.1:0".to_string(),
        };
        let app = Router::new().nest("/api/v2", routers(state.clone()).with_state(state.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/snapshots/resolve")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"target":{"kind":"latest"},"scope":"/project"}).to_string(),
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
            _temp: temp,
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
        self.state
            .storage
            .git_service
            .save_object_from_model(bytes, &self.oid)
            .await
            .unwrap();
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
    assert_eq!(response.status().as_u16(), status);
    assert_eq!(response.headers()["content-type"], "application/json");
    let request_id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(!request_id.is_empty());
    let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["error"]["code"], code);
    assert_eq!(value["error"]["retryable"], retryable);
    assert_eq!(value["error"]["request_id"], request_id);
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
    fixture.counts.assert(1, fixture.raw.len());
}

#[tokio::test]
async fn mst2_fixed_warm_map_leaf_and_chunk_aliases_skip_body_reads() {
    let fixture = Fixture::new().await;
    let initial = fixture.map("/file").await;
    fixture.counts.assert(1, fixture.raw.len());
    assert_eq!(initial["map"]["file_content_id"], fixture.digest_string());
    assert_eq!(initial["map"]["file_size"], fixture.raw.len().to_string());
    assert_eq!(initial["map"]["chunk_count"], "2");
    let map_id = initial["map"]["map_id"].as_str().unwrap();
    fixture.counts.reset();
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
        fixture.counts.assert(0, 0);
    }
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
async fn mst2_fixed_verified_fact_db_error_is_not_missing_metadata_or_empty_content() {
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
        500,
        "INTERNAL",
        true,
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
