//! Raw Git bytes for one traces stream. The object key is derived here.
//! This module does not publish `trace_object` membership.

use std::sync::OnceLock;

use futures::StreamExt;
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
}
