//! Service entry for raw traces objects. Key derivation and byte preservation
//! stay in `trace_object_storage`.

use std::future::Future;

use crate::{
    ceres::traces::admission::IngestContinuation,
    common::errors::{MegaError, TracesError},
    config::TracesProtocolLimits,
    jupiter::storage::{object_storage::MegaObjectStorageWrapper, trace_object_storage},
};

/// Push retains its original deadline; fetch supplies request cancellation only.
pub(crate) trait RawReadControl: Sync {
    fn check(&self) -> Result<(), MegaError>;

    fn checkpoint(&self) -> impl Future<Output = Result<(), MegaError>> + Send;

    fn run<T: Send>(
        &self,
        future: impl Future<Output = Result<T, MegaError>> + Send,
    ) -> impl Future<Output = Result<T, MegaError>> + Send;
}

impl RawReadControl for IngestContinuation {
    fn check(&self) -> Result<(), MegaError> {
        IngestContinuation::check(self)
    }

    async fn checkpoint(&self) -> Result<(), MegaError> {
        IngestContinuation::checkpoint(self).await
    }

    async fn run<T: Send>(
        &self,
        future: impl Future<Output = Result<T, MegaError>> + Send,
    ) -> Result<T, MegaError> {
        IngestContinuation::run(self, future).await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawObjectType {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl RawObjectType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Tree => "tree",
            Self::Blob => "blob",
            Self::Tag => "tag",
        }
    }
}

pub(crate) struct VerifiedRawObject {
    object_type: RawObjectType,
    bytes: Vec<u8>,
}

impl VerifiedRawObject {
    pub(crate) fn object_type(&self) -> RawObjectType {
        self.object_type
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

async fn hash_payload<D: sha2::Digest + Default>(
    object_type: RawObjectType,
    payload: &[u8],
    control: &impl RawReadControl,
) -> Result<String, MegaError> {
    let mut hash = D::new();
    hash.update(format!("{} {}\0", object_type.as_str(), payload.len()).as_bytes());
    for chunk in payload.chunks(64 * 1024) {
        control.checkpoint().await?;
        hash.update(chunk);
    }
    control.check()?;
    Ok(hex::encode(hash.finalize()))
}

pub(crate) async fn raw_object_oid(
    hash_kind: &str,
    object_type: RawObjectType,
    payload: &[u8],
    control: &impl RawReadControl,
) -> Result<String, MegaError> {
    if payload.len() as u64 > TracesProtocolLimits::MAX_OBJECT_BYTES {
        return Err(MegaError::Traces(TracesError::ResponseBudget));
    }
    match hash_kind {
        "sha1" => hash_payload::<sha1::Sha1>(object_type, payload, control).await,
        "sha256" => hash_payload::<sha2::Sha256>(object_type, payload, control).await,
        _ => Err(MegaError::Traces(TracesError::InvalidParameter)),
    }
}

pub(crate) async fn verify_raw_object(
    hash_kind: &str,
    oid: &str,
    bytes: Vec<u8>,
    control: &impl RawReadControl,
) -> Result<VerifiedRawObject, MegaError> {
    let width = match hash_kind {
        "sha1" => 40,
        "sha256" => 64,
        _ => return Err(MegaError::Traces(TracesError::InvalidParameter)),
    };
    if oid.len() != width || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MegaError::Traces(TracesError::ObjectIntegrity));
    }
    let mut matching = None;
    for object_type in [
        RawObjectType::Commit,
        RawObjectType::Tree,
        RawObjectType::Blob,
        RawObjectType::Tag,
    ] {
        let actual = raw_object_oid(hash_kind, object_type, &bytes, control).await?;
        if actual.eq_ignore_ascii_case(oid) && matching.replace(object_type).is_some() {
            return Err(MegaError::Traces(TracesError::ObjectIntegrity));
        }
    }
    Ok(VerifiedRawObject {
        object_type: matching.ok_or(MegaError::Traces(TracesError::ObjectIntegrity))?,
        bytes,
    })
}

pub(crate) fn raw_storage_error(error: MegaError) -> MegaError {
    match error {
        MegaError::Traces(_) => error,
        // A conditional-create visibility race and backend IO both fail closed.
        _ => MegaError::Traces(TracesError::QueryTimeout),
    }
}

/// Reads and writes raw Git bytes for one stream.
#[derive(Clone)]
pub struct TraceObjectService {
    store: MegaObjectStorageWrapper,
}

impl TraceObjectService {
    pub fn new(store: MegaObjectStorageWrapper) -> Self {
        Self { store }
    }

    pub fn mock() -> Self {
        Self::new(crate::jupiter::storage::object_storage::mock_object_storage())
    }

    pub async fn put_raw(
        &self,
        stream_id: &str,
        hash_kind: &str,
        oid: &str,
        bytes: &[u8],
    ) -> Result<(), MegaError> {
        trace_object_storage::put_raw(&self.store, stream_id, hash_kind, oid, bytes).await
    }

    pub async fn get_raw(
        &self,
        stream_id: &str,
        hash_kind: &str,
        oid: &str,
    ) -> Result<Option<Vec<u8>>, MegaError> {
        trace_object_storage::get_raw(&self.store, stream_id, hash_kind, oid).await
    }

    pub async fn read_delta_base(
        &self,
        stream_id: &str,
        hash_kind: &str,
        oid: &str,
    ) -> Result<Option<Vec<u8>>, MegaError> {
        trace_object_storage::read_delta_base(&self.store, stream_id, hash_kind, oid).await
    }

    /// Verifies identity only; the caller must independently qualify stream membership.
    pub(crate) async fn read_verified_raw(
        &self,
        stream_id: &str,
        hash_kind: &str,
        oid: &str,
        control: &impl RawReadControl,
    ) -> Result<Option<VerifiedRawObject>, MegaError> {
        use futures::StreamExt;

        let key = trace_object_storage::object_key(stream_id, hash_kind, oid)?;
        let bytes = control
            .run(async {
                let (mut stream, meta) = match self.store.inner.get_stream(&key).await {
                    Ok(value) => value,
                    Err(error) if error.is_not_found() => return Ok(None),
                    Err(error) => return Err(raw_storage_error(error.into())),
                };
                let size = u64::try_from(meta.size)
                    .map_err(|_| MegaError::Traces(TracesError::ObjectIntegrity))?;
                if size > TracesProtocolLimits::MAX_OBJECT_BYTES {
                    return Err(MegaError::Traces(TracesError::ResponseBudget));
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    control.checkpoint().await?;
                    let chunk = chunk.map_err(|error| raw_storage_error(error.into()))?;
                    let total = bytes
                        .len()
                        .checked_add(chunk.len())
                        .ok_or(MegaError::Traces(TracesError::ResponseBudget))?;
                    if total as u64 > TracesProtocolLimits::MAX_OBJECT_BYTES {
                        return Err(MegaError::Traces(TracesError::ResponseBudget));
                    }
                    bytes
                        .try_reserve_exact(chunk.len())
                        .map_err(|_| MegaError::Traces(TracesError::ResponseBudget))?;
                    bytes.extend_from_slice(&chunk);
                }
                if bytes.len() as u64 != size {
                    return Err(MegaError::Traces(TracesError::ObjectIntegrity));
                }
                Ok(Some(bytes))
            })
            .await?;
        match bytes {
            Some(bytes) => verify_raw_object(hash_kind, oid, bytes, control)
                .await
                .map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use sea_orm::{EntityTrait, PaginatorTrait};

    use super::*;
    use crate::{
        callisto::{trace_object, trace_stream},
        jupiter::{
            migration::apply_migrations,
            storage::{object_storage::mock_object_storage, trace_object_storage::object_key},
            tests::test_db_connection,
            utils::into_obj_stream::IntoObjectStream,
        },
        orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace},
    };

    #[tokio::test]
    async fn verified_raw_type_real_git_four_types_both_hashes_and_corruption() {
        use git_internal::hash::HashKind;

        use crate::ceres::traces::publication::tests::{Fixture, local_store};
        let (_directory, store) = local_store().await;
        let service = TraceObjectService::new(store);
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut fixture = Fixture::new(kind);
            fixture.object("blob", b"raw\x00\xffbytes");
            let tree = fixture.tree(&[]);
            let commit = fixture.commit(&tree, None);
            fixture.object("tag", format!("object {commit}\ntype commit\ntag sample\ntagger Test <test@example.test> 0 +0000\n\nraw tag\n").as_bytes());
            let decoded = fixture.decoded(None).await;
            let control = decoded.continuation();
            let stream = uuid::Uuid::new_v4().to_string();
            for (oid, kind, bytes) in &fixture.objects {
                let verified = verify_raw_object(fixture.hash_kind(), oid, bytes.clone(), control)
                    .await
                    .unwrap();
                assert_eq!(verified.object_type().as_str(), kind);
                assert_eq!(verified.bytes(), bytes);
                service
                    .put_raw(&stream, fixture.hash_kind(), oid, bytes)
                    .await
                    .unwrap();
                let read = service
                    .read_verified_raw(&stream, fixture.hash_kind(), oid, control)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(read.object_type(), verified.object_type());
                assert_eq!(read.bytes(), bytes);
                let mut corrupted = bytes.clone();
                corrupted.push(0);
                assert!(matches!(
                    verify_raw_object(fixture.hash_kind(), oid, corrupted, control).await,
                    Err(MegaError::Traces(TracesError::ObjectIntegrity))
                ));
                assert!(matches!(
                    verify_raw_object(
                        fixture.hash_kind(),
                        &"0".repeat(oid.len()),
                        bytes.clone(),
                        control
                    )
                    .await,
                    Err(MegaError::Traces(TracesError::ObjectIntegrity))
                ));
            }
        }
    }

    #[test]
    fn verified_raw_type_would_block_is_static_existing_error() {
        let error = raw_storage_error(
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "private-object-key").into(),
        );
        assert!(matches!(
            error,
            MegaError::Traces(TracesError::QueryTimeout)
        ));
        assert!(!error.to_string().contains("private-object-key"));
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
        bytes.extend_from_slice(
            b"gpgsig -----BEGIN PGP SIGNATURE-----\n x\n -----END PGP SIGNATURE-----\n\nmessage ",
        );
        bytes.push(0xff);
        bytes.push(b'\n');
        bytes
    }

    fn tree_payload() -> Vec<u8> {
        let mut bytes = b"100644 file".to_vec();
        bytes.push(0xff);
        bytes.push(0);
        bytes.extend_from_slice(&[0x22; 20]);
        bytes
    }

    fn blob_payload() -> Vec<u8> {
        let mut bytes = b"blob".to_vec();
        bytes.push(0xff);
        bytes
    }

    fn tag_payload() -> Vec<u8> {
        let mut bytes = b"object 341e54913a3a43069f2927cc0f703e5a9f730df1\ntype commit\ntag v1\nunknown-header kept\ntagger Synthetic <synthetic@example.test> 0 +0000\n\n".to_vec();
        bytes.push(0xff);
        bytes
    }

    #[tokio::test]
    async fn raw_roundtrip_preserves_commit_unknown_header_gpgsig_and_non_utf8() {
        let (_dir, db) = open_db().await;
        let service = TraceObjectService::mock();
        let commit = commit_payload();
        use git_internal::{
            hash::{HashKind, ObjectHash},
            internal::object::{ObjectTrait, commit::Commit},
        };
        let hash = ObjectHash::from_hex_for_kind(HashKind::Sha1, OID_SHA1).expect("oid");
        assert!(Commit::from_bytes(&commit, hash).is_err());
        service
            .put_raw(STREAM_A, "sha1", OID_SHA1, &commit)
            .await
            .expect("put");
        let stored = service
            .get_raw(STREAM_A, "sha1", OID_SHA1)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(stored, commit);
        assert!(
            stored
                .windows(b"unknown-header".len())
                .any(|w| w == b"unknown-header")
        );
        assert!(stored.windows(b"gpgsig".len()).any(|w| w == b"gpgsig"));
        assert!(stored.contains(&0xff));
        service
            .put_raw(STREAM_A, "sha1", OID_SHA1, &commit)
            .await
            .expect("replay");
        let err = service
            .put_raw(STREAM_A, "sha1", OID_SHA1, b"replaced")
            .await
            .expect_err("conflict");
        assert!(err.to_string().contains("object_integrity"), "{err}");
        assert_eq!(
            service
                .get_raw(STREAM_A, "sha1", OID_SHA1)
                .await
                .expect("kept"),
            Some(commit)
        );
        assert_no_membership(&db).await;
    }

    #[tokio::test]
    async fn raw_roundtrip_preserves_tree_blob_and_tag() {
        let (_dir, db) = open_db().await;
        let service = TraceObjectService::mock();
        service
            .put_raw(STREAM_A, "sha1", OID_TREE, &tree_payload())
            .await
            .expect("tree");
        service
            .put_raw(STREAM_A, "sha1", OID_BLOB, &blob_payload())
            .await
            .expect("blob");
        service
            .put_raw(STREAM_A, "sha256", OID_TAG, &tag_payload())
            .await
            .expect("tag");
        assert_eq!(
            service
                .get_raw(STREAM_A, "sha1", OID_TREE)
                .await
                .expect("tree read"),
            Some(tree_payload())
        );
        assert_eq!(
            service
                .get_raw(STREAM_A, "sha1", OID_BLOB)
                .await
                .expect("blob read"),
            Some(blob_payload())
        );
        assert_eq!(
            service
                .get_raw(STREAM_A, "sha256", OID_TAG)
                .await
                .expect("tag read"),
            Some(tag_payload())
        );
        assert_no_membership(&db).await;
    }

    #[tokio::test]
    async fn raw_roundtrip_delta_base_stays_in_stream() {
        let (_dir, db) = open_db().await;
        let store = mock_object_storage();
        let service = TraceObjectService::new(store.clone());
        let own = commit_payload();
        let other = b"other-stream-bytes".to_vec();
        let git_bytes = b"git-namespace-bytes".to_vec();
        let traces_key = object_key(STREAM_A, "sha1", OID_SHA1).expect("key");
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
                    key: traces_key.key,
                },
                b"agent-namespace-bytes".to_vec().into_stream(),
                ObjectMeta::default(),
            )
            .await
            .expect("agent plant");
        service
            .put_raw(STREAM_B, "sha1", OID_SHA1, &other)
            .await
            .expect("other");
        assert_eq!(
            service
                .read_delta_base(STREAM_A, "sha1", OID_SHA1)
                .await
                .expect("absent"),
            None
        );
        service
            .put_raw(STREAM_A, "sha1", OID_SHA1, &own)
            .await
            .expect("own");
        assert_eq!(
            service
                .read_delta_base(STREAM_A, "sha1", OID_SHA1)
                .await
                .expect("own base"),
            Some(own.clone())
        );
        assert_eq!(
            service
                .read_delta_base(STREAM_B, "sha1", OID_SHA1)
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
            service
                .read_delta_base(STREAM_A, "sha1", OID_BLOB)
                .await
                .expect("no git fallback"),
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
        assert_ne!(own.as_slice(), git_bytes.as_slice());
        assert_no_membership(&db).await;
    }
}
