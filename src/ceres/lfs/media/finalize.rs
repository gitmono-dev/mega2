//! Finalize: reconstruct, verify FastCDC, publish standard LFS fallback, then
//! the finalized manifest. Missing or corrupt chunks publish neither object.

use std::{fs::File, io::Write, path::Path, sync::OnceLock};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio_util::io::ReaderStream;

use crate::{
    callisto::lfs_objects,
    ceres::lfs::{
        digest::LfsDigest,
        media::{
            chunker,
            protocol::{MAX_MANIFEST_SIZE, ManifestError, ManifestResponse, MediaManifest},
            scope::{MediaObjectKind, MediaScope, redact_storage_error},
            service::{MediaError, MediaService, map_store, scope_key, unix_now},
        },
    },
    jupiter::storage::lfs_db_storage::LfsDbStorage,
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};

const MAX_CONCURRENT_FINALIZE: usize = 2;

fn finalizer_semaphore() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| Semaphore::new(MAX_CONCURRENT_FINALIZE))
}

pub async fn finalize(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    scope: &MediaScope,
    manifest_id: &str,
) -> Result<ManifestResponse, MediaError> {
    finalize_at(media, lfs_db, scope, manifest_id, unix_now()).await
}

pub async fn finalize_at(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    scope: &MediaScope,
    manifest_id: &str,
    now: u64,
) -> Result<ManifestResponse, MediaError> {
    let _permit = finalizer_semaphore()
        .acquire()
        .await
        .map_err(|_| MediaError::Invalid("finalize semaphore closed".to_string()))?;

    let session = media
        .require_active_pending(scope, manifest_id, now)
        .await?;
    let manifest = session.manifest;
    let expected_id = manifest.id().map_err(map_manifest)?;
    if expected_id != manifest_id {
        return Err(MediaError::Conflict(
            "pending manifest id does not match canonical id".to_string(),
        ));
    }
    if manifest.fallback_oid.as_ref() != Some(&manifest.media_oid) {
        return Err(MediaError::Invalid(
            "fallback_oid must equal media_oid".to_string(),
        ));
    }

    let tmp = tempfile::NamedTempFile::new().map_err(MediaError::Io)?;
    let result = async {
        rebuild_and_verify(media, scope, manifest_id, &manifest, now, tmp.path()).await?;
        put_fallback_from_path(media, lfs_db, &manifest, tmp.path()).await?;
        publish_finalized(media, scope, manifest_id, &manifest).await
    }
    .await;
    let _ = tmp.close();
    result
}

fn map_manifest(err: ManifestError) -> MediaError {
    match err {
        ManifestError::Invalid(msg) => MediaError::Invalid(msg),
        ManifestError::Serde(msg) => MediaError::Json(msg),
    }
}

async fn rebuild_and_verify(
    media: &MediaService,
    scope: &MediaScope,
    manifest_id: &str,
    manifest: &MediaManifest,
    now: u64,
    path: &Path,
) -> Result<(), MediaError> {
    let mut file = File::create(path).map_err(MediaError::Io)?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    for chunk in &manifest.chunks {
        let bytes = media
            .get_chunk_at(scope, manifest_id, &chunk.chunk_hash, now)
            .await?;
        if bytes.len() as u64 != chunk.length
            || LfsDigest::sha256_of(&bytes).hex() != chunk.chunk_hash
        {
            return Err(MediaError::Invalid(
                "chunk hash or length mismatch during finalize".to_string(),
            ));
        }
        file.write_all(&bytes).map_err(MediaError::Io)?;
        hasher.update(&bytes);
        written += bytes.len() as u64;
    }
    file.flush().map_err(MediaError::Io)?;
    drop(file);

    let digest = hex::encode(hasher.finalize());
    if digest != manifest.media_oid || written != manifest.media_size {
        return Err(MediaError::Invalid(
            "reassembled media SHA-256 or size does not match the manifest".to_string(),
        ));
    }

    let recomputed =
        chunker::chunk_reader(File::open(path).map_err(MediaError::Io)?).map_err(MediaError::Io)?;
    if recomputed.len() != manifest.chunks.len() {
        return Err(MediaError::Invalid(
            "fastcdc-v1 boundaries do not match the pending manifest".to_string(),
        ));
    }
    for (got, want) in recomputed.iter().zip(manifest.chunks.iter()) {
        if got.offset != want.offset
            || got.length != want.length
            || got.chunk_hash != want.chunk_hash
        {
            return Err(MediaError::Invalid(
                "fastcdc-v1 offset/length/hash do not match the pending manifest".to_string(),
            ));
        }
    }
    Ok(())
}

async fn put_fallback_from_path(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    manifest: &MediaManifest,
    path: &Path,
) -> Result<(), MediaError> {
    let key = ObjectKey {
        namespace: ObjectNamespace::Lfs,
        key: manifest.media_oid.clone(),
    };
    let file = tokio::fs::File::open(path).await?;
    let stream: ObjectByteStream = Box::pin(ReaderStream::new(file));
    let meta = ObjectMeta {
        size: manifest.media_size as i64,
        ..ObjectMeta::default()
    };
    media
        .object_store()
        .inner
        .put_stream_bounded(&key, stream, meta)
        .await
        .map_err(map_store)?;

    lfs_db
        .new_lfs_object(lfs_objects::Model {
            oid: manifest.media_oid.clone(),
            size: manifest.media_size as i64,
            exist: true,
        })
        .await
        .map_err(map_db)?;

    let stored = lfs_db
        .get_lfs_object(&manifest.media_oid)
        .await
        .map_err(map_db)?
        .ok_or(MediaError::NotFound)?;
    if stored.oid != manifest.media_oid
        || stored.size != manifest.media_size as i64
        || !stored.exist
    {
        return Err(MediaError::Conflict(
            "lfs_objects metadata does not match the published fallback".to_string(),
        ));
    }
    if !media.exists(&key).await? {
        return Err(MediaError::NotFound);
    }
    Ok(())
}

async fn publish_finalized(
    media: &MediaService,
    scope: &MediaScope,
    manifest_id: &str,
    manifest: &MediaManifest,
) -> Result<ManifestResponse, MediaError> {
    let response = ManifestResponse {
        manifest_id: manifest_id.to_string(),
        manifest: manifest.clone(),
    };
    let key = scope_key(scope, MediaObjectKind::Finalized, &manifest.media_oid)?;
    if media.exists(&key).await? {
        let existing = media.read_bytes(&key, MAX_MANIFEST_SIZE).await?;
        let parsed: ManifestResponse =
            serde_json::from_slice(&existing).map_err(|e| MediaError::Json(e.to_string()))?;
        if parsed.manifest_id != manifest_id || parsed.manifest.media_oid != manifest.media_oid {
            return Err(MediaError::Conflict(
                "finalized manifest does not match media oid or scope session".to_string(),
            ));
        }
        return Ok(parsed);
    }
    let payload =
        Bytes::from(serde_json::to_vec(&response).map_err(|e| MediaError::Json(e.to_string()))?);
    if payload.len() > MAX_MANIFEST_SIZE {
        return Err(MediaError::Invalid(
            "finalized manifest exceeds size limit".to_string(),
        ));
    }
    media.put_bytes(&key, payload).await?;
    Ok(response)
}

fn map_db(err: crate::common::errors::MegaError) -> MediaError {
    let _ = redact_storage_error(&err);
    MediaError::Storage
}
