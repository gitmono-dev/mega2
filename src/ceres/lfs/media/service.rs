//! Media prepare / chunk upload / resume (pre-finalize). Finalize is FC-06.

use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::{Bytes, BytesMut};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};

use crate::{
    ceres::lfs::{
        digest::LfsDigest,
        media::{
            chunker,
            protocol::{MAX_MANIFEST_SIZE, ManifestError, MediaManifest, PrepareResponse},
            scope::{MediaObjectKind, MediaScope, ScopeError, redact_storage_error},
        },
    },
    jupiter::{
        storage::object_storage::MegaObjectStorageWrapper, utils::into_obj_stream::IntoObjectStream,
    },
    orbit_api::{
        error::IoOrbitError,
        object_storage::{ObjectKey, ObjectMeta},
    },
};

pub const PENDING_TTL_SECS: u64 = 24 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("invalid media request: {0}")]
    Invalid(String),
    #[error("media object not found")]
    NotFound,
    #[error("media conflict: {0}")]
    Conflict(String),
    #[error("media object store error")]
    Storage,
    #[error("media I/O error")]
    Io(#[from] std::io::Error),
    #[error("media JSON error: {0}")]
    Json(String),
}

#[derive(Clone)]
pub struct MediaService {
    store: MegaObjectStorageWrapper,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingSession {
    created_at_unix: u64,
    manifest: MediaManifest,
}

impl MediaService {
    pub fn new(store: MegaObjectStorageWrapper) -> Self {
        Self { store }
    }

    pub async fn prepare(
        &self,
        scope: &MediaScope,
        manifest: MediaManifest,
    ) -> Result<PrepareResponse, MediaError> {
        self.prepare_at(scope, manifest, unix_now()).await
    }

    pub async fn upload_chunk(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
        body: Bytes,
    ) -> Result<(), MediaError> {
        self.upload_chunk_at(scope, manifest_id, chunk_hash, body, unix_now())
            .await
    }

    pub async fn get_chunk(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
    ) -> Result<Bytes, MediaError> {
        self.get_chunk_at(scope, manifest_id, chunk_hash, unix_now())
            .await
    }

    pub(crate) async fn prepare_at(
        &self,
        scope: &MediaScope,
        mut manifest: MediaManifest,
        now: u64,
    ) -> Result<PrepareResponse, MediaError> {
        manifest.validate().map_err(map_manifest)?;
        manifest.fallback_oid = Some(manifest.media_oid.clone());
        let manifest_id = manifest.id().map_err(map_manifest)?;
        let pending_key = scope_key(scope, MediaObjectKind::Pending, &manifest_id)?;
        let created_at_unix = match self.load_pending(&pending_key).await {
            Ok(existing) if !is_expired(existing.created_at_unix, now) => existing.created_at_unix,
            Ok(_) | Err(MediaError::NotFound) => now,
            Err(e) => return Err(e),
        };
        let session = PendingSession {
            created_at_unix,
            manifest: manifest.clone(),
        };
        let payload = encode_pending(&session)?;
        self.put_bytes(&pending_key, payload).await?;
        let missing_chunks = self.missing_chunks(scope, &manifest).await?;
        Ok(PrepareResponse {
            manifest_id,
            missing_chunks,
        })
    }

    pub(crate) async fn upload_chunk_at(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
        body: Bytes,
        now: u64,
    ) -> Result<(), MediaError> {
        let session = self.require_active_pending(scope, manifest_id, now).await?;
        let declared = session
            .manifest
            .chunks
            .iter()
            .find(|chunk| chunk.chunk_hash == chunk_hash)
            .ok_or_else(|| {
                MediaError::Invalid("chunk is not declared by the pending manifest".to_string())
            })?;
        if body.len() as u64 != declared.length {
            return Err(MediaError::Invalid(
                "chunk length does not match the pending manifest".to_string(),
            ));
        }
        if body.len() > chunker::MAX_SIZE {
            return Err(MediaError::Invalid("chunk exceeds 8 MiB".to_string()));
        }
        let actual = LfsDigest::sha256_of(&body).hex().to_owned();
        if actual != chunk_hash {
            return Err(MediaError::Invalid(
                "chunk SHA-256 does not match the declared hash".to_string(),
            ));
        }
        let key = scope_key(scope, MediaObjectKind::Chunk, chunk_hash)?;
        if self.exists(&key).await? {
            let existing = self.read_existing_chunk(&key).await?;
            if LfsDigest::sha256_of(&existing).hex() != chunk_hash {
                return Err(MediaError::Conflict(
                    "existing chunk content does not match the declared hash".to_string(),
                ));
            }
            return Ok(());
        }
        self.put_bytes(&key, body).await
    }

    pub(crate) async fn get_chunk_at(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        chunk_hash: &str,
        now: u64,
    ) -> Result<Bytes, MediaError> {
        let session = self.require_active_pending(scope, manifest_id, now).await?;
        if !session
            .manifest
            .chunks
            .iter()
            .any(|chunk| chunk.chunk_hash == chunk_hash)
        {
            return Err(MediaError::NotFound);
        }
        let key = scope_key(scope, MediaObjectKind::Chunk, chunk_hash)?;
        let bytes = self.read_existing_chunk(&key).await?;
        if LfsDigest::sha256_of(&bytes).hex() != chunk_hash {
            return Err(MediaError::Conflict(
                "stored chunk content does not match the declared hash".to_string(),
            ));
        }
        Ok(bytes)
    }

    async fn require_active_pending(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        now: u64,
    ) -> Result<PendingSession, MediaError> {
        let key = scope_key(scope, MediaObjectKind::Pending, manifest_id)?;
        let session = self.load_pending(&key).await?;
        if is_expired(session.created_at_unix, now) {
            return Err(MediaError::Invalid("media session expired".to_string()));
        }
        Ok(session)
    }

    async fn missing_chunks(
        &self,
        scope: &MediaScope,
        manifest: &MediaManifest,
    ) -> Result<Vec<String>, MediaError> {
        let mut missing = Vec::new();
        let mut seen = HashSet::new();
        for chunk in &manifest.chunks {
            if !seen.insert(chunk.chunk_hash.as_str()) {
                continue;
            }
            let key = scope_key(scope, MediaObjectKind::Chunk, &chunk.chunk_hash)?;
            if !self.exists(&key).await? {
                missing.push(chunk.chunk_hash.clone());
            }
        }
        Ok(missing)
    }

    async fn load_pending(&self, key: &ObjectKey) -> Result<PendingSession, MediaError> {
        let bytes = self.read_bytes(key, MAX_MANIFEST_SIZE).await?;
        serde_json::from_slice(&bytes).map_err(|e| MediaError::Json(e.to_string()))
    }

    async fn read_existing_chunk(&self, key: &ObjectKey) -> Result<Bytes, MediaError> {
        match self.read_bytes(key, chunker::MAX_SIZE).await {
            Ok(bytes) => Ok(bytes),
            Err(MediaError::Invalid(_)) => Err(MediaError::Conflict(
                "existing chunk content does not match the declared hash".to_string(),
            )),
            Err(e) => Err(e),
        }
    }

    async fn exists(&self, key: &ObjectKey) -> Result<bool, MediaError> {
        self.store.inner.exists(key).await.map_err(map_store)
    }

    async fn put_bytes(&self, key: &ObjectKey, bytes: Bytes) -> Result<(), MediaError> {
        let meta = ObjectMeta {
            size: bytes.len() as i64,
            ..ObjectMeta::default()
        };
        self.store
            .inner
            .put_stream_bounded(key, bytes.into_stream(), meta)
            .await
            .map_err(map_store)
    }

    async fn read_bytes(&self, key: &ObjectKey, max: usize) -> Result<Bytes, MediaError> {
        let (mut stream, meta) = self.store.inner.get_stream(key).await.map_err(map_store)?;
        if meta.size > max as i64 {
            return Err(MediaError::Invalid(
                "stored media object exceeds size limit".to_string(),
            ));
        }
        let mut buf = BytesMut::new();
        while let Some(chunk) = stream.try_next().await? {
            let projected = buf.len().saturating_add(chunk.len());
            if projected > max {
                return Err(MediaError::Invalid(
                    "stored media object exceeds size limit".to_string(),
                ));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf.freeze())
    }
}

#[cfg(test)]
impl MediaService {
    pub(crate) async fn overwrite_chunk_for_test(
        &self,
        scope: &MediaScope,
        chunk_hash: &str,
        body: Bytes,
    ) -> Result<(), MediaError> {
        let key = scope_key(scope, MediaObjectKind::Chunk, chunk_hash)?;
        self.put_bytes(&key, body).await
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_expired(created_at_unix: u64, now: u64) -> bool {
    now.saturating_sub(created_at_unix) >= PENDING_TTL_SECS
}

fn encode_pending(session: &PendingSession) -> Result<Bytes, MediaError> {
    let bytes = serde_json::to_vec(session).map_err(|e| MediaError::Json(e.to_string()))?;
    if bytes.len() > MAX_MANIFEST_SIZE {
        return Err(MediaError::Invalid(
            "pending manifest exceeds size limit".to_string(),
        ));
    }
    Ok(Bytes::from(bytes))
}

fn scope_key(
    scope: &MediaScope,
    kind: MediaObjectKind,
    object_id: &str,
) -> Result<ObjectKey, MediaError> {
    scope.object_key(kind, object_id).map_err(map_scope)
}

fn map_manifest(err: ManifestError) -> MediaError {
    match err {
        ManifestError::Invalid(msg) => MediaError::Invalid(msg),
        ManifestError::Serde(msg) => MediaError::Json(msg),
    }
}

fn map_scope(err: ScopeError) -> MediaError {
    match err {
        ScopeError::InvalidActor | ScopeError::InvalidRepository | ScopeError::InvalidObjectId => {
            MediaError::Invalid("invalid media scope or object id".to_string())
        }
    }
}

fn map_store(err: IoOrbitError) -> MediaError {
    let _ = redact_storage_error(&err);
    if err.is_not_found() {
        MediaError::NotFound
    } else {
        MediaError::Storage
    }
}
