use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    ceres::lfs::media::{
        chunker,
        protocol::{self, MediaManifest, PrepareResponse, valid_hash},
        scope::MediaScope,
    },
    jupiter::{service::lfs_service::LfsService, utils::into_obj_stream::IntoObjectStream},
    orbit_api::{
        error::IoOrbitError,
        object_storage::{ObjectKey, ObjectMeta},
    },
};

pub(super) const PENDING_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Error)]
pub(crate) enum MediaServiceError {
    #[error("invalid media request")]
    Invalid,
    #[error("media object not found")]
    NotFound,
    #[error("media manifest conflicts with stored state")]
    Conflict,
    #[error("media storage failed")]
    Storage,
    #[error("media I/O failed")]
    Io,
    #[error("media JSON failed")]
    Json(#[source] serde_json::Error),
}

#[derive(Serialize, Deserialize)]
pub(super) struct PendingManifest {
    pub(super) created_at: u64,
    pub(super) manifest: MediaManifest,
}

fn now() -> Result<u64, MediaServiceError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| MediaServiceError::Storage)
}

fn storage_error(error: IoOrbitError) -> MediaServiceError {
    if error.is_not_found() {
        MediaServiceError::NotFound
    } else {
        MediaServiceError::Storage
    }
}

fn scope_error() -> MediaServiceError {
    MediaServiceError::Invalid
}

fn validate_duplicate_chunk_lengths(manifest: &MediaManifest) -> Result<(), MediaServiceError> {
    let mut lengths = HashMap::with_capacity(manifest.chunks.len());
    for chunk in &manifest.chunks {
        if let Some(length) = lengths.insert(chunk.chunk_hash.as_str(), chunk.length)
            && length != chunk.length
        {
            return Err(MediaServiceError::Invalid);
        }
    }
    Ok(())
}

async fn read_bounded(
    service: &LfsService,
    key: &ObjectKey,
    limit: usize,
) -> Result<Bytes, MediaServiceError> {
    if !service
        .obj_storage
        .inner
        .exists(key)
        .await
        .map_err(storage_error)?
    {
        return Err(MediaServiceError::NotFound);
    }

    let (mut stream, _) = service
        .obj_storage
        .inner
        .get_stream(key)
        .await
        .map_err(storage_error)?;
    let mut data = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| MediaServiceError::Io)?;
        if chunk.len() > limit.saturating_sub(data.len()) {
            return Err(MediaServiceError::Invalid);
        }
        data.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(data))
}

async fn put_bytes(
    service: &LfsService,
    key: &ObjectKey,
    data: Bytes,
) -> Result<(), MediaServiceError> {
    let size = data.len() as i64;
    service
        .obj_storage
        .inner
        .put_stream(
            key,
            data.into_stream(),
            ObjectMeta {
                size,
                ..Default::default()
            },
        )
        .await
        .map_err(storage_error)
}

pub(crate) async fn pending_manifest(
    service: &LfsService,
    scope: &MediaScope,
    manifest_id: &str,
) -> Result<MediaManifest, MediaServiceError> {
    if !valid_hash(manifest_id) {
        return Err(MediaServiceError::NotFound);
    }

    let key = scope
        .pending_manifest_key(manifest_id)
        .map_err(|_| scope_error())?;
    let data = match read_bounded(service, &key, protocol::MAX_MANIFEST_SIZE).await {
        Err(MediaServiceError::Invalid) => return Err(MediaServiceError::Conflict),
        result => result?,
    };
    let pending: PendingManifest =
        serde_json::from_slice(&data).map_err(MediaServiceError::Json)?;

    if now()?.saturating_sub(pending.created_at) >= PENDING_TTL.as_secs() {
        return Err(MediaServiceError::NotFound);
    }
    pending
        .manifest
        .validate()
        .map_err(|_| MediaServiceError::Conflict)?;
    validate_duplicate_chunk_lengths(&pending.manifest).map_err(|_| MediaServiceError::Conflict)?;
    if pending.manifest.fallback_oid.as_deref() != Some(&pending.manifest.media_oid) {
        return Err(MediaServiceError::Conflict);
    }
    if pending
        .manifest
        .id()
        .map_err(|_| MediaServiceError::Conflict)?
        != manifest_id
    {
        return Err(MediaServiceError::Conflict);
    }

    Ok(pending.manifest)
}

pub(crate) async fn prepare(
    service: &LfsService,
    scope: &MediaScope,
    mut manifest: MediaManifest,
) -> Result<PrepareResponse, MediaServiceError> {
    manifest
        .validate()
        .map_err(|_| MediaServiceError::Invalid)?;
    validate_duplicate_chunk_lengths(&manifest)?;
    manifest.fallback_oid = Some(manifest.media_oid.clone());
    let manifest_id = manifest.id().map_err(|_| MediaServiceError::Invalid)?;

    let mut missing_chunks = Vec::new();
    let mut seen = HashSet::with_capacity(manifest.chunks.len());
    for chunk in &manifest.chunks {
        if !seen.insert(chunk.chunk_hash.as_str()) {
            continue;
        }

        match read_chunk(service, scope, &chunk.chunk_hash, chunk.length).await {
            Ok(_) => {}
            Err(MediaServiceError::NotFound | MediaServiceError::Invalid) => {
                missing_chunks.push(chunk.chunk_hash.clone());
            }
            Err(error) => return Err(error),
        }
    }

    let data = serde_json::to_vec(&PendingManifest {
        created_at: now()?,
        manifest,
    })
    .map_err(MediaServiceError::Json)?;
    if data.len() > protocol::MAX_MANIFEST_SIZE {
        return Err(MediaServiceError::Invalid);
    }
    let key = scope
        .pending_manifest_key(&manifest_id)
        .map_err(|_| scope_error())?;
    put_bytes(service, &key, Bytes::from(data)).await?;

    Ok(PrepareResponse {
        manifest_id,
        missing_chunks,
    })
}

async fn read_chunk(
    service: &LfsService,
    scope: &MediaScope,
    hash: &str,
    length: u64,
) -> Result<Bytes, MediaServiceError> {
    let key = scope.chunk_key(hash).map_err(|_| scope_error())?;
    let data = read_bounded(service, &key, chunker::MAX_SIZE).await?;
    if data.len() as u64 != length || protocol::sha256_hex(&data) != hash {
        return Err(MediaServiceError::Invalid);
    }
    Ok(data)
}

pub(crate) async fn upload_chunk(
    service: &LfsService,
    scope: &MediaScope,
    manifest_id: &str,
    hash: &str,
    data: Bytes,
) -> Result<(), MediaServiceError> {
    let manifest = pending_manifest(service, scope, manifest_id).await?;
    let chunk = manifest
        .chunks
        .iter()
        .find(|chunk| chunk.chunk_hash == hash)
        .ok_or(MediaServiceError::NotFound)?;

    if data.len() as u64 != chunk.length || protocol::sha256_hex(&data) != hash {
        return Err(MediaServiceError::Invalid);
    }

    match read_chunk(service, scope, hash, chunk.length).await {
        Ok(_) => return Ok(()),
        Err(MediaServiceError::NotFound | MediaServiceError::Invalid) => {}
        Err(error) => return Err(error),
    }

    let key = scope.chunk_key(hash).map_err(|_| scope_error())?;
    put_bytes(service, &key, data).await
}

pub(crate) async fn read_pending_chunk(
    service: &LfsService,
    scope: &MediaScope,
    manifest_id: &str,
    hash: &str,
) -> Result<Bytes, MediaServiceError> {
    let manifest = pending_manifest(service, scope, manifest_id).await?;
    let chunk = manifest
        .chunks
        .iter()
        .find(|chunk| chunk.chunk_hash == hash)
        .ok_or(MediaServiceError::NotFound)?;

    read_chunk(service, scope, hash, chunk.length).await
}
