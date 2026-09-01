use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use super::{
    chunker,
    protocol::{self, ChunkEntry, CreatedBy, MediaManifest},
    scope::MediaScope,
    service::{MediaServiceError, PENDING_TTL, PendingManifest},
};
use crate::{
    jupiter::{service::lfs_service::LfsService, utils::into_obj_stream::IntoObjectStream},
    orbit_api::object_storage::{ObjectKey, ObjectMeta},
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
