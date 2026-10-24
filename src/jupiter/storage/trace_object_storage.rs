//! Raw Git bytes for one traces stream. The object key is derived here.
//! This module does not publish `trace_object` membership.

use std::sync::OnceLock;

use futures::StreamExt;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    common::errors::{MegaError, TracesError},
    jupiter::{
        storage::object_storage::MegaObjectStorageWrapper, utils::into_obj_stream::IntoObjectStream,
    },
    orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace},
};

const HASH_KIND_SHA1: &str = "sha1";
const HASH_KIND_SHA256: &str = "sha256";

/// Server-derived traces object key. Callers do not supply the storage path.
pub fn object_key(stream_id: &str, hash_kind: &str, oid: &str) -> Result<ObjectKey, MegaError> {
    let stream_id = canonical_stream_id(stream_id)?;
    let hash_kind = canonical_hash_kind(hash_kind)?;
    let oid = canonical_oid(oid)?;
    let key = ObjectKey {
        namespace: ObjectNamespace::Traces,
        key: format!("v1/{stream_id}/objects/{hash_kind}/{}", oid_shard(&oid)),
    };
    key.validate()
        .map_err(|_| MegaError::Traces(TracesError::InvalidParameter))?;
    Ok(key)
}

/// Store the original bytes. The same payload is idempotent. A different
/// payload for the same stream, hash kind, and OID is left untouched.
///
/// Puts in this process share one lock, so two tasks cannot both observe a
/// missing key and overwrite each other.
pub async fn put_raw(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    oid: &str,
    bytes: &[u8],
) -> Result<(), MegaError> {
    let key = object_key(stream_id, hash_kind, oid)?;
    let _gate = put_gate().lock().await;
    if let Some(existing) = read_stored(store, &key).await? {
        if existing.as_slice() == bytes {
            return Ok(());
        }
        return Err(MegaError::Traces(TracesError::ObjectIntegrity));
    }
    let meta = ObjectMeta {
        size: bytes.len() as i64,
        content_type: Some("application/octet-stream".to_string()),
        ..Default::default()
    };
    store
        .inner
        .put_stream(&key, bytes.to_vec().into_stream(), meta)
        .await?;
    match read_stored(store, &key).await? {
        Some(stored) if stored.as_slice() == bytes => Ok(()),
        _ => Err(MegaError::Traces(TracesError::ObjectIntegrity)),
    }
}

fn put_gate() -> &'static Mutex<()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
}

/// Read one archived object. A missing object is `None`.
pub async fn get_raw(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    oid: &str,
) -> Result<Option<Vec<u8>>, MegaError> {
    let key = object_key(stream_id, hash_kind, oid)?;
    read_stored(store, &key).await
}

/// Delta-base lookup for this stream only. There is no `git` or other-stream fallback.
pub async fn read_delta_base(
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    hash_kind: &str,
    oid: &str,
) -> Result<Option<Vec<u8>>, MegaError> {
    get_raw(store, stream_id, hash_kind, oid).await
}

fn canonical_stream_id(stream_id: &str) -> Result<String, MegaError> {
    Uuid::parse_str(stream_id)
        .map(|id| id.to_string())
        .map_err(|_| MegaError::Traces(TracesError::InvalidParameter))
}

fn canonical_hash_kind(hash_kind: &str) -> Result<&'static str, MegaError> {
    match hash_kind {
        HASH_KIND_SHA1 => Ok(HASH_KIND_SHA1),
        HASH_KIND_SHA256 => Ok(HASH_KIND_SHA256),
        _ => Err(MegaError::Traces(TracesError::InvalidParameter)),
    }
}

fn canonical_oid(oid: &str) -> Result<String, MegaError> {
    if oid.is_empty() || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(MegaError::Traces(TracesError::InvalidParameter));
    }
    Ok(oid.to_ascii_lowercase())
}

fn oid_shard(oid: &str) -> String {
    if oid.len() < 6 {
        return oid.to_owned();
    }
    let rest = &oid[6..];
    if rest.is_empty() {
        format!("{}/{}/{}", &oid[0..2], &oid[2..4], &oid[4..6])
    } else {
        format!("{}/{}/{}/{}", &oid[0..2], &oid[2..4], &oid[4..6], rest)
    }
}

/// Staging key for one quarantined object. It is not the published object key.
pub fn staging_object_key(
    ingest_id: &str,
    hash_kind: &str,
    oid: &str,
) -> Result<ObjectKey, MegaError> {
    let ingest_id = canonical_stream_id(ingest_id)?;
    let hash_kind = canonical_hash_kind(hash_kind)?;
    let oid = canonical_oid(oid)?;
    let key = ObjectKey {
        namespace: ObjectNamespace::Traces,
        key: format!(
            "v1/staging/{ingest_id}/objects/{hash_kind}/{}",
            oid_shard(&oid)
        ),
    };
    key.validate()
        .map_err(|_| MegaError::Traces(TracesError::InvalidParameter))?;
    Ok(key)
}

/// Git object id of `type size\\0payload` for `sha1` or `sha256`.
pub fn git_object_oid(
    hash_kind: &str,
    object_type: &str,
    payload: &[u8],
) -> Result<String, MegaError> {
    let kind = match canonical_hash_kind(hash_kind)? {
        HASH_KIND_SHA1 => git_internal::hash::HashKind::Sha1,
        HASH_KIND_SHA256 => git_internal::hash::HashKind::Sha256,
        _ => return Err(MegaError::Traces(TracesError::InvalidParameter)),
    };
    let object_type = match object_type {
        "blob" => git_internal::internal::object::types::ObjectType::Blob,
        "tree" => git_internal::internal::object::types::ObjectType::Tree,
        "commit" => git_internal::internal::object::types::ObjectType::Commit,
        "tag" => git_internal::internal::object::types::ObjectType::Tag,
        _ => return Err(MegaError::Traces(TracesError::InvalidParameter)),
    };
    git_internal::hash::ObjectHash::from_type_and_data_for_kind(kind, object_type, payload)
        .map(|oid| oid.to_string())
        .map_err(|_| MegaError::Traces(TracesError::InvalidParameter))
}

/// Reject a declared id that is not the Git id of this payload.
pub fn verify_declared_oid(
    hash_kind: &str,
    object_type: &str,
    payload: &[u8],
    declared_oid: &str,
) -> Result<String, MegaError> {
    let declared = canonical_oid(declared_oid)?;
    let actual = git_object_oid(hash_kind, object_type, payload)?;
    if actual != declared {
        return Err(MegaError::Traces(TracesError::ObjectIntegrity));
    }
    Ok(actual)
}

/// Write one object under the ingest quarantine prefix.
pub async fn put_staging(
    store: &MegaObjectStorageWrapper,
    ingest_id: &str,
    hash_kind: &str,
    oid: &str,
    bytes: &[u8],
) -> Result<(), MegaError> {
    let key = staging_object_key(ingest_id, hash_kind, oid)?;
    let _gate = put_gate().lock().await;
    if let Some(existing) = read_stored(store, &key).await? {
        if existing.as_slice() == bytes {
            return Ok(());
        }
        return Err(MegaError::Traces(TracesError::ObjectIntegrity));
    }
    let meta = ObjectMeta {
        size: bytes.len() as i64,
        content_type: Some("application/octet-stream".to_string()),
        ..Default::default()
    };
    store
        .inner
        .put_stream(&key, bytes.to_vec().into_stream(), meta)
        .await?;
    match read_stored(store, &key).await? {
        Some(stored) if stored.as_slice() == bytes => Ok(()),
        _ => Err(MegaError::Traces(TracesError::ObjectIntegrity)),
    }
}

/// Read one quarantined object. A missing object is `None`.
pub async fn get_staging(
    store: &MegaObjectStorageWrapper,
    ingest_id: &str,
    hash_kind: &str,
    oid: &str,
) -> Result<Option<Vec<u8>>, MegaError> {
    let key = staging_object_key(ingest_id, hash_kind, oid)?;
    read_stored(store, &key).await
}

async fn delete_staging(
    store: &MegaObjectStorageWrapper,
    ingest_id: &str,
    hash_kind: &str,
    oid: &str,
) -> Result<(), MegaError> {
    let key = staging_object_key(ingest_id, hash_kind, oid)?;
    match store.inner.delete(&key).await {
        Ok(()) => Ok(()),
        Err(err) => match MegaError::from(err) {
            MegaError::ObjStorageNotFound(_) => Ok(()),
            other => Err(other),
        },
    }
}

/// Delete staged copies whose oid is not a visible membership of this stream.
/// Published object keys and membership rows are left in place.
pub async fn recover_quarantine_objects<C: ConnectionTrait>(
    conn: &C,
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    ingest_id: &str,
    hash_kind: &str,
    staged_oids: &[String],
) -> Result<Vec<String>, MegaError> {
    let stream_id = canonical_stream_id(stream_id)?;
    let hash_kind = canonical_hash_kind(hash_kind)?;
    let mut deleted = Vec::new();
    for oid in staged_oids {
        let oid = canonical_oid(oid)?;
        if staging_oid_published(conn, &stream_id, hash_kind, &oid).await? {
            continue;
        }
        delete_staging(store, ingest_id, hash_kind, &oid).await?;
        deleted.push(oid);
    }
    Ok(deleted)
}

async fn staging_oid_published<C: ConnectionTrait>(
    conn: &C,
    stream_id: &str,
    hash_kind: &str,
    oid: &str,
) -> Result<bool, MegaError> {
    let count = crate::callisto::trace_object::Entity::find()
        .filter(crate::callisto::trace_object::Column::StreamId.eq(stream_id))
        .filter(crate::callisto::trace_object::Column::HashKind.eq(hash_kind))
        .filter(crate::callisto::trace_object::Column::Oid.eq(oid))
        .filter(crate::callisto::trace_object::Column::Visible.eq(true))
        .count(conn)
        .await?;
    Ok(count > 0)
}

async fn read_stored(
    store: &MegaObjectStorageWrapper,
    key: &ObjectKey,
) -> Result<Option<Vec<u8>>, MegaError> {
    match store.inner.get_stream(key).await {
        Ok((mut stream, _)) => {
            let mut data = Vec::new();
            while let Some(chunk) = stream.next().await {
                data.extend_from_slice(&chunk?);
            }
            Ok(Some(data))
        }
        Err(err) => match MegaError::from(err) {
            MegaError::ObjStorageNotFound(_) => Ok(None),
            other => Err(other),
        },
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{EntityTrait, PaginatorTrait};

    use super::*;
    use crate::{
        callisto::{trace_object, trace_stream},
        jupiter::{
            migration::apply_migrations, storage::object_storage::mock_object_storage,
            tests::test_db_connection,
        },
        orbit_api::object_storage::ObjectNamespace,
    };

    struct RawChild(std::process::Child, std::path::PathBuf);

    impl RawChild {
        fn wait(&mut self) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(50);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "raw child exited {status}: {}",
                        std::fs::read_to_string(&self.1).unwrap_or_default()
                    );
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "raw child deadline");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    impl Drop for RawChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    fn raw_wait_ready(path: &std::path::Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(35);
        while !path.try_exists().unwrap() {
            assert!(
                std::time::Instant::now() < deadline,
                "child did not observe real NotFound"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn raw_cross_process_matrix(rustfs: bool, same: bool, simultaneous: bool) {
        use crate::orbit_api::factory::{
            LocalConfig, ObjectStorageBackend, ObjectStorageConfig, S3Config,
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for staging in [false, true] {
            for multipart in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let backend_dir = directory.path().join("objects");
                std::fs::create_dir(&backend_dir).unwrap();
                let cfg = if rustfs {
                    ObjectStorageConfig {
                        storage_type: ObjectStorageBackend::S3Compatible,
                        s3: S3Config {
                            region: "us-east-1".into(),
                            bucket: "mega2".into(),
                            access_key_id: "rustfs".into(),
                            secret_access_key: "rustfs_secret".into(),
                            endpoint_url: std::env::var("MEGA2_IT_RUSTFS_URL")
                                .unwrap_or_else(|_| "http://127.0.0.1:19000".into()),
                            public_endpoint_url: String::new(),
                        },
                        ..Default::default()
                    }
                } else {
                    ObjectStorageConfig {
                        local: LocalConfig {
                            root_dir: backend_dir.to_str().unwrap().into(),
                        },
                        ..Default::default()
                    }
                };
                std::fs::write(
                    directory.path().join("config.json"),
                    serde_json::to_vec(&cfg).unwrap(),
                )
                .unwrap();
                let store = runtime
                    .block_on(crate::orbit::factory::ObjectStorageFactory::build(&cfg))
                    .unwrap();
                let id = Uuid::new_v4().to_string();
                let key = if staging {
                    staging_object_key(&id, "sha1", "abcdef")
                } else {
                    object_key(&id, "sha1", "abcdef")
                }
                .unwrap();
                let read = || {
                    runtime.block_on(read_stored(&store, &key)).expect("canonical RustFS is required; run docker compose -p mega2-it -f docker/docker-compose.test.yml up -d --wait and initialize the test bucket")
                };
                assert!(read().is_none());
                let spawn = |role: &str, retry: bool| {
                    let payload = if role == "a" || same {
                        "first-payload"
                    } else {
                        "other-payload"
                    };
                    let release = if simultaneous {
                        "release".to_owned()
                    } else {
                        format!("{role}.release")
                    };
                    let log_path = directory.path().join(format!("{role}.child.log"));
                    let log = std::fs::File::create(&log_path).unwrap();
                    RawChild(
                        std::process::Command::new(std::env::current_exe().unwrap())
                            .args([
                                "--exact",
                                "orbit::adapter::tests::traces_immutable_create_child",
                                "--nocapture",
                            ])
                            .env("MEGA2_FIX07_CHILD_DIRECTORY", directory.path())
                            .env("MEGA2_FIX07_ROLE", role)
                            .env("MEGA2_FIX07_RELEASE", release)
                            .env("MEGA2_FIX07_MULTIPART", multipart.to_string())
                            .env("MEGA2_FIX07_STAGING", staging.to_string())
                            .env("MEGA2_FIX07_RETRY", retry.to_string())
                            .env("MEGA2_FIX07_ID", &id)
                            .env("MEGA2_FIX07_PAYLOAD", payload)
                            .stdin(std::process::Stdio::null())
                            .stdout(log.try_clone().unwrap())
                            .stderr(log)
                            .spawn()
                            .unwrap(),
                        log_path,
                    )
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut a = spawn("a", false);
                    let mut b = spawn("b", false);
                    raw_wait_ready(&directory.path().join("a.ready"));
                    raw_wait_ready(&directory.path().join("b.ready"));
                    if simultaneous {
                        std::fs::write(directory.path().join("release"), b"go").unwrap();
                    } else {
                        std::fs::write(directory.path().join("a.release"), b"go").unwrap();
                        a.wait();
                        assert_eq!(read().unwrap(), b"first-payload");
                        std::fs::write(directory.path().join("b.release"), b"go").unwrap();
                    }
                    a.wait();
                    b.wait();
                    let result_for = |role: &str| {
                        std::fs::read_to_string(directory.path().join(format!("{role}.result")))
                            .unwrap()
                    };
                    let first = [result_for("a"), result_for("b")];
                    assert!(
                        first.iter().any(|value| value == "ok"),
                        "no successful creator: {first:?}"
                    );
                    let winner = read().unwrap();
                    assert!(winner == b"first-payload" || (!same && winner == b"other-payload"));
                    for role in ["a", "b"] {
                        if result_for(role) == "retryable" {
                            spawn(role, true).wait();
                        }
                    }
                    let results = [result_for("a"), result_for("b")];
                    if same {
                        assert_eq!(results, ["ok", "ok"]);
                    } else if simultaneous {
                        assert!(
                            results == ["ok", "integrity"] || results == ["integrity", "ok"],
                            "{results:?}"
                        );
                        assert_eq!(
                            winner,
                            if results[0] == "ok" {
                                b"first-payload".as_slice()
                            } else {
                                b"other-payload".as_slice()
                            }
                        );
                    } else {
                        assert_eq!(results, ["ok", "integrity"]);
                        assert_eq!(winner, b"first-payload");
                    }
                    assert_eq!(read().unwrap(), winner);
                    eprintln!(
                        "raw process oracle backend={} staging={staging} multipart={multipart} same={same} simultaneous={simultaneous} results={results:?}",
                        if rustfs { "RustFS" } else { "Local" }
                    );
                }));
                runtime
                    .block_on(store.inner.delete(&key))
                    .expect("cleanup owned raw key");
                assert!(read().is_none(), "owned raw key leaked");
                if let Err(panic) = result {
                    std::panic::resume_unwind(panic);
                }
            }
        }
    }

    #[test]
    fn raw_cross_process_local_sequential_different() {
        raw_cross_process_matrix(false, false, false);
    }
    #[test]
    fn raw_cross_process_local_sequential_same() {
        raw_cross_process_matrix(false, true, false);
    }
    #[test]
    fn raw_cross_process_local_simultaneous_different() {
        raw_cross_process_matrix(false, false, true);
    }
    #[test]
    fn raw_cross_process_local_simultaneous_same() {
        raw_cross_process_matrix(false, true, true);
    }
    #[test]
    fn raw_cross_process_rustfs_sequential_different() {
        raw_cross_process_matrix(true, false, false);
    }
    #[test]
    fn raw_cross_process_rustfs_sequential_same() {
        raw_cross_process_matrix(true, true, false);
    }
    #[test]
    fn raw_cross_process_rustfs_simultaneous_different() {
        raw_cross_process_matrix(true, false, true);
    }
    #[test]
    fn raw_cross_process_rustfs_simultaneous_same() {
        raw_cross_process_matrix(true, true, true);
    }

    const STREAM_A: &str = "11111111-1111-4111-8111-111111111111";
    const STREAM_B: &str = "22222222-2222-4222-8222-222222222222";
    const OID_SHA1: &str = "abcdef0123456789abcdef0123456789abcdef01";
    const OID_TREE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const OID_BLOB: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const OID_TAG: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    async fn open_db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        (dir, db)
    }

    async fn assert_no_membership(db: &sea_orm::DatabaseConnection) {
        assert_eq!(
            trace_stream::Entity::find()
                .count(db)
                .await
                .expect("stream count"),
            0
        );
        assert_eq!(
            trace_object::Entity::find()
                .count(db)
                .await
                .expect("object count"),
            0
        );
    }

    fn commit_payload() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"tree 341e54913a3a43069f2927cc0f703e5a9f730df1\n");
        bytes.extend_from_slice(b"parent 1111111111111111111111111111111111111111\n");
        bytes.extend_from_slice(b"unknown-header kept\n");
        bytes.extend_from_slice(b"author Synthetic <synthetic@example.test> 0 +0000\n");
        bytes.extend_from_slice(b"committer Synthetic <synthetic@example.test> 0 +0000\n");
        bytes.extend_from_slice(b"gpgsig -----BEGIN PGP SIGNATURE-----\n");
        bytes.extend_from_slice(b" x\n");
        bytes.extend_from_slice(b" -----END PGP SIGNATURE-----\n");
        bytes.extend_from_slice(b"\nmessage ");
        bytes.push(0xff);
        bytes.push(b'\n');
        bytes
    }

    fn tree_payload() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"100644 file");
        bytes.push(0xff);
        bytes.push(0);
        bytes.extend_from_slice(&[0x11; 20]);
        bytes
    }

    fn blob_payload() -> Vec<u8> {
        let mut bytes = b"blob-body".to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b"\0tail");
        bytes
    }

    fn tag_payload() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"object 341e54913a3a43069f2927cc0f703e5a9f730df1\n");
        bytes.extend_from_slice(b"type commit\n");
        bytes.extend_from_slice(b"tag v1\n");
        bytes.extend_from_slice(b"unknown-header kept\n");
        bytes.extend_from_slice(b"tagger Synthetic <synthetic@example.test> 0 +0000\n\nnote ");
        bytes.push(0xff);
        bytes
    }

    fn assert_commit_model_rejects(payload: &[u8]) {
        use git_internal::{
            hash::{HashKind, ObjectHash},
            internal::object::{ObjectTrait, commit::Commit},
        };
        let hash = ObjectHash::from_hex_for_kind(HashKind::Sha1, OID_SHA1).expect("oid");
        assert!(
            Commit::from_bytes(payload, hash).is_err(),
            "relational commit model must reject unknown headers"
        );
    }

    #[tokio::test]
    async fn raw_storage_preserves_commit_tree_blob_and_tag() {
        let (_dir, db) = open_db().await;
        let store = mock_object_storage();
        let commit = commit_payload();
        assert_commit_model_rejects(&commit);
        assert!(
            commit
                .windows(b"unknown-header".len())
                .any(|w| w == b"unknown-header")
        );
        assert!(commit.windows(b"gpgsig".len()).any(|w| w == b"gpgsig"));
        assert!(commit.contains(&0xff));

        put_raw(&store, STREAM_A, "sha1", OID_SHA1, &commit)
            .await
            .expect("commit");
        put_raw(&store, STREAM_A, "sha1", OID_TREE, &tree_payload())
            .await
            .expect("tree");
        put_raw(&store, STREAM_A, "sha1", OID_BLOB, &blob_payload())
            .await
            .expect("blob");
        put_raw(&store, STREAM_A, "sha256", OID_TAG, &tag_payload())
            .await
            .expect("tag");

        assert_eq!(
            get_raw(&store, STREAM_A, "sha1", OID_SHA1)
                .await
                .expect("read commit"),
            Some(commit)
        );
        assert_eq!(
            get_raw(&store, STREAM_A, "sha1", OID_TREE)
                .await
                .expect("read tree"),
            Some(tree_payload())
        );
        assert_eq!(
            get_raw(&store, STREAM_A, "sha1", OID_BLOB)
                .await
                .expect("read blob"),
            Some(blob_payload())
        );
        assert_eq!(
            get_raw(&store, STREAM_A, "sha256", OID_TAG)
                .await
                .expect("read tag"),
            Some(tag_payload())
        );
        assert_no_membership(&db).await;
    }

    #[tokio::test]
    async fn raw_storage_replays_same_bytes_and_keeps_conflict() {
        let (_dir, db) = open_db().await;
        let store = mock_object_storage();
        let first = blob_payload();
        put_raw(&store, STREAM_A, "SHA1", OID_SHA1, &first)
            .await
            .expect_err("hash kind is exact");
        put_raw(&store, STREAM_A, "sha1", OID_SHA1, &first)
            .await
            .expect("first");
        put_raw(
            &store,
            &STREAM_A.to_ascii_uppercase(),
            "sha1",
            &OID_SHA1.to_ascii_uppercase(),
            &first,
        )
        .await
        .expect("same bytes");
        let err = put_raw(&store, STREAM_A, "sha1", OID_SHA1, b"other")
            .await
            .expect_err("conflict");
        assert!(err.to_string().contains("object_integrity"), "{err}");
        assert_eq!(
            get_raw(&store, STREAM_A, "sha1", OID_SHA1)
                .await
                .expect("kept"),
            Some(first)
        );
        assert_no_membership(&db).await;
    }

    #[tokio::test]
    async fn raw_storage_delta_base_ignores_git_agent_and_other_stream() {
        let (_dir, db) = open_db().await;
        let store = mock_object_storage();
        let own = commit_payload();
        let other = b"other-stream".to_vec();
        let git_bytes = b"global-git-object".to_vec();
        let agent_bytes = b"agent-object".to_vec();
        let traces_key = object_key(STREAM_A, "sha1", OID_SHA1).expect("key");
        assert_eq!(
            traces_key.default_sharding(),
            format!(
                "traces/v1/{STREAM_A}/objects/sha1/ab/cd/ef/0123456789abcdef0123456789abcdef01"
            )
        );
        assert!(traces_key.default_sharding().starts_with("traces/"));
        assert!(!traces_key.default_sharding().starts_with("git/"));
        assert!(!traces_key.default_sharding().starts_with("agent/"));

        store
            .inner
            .put_stream(
                &ObjectKey {
                    namespace: ObjectNamespace::Git,
                    key: OID_SHA1.to_owned(),
                },
                git_bytes.clone().into_stream(),
                ObjectMeta {
                    size: git_bytes.len() as i64,
                    ..Default::default()
                },
            )
            .await
            .expect("git plant");
        store
            .inner
            .put_stream(
                &ObjectKey {
                    namespace: ObjectNamespace::Agent,
                    key: traces_key.key.clone(),
                },
                agent_bytes.clone().into_stream(),
                ObjectMeta {
                    size: agent_bytes.len() as i64,
                    ..Default::default()
                },
            )
            .await
            .expect("agent plant");
        put_raw(&store, STREAM_B, "sha1", OID_SHA1, &other)
            .await
            .expect("other stream");

        assert_eq!(
            read_delta_base(&store, STREAM_A, "sha1", OID_SHA1)
                .await
                .expect("missing in this stream"),
            None
        );
        put_raw(&store, STREAM_A, "sha1", OID_SHA1, &own)
            .await
            .expect("own");
        assert_eq!(
            read_delta_base(&store, STREAM_A, "sha1", OID_SHA1)
                .await
                .expect("own base"),
            Some(own)
        );
        assert_eq!(
            read_delta_base(&store, STREAM_B, "sha1", OID_SHA1)
                .await
                .expect("other base"),
            Some(other)
        );
        let git_only = b"git-only-blob".to_vec();
        store
            .inner
            .put_stream(
                &ObjectKey {
                    namespace: ObjectNamespace::Git,
                    key: OID_BLOB.to_owned(),
                },
                git_only.clone().into_stream(),
                ObjectMeta {
                    size: git_only.len() as i64,
                    ..Default::default()
                },
            )
            .await
            .expect("git-only plant");
        assert_eq!(
            read_delta_base(&store, STREAM_A, "sha1", OID_BLOB)
                .await
                .expect("git-only oid"),
            None
        );
        let (mut git_stream, _) = store
            .inner
            .get_stream(&ObjectKey {
                namespace: ObjectNamespace::Git,
                key: OID_SHA1.to_owned(),
            })
            .await
            .expect("git remains");
        let git_chunk = git_stream
            .next()
            .await
            .expect("git chunk")
            .expect("git bytes");
        assert_eq!(git_chunk.as_ref(), git_bytes.as_slice());
        assert_no_membership(&db).await;
    }

    #[tokio::test]
    async fn raw_storage_derives_shard_and_rejects_client_key() {
        let (_dir, db) = open_db().await;
        let store = mock_object_storage();
        let short = object_key(STREAM_A, "sha1", "abcd").expect("short");
        assert_eq!(
            short.default_sharding(),
            format!("traces/v1/{STREAM_A}/objects/sha1/abcd")
        );
        let exact = object_key(STREAM_A, "sha256", "abcdef").expect("exact");
        assert_eq!(
            exact.default_sharding(),
            format!("traces/v1/{STREAM_A}/objects/sha256/ab/cd/ef")
        );
        assert!(
            !exact.default_sharding().ends_with('/'),
            "six-byte oid has no empty segment"
        );

        for (stream_id, hash_kind, oid) in [
            ("not-a-uuid", "sha1", OID_SHA1),
            (STREAM_A, "sha-1", OID_SHA1),
            (STREAM_A, "blake3", OID_SHA1),
            (STREAM_A, "sha1", ""),
            (STREAM_A, "sha1", "../git"),
            (STREAM_A, "sha1", "ab/cd"),
            (STREAM_A, "sha1", "zzzz"),
            ("../git", "sha1", OID_SHA1),
        ] {
            let err = put_raw(&store, stream_id, hash_kind, oid, b"nope")
                .await
                .expect_err("rejected");
            assert!(err.to_string().contains("invalid_parameter"), "{err}");
            assert_eq!(
                get_raw(&store, STREAM_A, "sha1", OID_SHA1)
                    .await
                    .expect("nothing stored"),
                None
            );
        }
        assert_no_membership(&db).await;
    }

    #[tokio::test]
    async fn raw_storage_concurrent_conflict_keeps_the_visible_payload() {
        let (_dir, db) = open_db().await;
        let store = mock_object_storage();
        let left_store = store.clone();
        let right_store = store.clone();
        let left = tokio::spawn(async move {
            put_raw(
                &left_store,
                STREAM_A,
                "sha1",
                OID_BLOB,
                b"payload-one-bytes",
            )
            .await
        });
        let right = tokio::spawn(async move {
            put_raw(
                &right_store,
                STREAM_A,
                "sha1",
                OID_BLOB,
                b"payload-two-bytes",
            )
            .await
        });
        let left = left.await.expect("left task");
        let right = right.await.expect("right task");
        let stored = get_raw(&store, STREAM_A, "sha1", OID_BLOB)
            .await
            .expect("read")
            .expect("one payload remains");
        match (left, right) {
            (Ok(()), Err(err)) => {
                assert!(err.to_string().contains("object_integrity"), "{err}");
                assert_eq!(stored, b"payload-one-bytes");
            }
            (Err(err), Ok(())) => {
                assert!(err.to_string().contains("object_integrity"), "{err}");
                assert_eq!(stored, b"payload-two-bytes");
            }
            (Ok(()), Ok(())) => panic!("different payloads both reported success"),
            (Err(left_err), Err(right_err)) => {
                panic!("both writes failed: {left_err}; {right_err}")
            }
        }
        let same_store = store.clone();
        let replay_store = store.clone();
        let same = tokio::spawn(async move {
            put_raw(&same_store, STREAM_A, "sha1", OID_TREE, b"same-payload").await
        });
        let replay = tokio::spawn(async move {
            put_raw(&replay_store, STREAM_A, "sha1", OID_TREE, b"same-payload").await
        });
        same.await.expect("same task").expect("same bytes");
        replay.await.expect("replay task").expect("same bytes");
        assert_eq!(
            get_raw(&store, STREAM_A, "sha1", OID_TREE)
                .await
                .expect("same read"),
            Some(b"same-payload".to_vec())
        );
        assert_no_membership(&db).await;
    }

    #[tokio::test]
    async fn quarantine_object_recovery_keeps_published_bytes() {
        use chrono::Utc;
        use sea_orm::{ActiveModelTrait, ColumnTrait, QueryFilter, Set};
        use uuid::Uuid;

        use crate::callisto::{trace_principal, trace_target};

        let (_dir, db) = open_db().await;
        let store = mock_object_storage();
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let now = Utc::now().fixed_offset();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("principal");
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/team/{target_id}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("target");
        let stream_id = trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id),
            target_id: Set(target_id),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("stream")
        .id;
        let published = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let unpublished = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let ingest_id = "33333333-3333-4333-8333-333333333333";
        put_raw(&store, &stream_id, "sha1", published, b"keep-me")
            .await
            .expect("durable");
        put_staging(&store, ingest_id, "sha1", published, b"keep-me")
            .await
            .expect("published staging");
        put_staging(&store, ingest_id, "sha1", unpublished, b"drop-me")
            .await
            .expect("unpublished staging");
        trace_object::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            hash_kind: Set("sha1".to_owned()),
            oid: Set(published.to_owned()),
            visible: Set(true),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("membership");
        let deleted = recover_quarantine_objects(
            &db,
            &store,
            &stream_id,
            ingest_id,
            "sha1",
            &[published.to_owned(), unpublished.to_owned()],
        )
        .await
        .expect("recover");
        assert_eq!(deleted, vec![unpublished.to_owned()]);
        assert_eq!(
            get_raw(&store, &stream_id, "sha1", published)
                .await
                .expect("durable read"),
            Some(b"keep-me".to_vec())
        );
        assert_eq!(
            get_staging(&store, ingest_id, "sha1", published)
                .await
                .expect("published staging"),
            Some(b"keep-me".to_vec())
        );
        assert_eq!(
            get_staging(&store, ingest_id, "sha1", unpublished)
                .await
                .expect("unpublished staging"),
            None
        );
        assert_eq!(
            trace_object::Entity::find()
                .filter(trace_object::Column::Oid.eq(published))
                .count(&db)
                .await
                .expect("membership count"),
            1
        );
    }
}
