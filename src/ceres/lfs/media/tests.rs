use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt;

use super::{
    chunker,
    protocol::{self, ChunkEntry, CreatedBy, MediaManifest},
    scope::MediaScope,
    service::{MediaServiceError, PENDING_TTL, PendingManifest},
};
use crate::{
    jupiter::{
        service::lfs_service::LfsService, tests::test_storage,
        utils::into_obj_stream::IntoObjectStream,
    },
    orbit::factory::{
        LocalConfig, ObjectStorageBackend, ObjectStorageConfig, ObjectStorageFactory,
    },
    orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace},
};

fn fixture() -> (LfsService, MediaScope) {
    (
        LfsService::mock(),
        MediaScope::from_access_token_username("alice", "/project/demo.git").unwrap(),
    )
}

fn manifest_for_parts(parts: &[&[u8]]) -> MediaManifest {
    let media = parts.concat();
    let mut offset = 0;
    let chunks = parts
        .iter()
        .map(|part| {
            let length = part.len() as u64;
            let chunk = ChunkEntry {
                offset,
                length,
                chunk_hash: protocol::sha256_hex(part),
                encoded_length: length,
                compression: "none".into(),
                checksum: None,
            };
            offset += length;
            chunk
        })
        .collect();

    MediaManifest {
        version: 1,
        algorithm: chunker::ALGORITHM.into(),
        hash_algorithm: "sha256".into(),
        media_oid: protocol::sha256_hex(&media),
        media_size: media.len() as u64,
        chunks,
        created_by: CreatedBy {
            client: "monoengine".into(),
            version: "test".into(),
            capabilities: vec![chunker::ALGORITHM.into()],
        },
        fallback_oid: None,
    }
}

async fn put_raw(service: &LfsService, key: &ObjectKey, data: Bytes) {
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
        .unwrap();
}

async fn finalize_fixture() -> (tempfile::TempDir, LfsService, MediaScope) {
    let temp_dir = tempfile::tempdir().unwrap();
    let storage = test_storage(temp_dir.path()).await;
    let object_storage = ObjectStorageFactory::build(&ObjectStorageConfig {
        storage_type: ObjectStorageBackend::Local,
        local: LocalConfig {
            root_dir: temp_dir
                .path()
                .join("objects")
                .to_string_lossy()
                .into_owned(),
        },
        ..Default::default()
    })
    .await
    .unwrap();

    (
        temp_dir,
        LfsService {
            lfs_storage: storage.lfs_db_storage(),
            obj_storage: object_storage,
        },
        MediaScope::from_access_token_username("alice", "/project/demo.git").unwrap(),
    )
}

fn deterministic_bytes(length: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(length);
    let mut state = 0x1234_5678_9abc_def0u64;
    while data.len() < length {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.push((state >> 33) as u8);
    }
    data
}

fn fastcdc_manifest(media: &[u8]) -> MediaManifest {
    let chunks = chunker::chunk_bytes(media)
        .into_iter()
        .map(|chunk| ChunkEntry {
            offset: chunk.offset,
            length: chunk.length,
            chunk_hash: chunk.chunk_hash,
            encoded_length: chunk.length,
            compression: "none".into(),
            checksum: None,
        })
        .collect();

    MediaManifest {
        version: 1,
        algorithm: chunker::ALGORITHM.into(),
        hash_algorithm: "sha256".into(),
        media_oid: protocol::sha256_hex(media),
        media_size: media.len() as u64,
        chunks,
        created_by: CreatedBy {
            client: "monoengine".into(),
            version: "test".into(),
            capabilities: vec![chunker::ALGORITHM.into()],
        },
        fallback_oid: None,
    }
}

async fn upload_all(
    service: &LfsService,
    scope: &MediaScope,
    manifest: &MediaManifest,
    media: &[u8],
) -> String {
    let prepared = service
        .prepare_media(scope, manifest.clone())
        .await
        .unwrap();
    for hash in &prepared.missing_chunks {
        let chunk = manifest
            .chunks
            .iter()
            .find(|chunk| &chunk.chunk_hash == hash)
            .unwrap();
        let start = chunk.offset as usize;
        let end = (chunk.offset + chunk.length) as usize;
        service
            .upload_media_chunk(
                scope,
                &prepared.manifest_id,
                hash,
                Bytes::copy_from_slice(&media[start..end]),
            )
            .await
            .unwrap();
    }
    prepared.manifest_id
}

fn lfs_fallback_key(media_oid: &str) -> ObjectKey {
    ObjectKey {
        namespace: ObjectNamespace::Lfs,
        key: media_oid.to_owned(),
    }
}

async fn read_lfs_fallback(service: &LfsService, media_oid: &str) -> Vec<u8> {
    let stream =
        crate::ceres::lfs::handler::lfs_download_object(service.clone(), media_oid.to_owned())
            .await
            .unwrap();
    let mut stream = Box::pin(stream);
    let mut media = Vec::new();
    while let Some(chunk) = stream.next().await {
        media.extend_from_slice(&chunk.unwrap());
    }
    media
}

async fn assert_not_published(service: &LfsService, scope: &MediaScope, media_oid: &str) {
    assert!(
        service
            .lfs_storage
            .get_lfs_object(media_oid)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !service
            .obj_storage
            .inner
            .exists(&lfs_fallback_key(media_oid))
            .await
            .unwrap()
    );
    assert!(matches!(
        service.finalized_media_manifest(scope, media_oid).await,
        Err(MediaServiceError::NotFound)
    ));
}

#[tokio::test]
async fn prepare_and_upload() {
    let (service, scope) = fixture();
    let part = Bytes::from_static(b"declared media chunk");
    let manifest = manifest_for_parts(&[part.as_ref(), part.as_ref()]);
    let hash = manifest.chunks[0].chunk_hash.clone();

    let prepared = service
        .prepare_media(&scope, manifest.clone())
        .await
        .unwrap();
    assert_eq!(prepared.missing_chunks, vec![hash.clone()]);
    assert_eq!(
        service
            .pending_media_manifest(&scope, &prepared.manifest_id)
            .await
            .unwrap()
            .fallback_oid,
        Some(manifest.media_oid.clone())
    );
    assert!(matches!(
        service
            .read_pending_media_chunk(&scope, &prepared.manifest_id, &hash)
            .await,
        Err(MediaServiceError::NotFound)
    ));
    assert!(matches!(
        service
            .upload_media_chunk(
                &scope,
                &prepared.manifest_id,
                &hash,
                Bytes::from_static(b"incorrect media bytes"),
            )
            .await,
        Err(MediaServiceError::Invalid)
    ));

    let unclaimed = Bytes::from_static(b"unclaimed scoped chunk");
    let unclaimed_hash = protocol::sha256_hex(&unclaimed);
    put_raw(
        &service,
        &scope.chunk_key(&unclaimed_hash).unwrap(),
        unclaimed,
    )
    .await;
    assert!(matches!(
        service
            .read_pending_media_chunk(&scope, &prepared.manifest_id, &unclaimed_hash)
            .await,
        Err(MediaServiceError::NotFound)
    ));

    service
        .upload_media_chunk(&scope, &prepared.manifest_id, &hash, part.clone())
        .await
        .unwrap();
    service
        .upload_media_chunk(&scope, &prepared.manifest_id, &hash, part.clone())
        .await
        .unwrap();
    assert_eq!(
        service
            .read_pending_media_chunk(&scope, &prepared.manifest_id, &hash)
            .await
            .unwrap(),
        part
    );

    let other_scope = MediaScope::from_access_token_username("bob", "/project/demo.git").unwrap();
    assert!(matches!(
        service
            .read_pending_media_chunk(&other_scope, &prepared.manifest_id, &hash)
            .await,
        Err(MediaServiceError::NotFound)
    ));

    let mut invalid_fallback = manifest.clone();
    invalid_fallback.fallback_oid = Some("f".repeat(64));
    assert!(matches!(
        service.prepare_media(&scope, invalid_fallback).await,
        Err(MediaServiceError::Invalid)
    ));

    let mut inconsistent_duplicate = manifest.clone();
    inconsistent_duplicate.chunks[1].length += 1;
    inconsistent_duplicate.chunks[1].encoded_length += 1;
    inconsistent_duplicate.media_size += 1;
    assert!(matches!(
        service.prepare_media(&scope, inconsistent_duplicate).await,
        Err(MediaServiceError::Invalid)
    ));

    let mut oversized = manifest_for_parts(&[]);
    oversized.created_by.client = "x".repeat(protocol::MAX_MANIFEST_SIZE);
    assert!(matches!(
        service.prepare_media(&scope, oversized).await,
        Err(MediaServiceError::Invalid)
    ));
}

#[tokio::test]
async fn resume_and_deduplicate() {
    let (service, scope) = fixture();
    let part = Bytes::from_static(b"resume media chunk");
    let manifest = manifest_for_parts(&[part.as_ref(), part.as_ref()]);
    let hash = manifest.chunks[0].chunk_hash.clone();

    let prepared = service
        .prepare_media(&scope, manifest.clone())
        .await
        .unwrap();
    service
        .upload_media_chunk(&scope, &prepared.manifest_id, &hash, part)
        .await
        .unwrap();
    let resumed = service.prepare_media(&scope, manifest).await.unwrap();
    assert_eq!(resumed.manifest_id, prepared.manifest_id);
    assert!(resumed.missing_chunks.is_empty());

    let expired_part = b"expired media chunk";
    let mut expired_manifest = manifest_for_parts(&[expired_part]);
    expired_manifest.fallback_oid = Some(expired_manifest.media_oid.clone());
    let expired_id = expired_manifest.id().unwrap();
    let expired_payload = serde_json::to_vec(&PendingManifest {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(PENDING_TTL.as_secs()),
        manifest: expired_manifest.clone(),
    })
    .unwrap();
    put_raw(
        &service,
        &scope.pending_manifest_key(&expired_id).unwrap(),
        Bytes::from(expired_payload),
    )
    .await;
    assert!(matches!(
        service
            .upload_media_chunk(
                &scope,
                &expired_id,
                &expired_manifest.chunks[0].chunk_hash,
                Bytes::from_static(expired_part),
            )
            .await,
        Err(MediaServiceError::NotFound)
    ));

    let malformed_id = "d".repeat(64);
    put_raw(
        &service,
        &scope.pending_manifest_key(&malformed_id).unwrap(),
        Bytes::from_static(b"{"),
    )
    .await;
    assert!(matches!(
        service.pending_media_manifest(&scope, &malformed_id).await,
        Err(MediaServiceError::Json(_))
    ));

    let conflicting_id = "e".repeat(64);
    let mut conflicting_manifest = manifest_for_parts(&[b"conflict"]);
    conflicting_manifest.fallback_oid = Some(conflicting_manifest.media_oid.clone());
    let conflict_payload = serde_json::to_vec(&PendingManifest {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        manifest: conflicting_manifest,
    })
    .unwrap();
    put_raw(
        &service,
        &scope.pending_manifest_key(&conflicting_id).unwrap(),
        Bytes::from(conflict_payload),
    )
    .await;
    assert!(matches!(
        service
            .pending_media_manifest(&scope, &conflicting_id)
            .await,
        Err(MediaServiceError::Conflict)
    ));
}

#[test]
fn service_io_errors_do_not_retain_sources() {
    assert!(std::error::Error::source(&MediaServiceError::Io).is_none());
}

#[tokio::test]
async fn finalize_round_trip() {
    let (_temp_dir, service, scope) = finalize_fixture().await;
    let media = deterministic_bytes(chunker::MIN_SIZE + 128 * 1024);
    let manifest = fastcdc_manifest(&media);
    let manifest_id = upload_all(&service, &scope, &manifest, &media).await;

    service.finalize_media(&scope, &manifest_id).await.unwrap();
    service.finalize_media(&scope, &manifest_id).await.unwrap();

    let finalized = service
        .finalized_media_manifest(&scope, &manifest.media_oid)
        .await
        .unwrap();
    assert_eq!(finalized.manifest_id, manifest_id);
    assert_eq!(finalized.manifest, {
        let mut expected = manifest.clone();
        expected.fallback_oid = Some(expected.media_oid.clone());
        expected
    });
    let metadata = service
        .lfs_storage
        .get_lfs_object(&manifest.media_oid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(metadata.size, media.len() as i64);
    assert!(metadata.exist);
    assert_eq!(
        read_lfs_fallback(&service, &manifest.media_oid).await,
        media
    );

    let other_scope = MediaScope::from_access_token_username("bob", "/project/demo.git").unwrap();
    assert!(matches!(
        service
            .finalized_media_manifest(&other_scope, &manifest.media_oid)
            .await,
        Err(MediaServiceError::NotFound)
    ));
    assert!(matches!(
        service.finalize_media(&other_scope, &manifest_id).await,
        Err(MediaServiceError::NotFound)
    ));

    let noncanonical = manifest_for_parts(&[&media[..1], &media[1..]]);
    let noncanonical_id = upload_all(&service, &scope, &noncanonical, &media).await;
    assert_ne!(noncanonical_id, manifest_id);
    assert!(matches!(
        service.finalize_media(&scope, &noncanonical_id).await,
        Err(MediaServiceError::Conflict)
    ));

    let empty_manifest = fastcdc_manifest(&[]);
    let empty_id = upload_all(&service, &scope, &empty_manifest, &[]).await;
    service.finalize_media(&scope, &empty_id).await.unwrap();
    assert_eq!(
        read_lfs_fallback(&service, &empty_manifest.media_oid).await,
        Vec::<u8>::new()
    );
}

#[tokio::test]
async fn rejects_corruption_without_publication() {
    let (_temp_dir, service, scope) = finalize_fixture().await;
    let media = deterministic_bytes(chunker::MIN_SIZE + 128 * 1024);
    let manifest = fastcdc_manifest(&media);
    let manifest_id = upload_all(&service, &scope, &manifest, &media).await;
    let corrupted_chunk = &manifest.chunks[0];
    put_raw(
        &service,
        &scope.chunk_key(&corrupted_chunk.chunk_hash).unwrap(),
        Bytes::from_static(b"corrupt"),
    )
    .await;

    assert!(matches!(
        service.finalize_media(&scope, &manifest_id).await,
        Err(MediaServiceError::Invalid)
    ));
    assert_not_published(&service, &scope, &manifest.media_oid).await;

    let noncanonical_media = deterministic_bytes(chunker::MIN_SIZE + 1);
    let noncanonical = manifest_for_parts(&[&noncanonical_media[..1], &noncanonical_media[1..]]);
    let noncanonical_id = upload_all(&service, &scope, &noncanonical, &noncanonical_media).await;
    assert!(matches!(
        service.finalize_media(&scope, &noncanonical_id).await,
        Err(MediaServiceError::Invalid)
    ));
    assert_not_published(&service, &scope, &noncanonical.media_oid).await;
}

#[tokio::test]
async fn rejects_mismatched_finalized_manifest() {
    let (_temp_dir, service, scope) = finalize_fixture().await;
    let media = deterministic_bytes(chunker::MIN_SIZE + 1);
    let manifest = fastcdc_manifest(&media);
    let mut mismatched = manifest.clone();
    mismatched.media_oid = "a".repeat(64);
    mismatched.fallback_oid = Some(mismatched.media_oid.clone());
    put_raw(
        &service,
        &scope.finalized_manifest_key(&manifest.media_oid).unwrap(),
        Bytes::from(serde_json::to_vec(&mismatched).unwrap()),
    )
    .await;

    assert!(matches!(
        service
            .finalized_media_manifest(&scope, &manifest.media_oid)
            .await,
        Err(MediaServiceError::Conflict)
    ));
}
