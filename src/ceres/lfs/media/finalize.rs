//! Finalize a validated Media upload into the standard LFS fallback object.

use std::{
    io::{Seek, SeekFrom},
    sync::LazyLock,
};

use bytes::Bytes;
use futures::stream;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::Semaphore,
};

use crate::{
    callisto::lfs_objects,
    ceres::lfs::media::{
        chunker,
        protocol::{self, ManifestResponse, MediaManifest, valid_hash},
        scope::MediaScope,
        service::{self, MediaServiceError},
    },
    jupiter::service::lfs_service::LfsService,
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};

const TEMP_READ_BUFFER_SIZE: usize = 64 * 1024;
static FINALIZERS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

fn fallback_key(media_oid: &str) -> ObjectKey {
    ObjectKey {
        namespace: ObjectNamespace::Lfs,
        key: media_oid.to_owned(),
    }
}

fn validate_finalized_manifest(
    manifest: &MediaManifest,
    media_oid: &str,
) -> Result<String, MediaServiceError> {
    if manifest.media_oid != media_oid {
        return Err(MediaServiceError::Conflict);
    }
    manifest
        .validate()
        .map_err(|_| MediaServiceError::Conflict)?;
    service::validate_duplicate_chunk_lengths(manifest).map_err(|_| MediaServiceError::Conflict)?;
    if manifest.fallback_oid.as_deref() != Some(&manifest.media_oid) {
        return Err(MediaServiceError::Conflict);
    }
    manifest.id().map_err(|_| MediaServiceError::Conflict)
}

pub(crate) async fn finalized_manifest(
    service: &LfsService,
    scope: &MediaScope,
    media_oid: &str,
) -> Result<ManifestResponse, MediaServiceError> {
    if !valid_hash(media_oid) {
        return Err(MediaServiceError::NotFound);
    }

    let key = scope
        .finalized_manifest_key(media_oid)
        .map_err(|_| service::scope_error())?;
    let data = match service::read_bounded(service, &key, protocol::MAX_MANIFEST_SIZE).await {
        Err(MediaServiceError::Invalid) => return Err(MediaServiceError::Conflict),
        result => result?,
    };
    let manifest = serde_json::from_slice(&data).map_err(MediaServiceError::Json)?;
    let manifest_id = validate_finalized_manifest(&manifest, media_oid)?;

    Ok(ManifestResponse {
        manifest_id,
        manifest,
    })
}

async fn reconstruct(
    service: &LfsService,
    scope: &MediaScope,
    manifest: &MediaManifest,
) -> Result<tokio::fs::File, MediaServiceError> {
    let temp = tempfile::tempfile().map_err(|_| MediaServiceError::Io)?;
    let mut file = tokio::fs::File::from_std(temp);
    let mut digest = Sha256::new();
    let mut media_size = 0u64;

    for chunk in &manifest.chunks {
        let data = service::read_chunk(service, scope, &chunk.chunk_hash, chunk.length).await?;
        media_size = media_size
            .checked_add(data.len() as u64)
            .ok_or(MediaServiceError::Invalid)?;
        digest.update(&data);
        file.write_all(&data)
            .await
            .map_err(|_| MediaServiceError::Io)?;
    }

    if media_size != manifest.media_size || hex::encode(digest.finalize()) != manifest.media_oid {
        return Err(MediaServiceError::Invalid);
    }
    file.flush().await.map_err(|_| MediaServiceError::Io)?;
    file.rewind().await.map_err(|_| MediaServiceError::Io)?;

    let mut file = file.into_std().await;
    let (mut file, chunks) = tokio::task::spawn_blocking(move || {
        let chunks = chunker::chunk_reader(&mut file)?;
        Ok::<_, std::io::Error>((file, chunks))
    })
    .await
    .map_err(|_| MediaServiceError::Io)?
    .map_err(|_| MediaServiceError::Io)?;

    if chunks.len() != manifest.chunks.len()
        || chunks
            .iter()
            .zip(&manifest.chunks)
            .any(|(actual, expected)| {
                actual.offset != expected.offset
                    || actual.length != expected.length
                    || actual.chunk_hash != expected.chunk_hash
            })
    {
        return Err(MediaServiceError::Invalid);
    }

    file.seek(SeekFrom::Start(0))
        .map_err(|_| MediaServiceError::Io)?;
    Ok(tokio::fs::File::from_std(file))
}

fn temporary_file_stream(file: tokio::fs::File) -> ObjectByteStream {
    Box::pin(stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0; TEMP_READ_BUFFER_SIZE];
        let length = file.read(&mut buffer).await?;
        if length == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(length);
        Ok(Some((Bytes::from(buffer), file)))
    }))
}

async fn publish_fallback(
    service: &LfsService,
    manifest: &MediaManifest,
    file: tokio::fs::File,
) -> Result<(), MediaServiceError> {
    let key = fallback_key(&manifest.media_oid);
    service
        .obj_storage
        .inner
        .put_stream_bounded(
            &key,
            temporary_file_stream(file),
            ObjectMeta {
                size: manifest.media_size as i64,
                ..Default::default()
            },
        )
        .await
        .map_err(|_| MediaServiceError::Storage)?;

    service
        .lfs_storage
        .new_lfs_object(lfs_objects::Model {
            oid: manifest.media_oid.clone(),
            size: manifest.media_size as i64,
            exist: true,
        })
        .await
        .map_err(|_| MediaServiceError::Storage)?;

    let metadata = service
        .lfs_storage
        .get_lfs_object(&manifest.media_oid)
        .await
        .map_err(|_| MediaServiceError::Storage)?
        .ok_or(MediaServiceError::Storage)?;
    if metadata.size != manifest.media_size as i64 || !metadata.exist {
        return Err(MediaServiceError::Conflict);
    }

    Ok(())
}

async fn publish_manifest(
    service: &LfsService,
    scope: &MediaScope,
    manifest: &MediaManifest,
) -> Result<(), MediaServiceError> {
    let payload = serde_json::to_vec(manifest).map_err(MediaServiceError::Json)?;
    if payload.len() > protocol::MAX_MANIFEST_SIZE {
        return Err(MediaServiceError::Conflict);
    }
    let key = scope
        .finalized_manifest_key(&manifest.media_oid)
        .map_err(|_| service::scope_error())?;
    service::put_bytes(service, &key, Bytes::from(payload)).await
}

/// Reconstructs, verifies and publishes an upload only after every integrity
/// check has succeeded. The standard LFS fallback is published before the
/// scoped finalized manifest, so the manifest never advertises a missing
/// fallback object.
pub(crate) async fn finalize(
    service: &LfsService,
    scope: &MediaScope,
    manifest_id: &str,
) -> Result<(), MediaServiceError> {
    let _permit = FINALIZERS
        .acquire()
        .await
        .map_err(|_| MediaServiceError::Storage)?;
    let manifest = service::pending_manifest(service, scope, manifest_id).await?;

    if let Some(metadata) = service
        .lfs_storage
        .get_lfs_object(&manifest.media_oid)
        .await
        .map_err(|_| MediaServiceError::Storage)?
        && metadata.size != manifest.media_size as i64
    {
        return Err(MediaServiceError::Conflict);
    }

    match finalized_manifest(service, scope, &manifest.media_oid).await {
        Ok(existing) if existing.manifest_id != manifest_id => {
            return Err(MediaServiceError::Conflict);
        }
        Ok(_) | Err(MediaServiceError::NotFound) => {}
        Err(error) => return Err(error),
    }

    let file = reconstruct(service, scope, &manifest).await?;
    publish_fallback(service, &manifest, file).await?;
    publish_manifest(service, scope, &manifest).await
}
