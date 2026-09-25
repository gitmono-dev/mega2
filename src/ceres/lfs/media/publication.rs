//! Multi-layout atomic publication (MF-07 / ADR-MF-05 / C-04).
//!
//! Order after coverage verification:
//! 1. LFS fallback object + `lfs_objects` (same-oid idempotent)
//! 2. Immutable identity record at `(scope, Finalized, manifest_id)`
//! 3. Durable session `finalized` state
//! 4. Replaceable by-media discovery at `(scope, Manifest, media_oid)`
//! 5. WH-06 event only after the immutable record is readable
//!
//! Metadata writes use [`MegaObjectStorage::put_metadata_atomic`] (≤1 MiB
//! complete-object PUT). Partial / edge-visible bodies are never published.

use bytes::Bytes;

use crate::{
    callisto::lfs_objects,
    ceres::lfs::media::{
        protocol::{MAX_ENVELOPE_SIZE, ManifestResponse, MediaManifest},
        scope::{MediaObjectKind, MediaScope, redact_storage_error},
        service::{MediaError, MediaService, map_store, scope_key},
    },
    jupiter::storage::{
        lfs_db_storage::LfsDbStorage,
        media_paging_storage::{MediaPagingError, STATE_FINALIZED},
    },
    orbit_api::object_storage::{ObjectKey, ObjectMeta, ObjectNamespace},
};

/// Publish a verified layout. Caller has already reassembled and verified bytes.
pub async fn publish_verified_layout(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    scope: &MediaScope,
    manifest_id: &str,
    manifest: &MediaManifest,
    fallback_path: &std::path::Path,
    now: u64,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
) -> Result<ManifestResponse, MediaError> {
    put_fallback_idempotent(media, lfs_db, manifest, fallback_path).await?;
    publish_finalized_records(media, scope, manifest_id, manifest, now, emitter).await
}

/// Immutable + by-media + session state + optional event (no fallback rewrite).
pub async fn publish_finalized_records(
    media: &MediaService,
    scope: &MediaScope,
    manifest_id: &str,
    manifest: &MediaManifest,
    now: u64,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
) -> Result<ManifestResponse, MediaError> {
    let response = ManifestResponse {
        manifest_id: manifest_id.to_string(),
        manifest: manifest.clone(),
    };
    let payload =
        Bytes::from(serde_json::to_vec(&response).map_err(|e| MediaError::Json(e.to_string()))?);
    if payload.len() > MAX_ENVELOPE_SIZE {
        return Err(MediaError::Invalid(
            "finalized manifest exceeds size limit".to_string(),
        ));
    }

    let immutable_key = scope_key(scope, MediaObjectKind::Finalized, manifest_id)?;
    let (identity, wrote_new) =
        put_immutable_identity(media, &immutable_key, &response, payload).await?;

    media
        .paging()
        .mark_session_finalized(&scope.digest(), manifest_id)
        .await
        .map_err(map_paging)?;

    // by-media: any complete published layout may win (no winner CAS).
    put_by_media(media, scope, &identity).await?;

    // WH-06: emit only when this call created the immutable record and it is
    // readable (session finalized + by-media written).
    if wrote_new {
        let event = crate::jupiter::service::storage_event::CommittedEvent {
            event_id: uuid::Uuid::new_v4(),
            event_type: crate::jupiter::service::storage_event::EventType::LfsMediaFinalized,
            occurred_at: now,
            source: crate::jupiter::service::storage_event::EventSource::Lfs,
            scope: crate::jupiter::service::storage_event::EventScope::Media {
                repo_path: scope.repository().to_owned(),
            },
            data: crate::jupiter::service::storage_event::EventData::LfsMediaFinalized {
                oid: identity.manifest.media_oid.clone(),
                size: identity.manifest.media_size,
                manifest_id: identity.manifest_id.clone(),
            },
        };
        let _ = emitter.try_emit(event);
    }

    Ok(identity)
}

/// Load immutable identity by `manifest_id` (HTTP routes in MF-04).
pub async fn load_immutable_by_id(
    media: &MediaService,
    scope: &MediaScope,
    manifest_id: &str,
) -> Result<ManifestResponse, MediaError> {
    let session = media
        .paging()
        .get_session(&scope.digest(), manifest_id)
        .await
        .map_err(map_paging)?;
    if session.state != STATE_FINALIZED {
        return Err(MediaError::NotFound);
    }
    let key = scope_key(scope, MediaObjectKind::Finalized, manifest_id)?;
    let bytes = media.read_bytes(&key, MAX_ENVELOPE_SIZE).await?;
    let parsed: ManifestResponse =
        serde_json::from_slice(&bytes).map_err(|e| MediaError::Json(e.to_string()))?;
    if parsed.manifest_id != manifest_id {
        return Err(MediaError::Conflict(
            "immutable record manifest_id mismatch".into(),
        ));
    }
    let expected = parsed.manifest.id().map_err(|e| match e {
        crate::ceres::lfs::media::protocol::ManifestError::Invalid(m) => MediaError::Invalid(m),
        crate::ceres::lfs::media::protocol::ManifestError::Serde(m) => MediaError::Json(m),
    })?;
    if expected != manifest_id {
        return Err(MediaError::Conflict(
            "immutable record fails canonical identity check".into(),
        ));
    }
    Ok(parsed)
}

/// Load replaceable by-media discovery (full ManifestResponse).
pub async fn load_by_media(
    media: &MediaService,
    scope: &MediaScope,
    oid: &str,
) -> Result<ManifestResponse, MediaError> {
    let key = scope_key(scope, MediaObjectKind::Manifest, oid)?;
    let bytes = media.read_bytes(&key, MAX_ENVELOPE_SIZE).await?;
    let parsed: ManifestResponse =
        serde_json::from_slice(&bytes).map_err(|e| MediaError::Json(e.to_string()))?;
    // Only expose layouts whose durable session is finalized.
    let session = media
        .paging()
        .get_session(&scope.digest(), &parsed.manifest_id)
        .await
        .map_err(map_paging)?;
    if session.state != STATE_FINALIZED {
        return Err(MediaError::NotFound);
    }
    if parsed.manifest.media_oid != oid {
        return Err(MediaError::Conflict("by-media record oid mismatch".into()));
    }
    Ok(parsed)
}

async fn put_immutable_identity(
    media: &MediaService,
    key: &ObjectKey,
    response: &ManifestResponse,
    payload: Bytes,
) -> Result<(ManifestResponse, bool), MediaError> {
    if media.exists(key).await? {
        let existing = media.read_bytes(key, MAX_ENVELOPE_SIZE).await?;
        let parsed: ManifestResponse =
            serde_json::from_slice(&existing).map_err(|e| MediaError::Json(e.to_string()))?;
        if !canonical_identity_equiv(&parsed, response) {
            return Err(MediaError::Conflict(
                "finalized manifest does not match canonical identity for this id".to_string(),
            ));
        }
        return Ok((parsed, false));
    }
    media.put_metadata(key, payload).await?;
    // Re-read so concurrent first-writers converge on one complete record.
    let stored = media.read_bytes(key, MAX_ENVELOPE_SIZE).await?;
    let parsed: ManifestResponse =
        serde_json::from_slice(&stored).map_err(|e| MediaError::Json(e.to_string()))?;
    if !canonical_identity_equiv(&parsed, response) {
        return Err(MediaError::Conflict(
            "finalized manifest does not match canonical identity for this id".to_string(),
        ));
    }
    Ok((parsed, true))
}

async fn put_by_media(
    media: &MediaService,
    scope: &MediaScope,
    identity: &ManifestResponse,
) -> Result<(), MediaError> {
    let key = scope_key(
        scope,
        MediaObjectKind::Manifest,
        &identity.manifest.media_oid,
    )?;
    let payload =
        Bytes::from(serde_json::to_vec(identity).map_err(|e| MediaError::Json(e.to_string()))?);
    if payload.len() > MAX_ENVELOPE_SIZE {
        return Err(MediaError::Invalid(
            "by-media manifest exceeds size limit".to_string(),
        ));
    }
    media.put_metadata(&key, payload).await
}

fn canonical_identity_equiv(a: &ManifestResponse, b: &ManifestResponse) -> bool {
    if a.manifest_id != b.manifest_id {
        return false;
    }
    // created_by / fallback_oid are not part of canonical identity.
    a.manifest.version == b.manifest.version
        && a.manifest.algorithm == b.manifest.algorithm
        && a.manifest.hash_algorithm == b.manifest.hash_algorithm
        && a.manifest.media_oid == b.manifest.media_oid
        && a.manifest.media_size == b.manifest.media_size
        && a.manifest.chunks == b.manifest.chunks
}

pub(crate) async fn put_fallback_idempotent(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    manifest: &MediaManifest,
    path: &std::path::Path,
) -> Result<(), MediaError> {
    let key = ObjectKey {
        namespace: ObjectNamespace::Lfs,
        key: manifest.media_oid.clone(),
    };
    if media.exists(&key).await? {
        // Same oid may already be published by a concurrent layout; accept.
    } else {
        let file = tokio::fs::File::open(path).await?;
        let stream: crate::orbit_api::object_storage::ObjectByteStream =
            Box::pin(tokio_util::io::ReaderStream::new(file));
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
    }

    lfs_db
        .ensure_lfs_object(lfs_objects::Model {
            oid: manifest.media_oid.clone(),
            size: manifest.media_size as i64,
            exist: true,
        })
        .await
        .map_err(map_db)?;

    if !media.exists(&key).await? {
        return Err(MediaError::NotFound);
    }
    Ok(())
}

fn map_paging(err: MediaPagingError) -> MediaError {
    match err {
        MediaPagingError::NotFound => MediaError::NotFound,
        MediaPagingError::Conflict(msg) => MediaError::Conflict(msg),
        MediaPagingError::Storage(_) => MediaError::Storage,
        MediaPagingError::StaleLease => MediaError::Conflict("stale media lease".into()),
    }
}

fn map_db(err: crate::common::errors::MegaError) -> MediaError {
    let msg = err.to_string();
    if msg.contains("does not match") {
        return MediaError::Conflict(
            "lfs_objects metadata does not match the published fallback".to_string(),
        );
    }
    let _ = redact_storage_error(&err);
    MediaError::Storage
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use bytes::Bytes;

    use super::*;
    use crate::{
        ceres::lfs::{
            digest::LfsDigest,
            media::{
                chunker, finalize,
                protocol::{ChunkEntry, CreatedBy},
                service::MediaService,
            },
        },
        jupiter::{
            service::storage_event_emitter::StorageEventEmitter,
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                lfs_db_storage::LfsDbStorage,
                object_storage::build_object_storage,
            },
            tests::test_db_connection,
        },
        orbit_api::{
            error::{IoOrbitError, OrbitResult},
            factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig},
            log_storage::{LogManifest, LogStorage},
            object_storage::{MegaObjectStorage, ObjectByteStream, ObjectKey, ObjectMeta},
        },
    };

    struct Fixture {
        _obj_dir: tempfile::TempDir,
        _db_dir: tempfile::TempDir,
        service: MediaService,
        lfs_db: LfsDbStorage,
        scope: MediaScope,
    }

    async fn fixture_with_store(
        store: crate::orbit_api::factory::MegaObjectStorageWrapper,
    ) -> Fixture {
        let db_dir = tempfile::tempdir().unwrap();
        let db = test_db_connection(db_dir.path()).await;
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .unwrap();
        let lfs_db = LfsDbStorage {
            base: BaseStorage::new(Arc::new(db)),
        };
        let paging = crate::jupiter::storage::media_paging_storage::MediaPagingStorage::new(
            lfs_db.base.clone(),
        );
        Fixture {
            _obj_dir: tempfile::tempdir().unwrap(),
            _db_dir: db_dir,
            service: MediaService::new(store, paging),
            lfs_db,
            scope: MediaScope::from_server("user-1", "/acme/app").unwrap(),
        }
    }

    async fn local_fixture() -> Fixture {
        let obj_dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: obj_dir.path().to_string_lossy().into_owned(),
            },
            ..Default::default()
        };
        let store = build_object_storage(&cfg).await.unwrap();
        let mut fx = fixture_with_store(store).await;
        fx._obj_dir = obj_dir;
        fx
    }

    fn layout_a(data: &[u8]) -> (MediaManifest, Vec<(String, Bytes)>) {
        let chunks = chunker::chunk_bytes(data)
            .into_iter()
            .map(|c| ChunkEntry {
                offset: c.offset,
                length: c.length,
                chunk_hash: c.chunk_hash,
                encoded_length: c.length,
                compression: "none".to_string(),
                checksum: None,
            })
            .collect::<Vec<_>>();
        let bodies = chunks
            .iter()
            .map(|c| {
                let start = c.offset as usize;
                let end = start + c.length as usize;
                (
                    c.chunk_hash.clone(),
                    Bytes::copy_from_slice(&data[start..end]),
                )
            })
            .collect();
        let manifest = MediaManifest {
            version: 1,
            algorithm: chunker::ALGORITHM.to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid: LfsDigest::sha256_of(data).hex().to_owned(),
            media_size: data.len() as u64,
            chunks,
            created_by: CreatedBy {
                client: "test-a".to_string(),
                version: "0".to_string(),
                capabilities: vec![chunker::ALGORITHM.to_string()],
            },
            fallback_oid: None,
        };
        (manifest, bodies)
    }

    async fn prepare_layout(
        fx: &Fixture,
        manifest: &MediaManifest,
        bodies: &[(String, Bytes)],
        now: u64,
    ) -> String {
        let prepared = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), now)
            .await
            .unwrap();
        for (hash, body) in bodies {
            fx.service
                .upload_chunk_at(&fx.scope, &prepared.manifest_id, hash, body.clone(), now)
                .await
                .unwrap();
        }
        finalize::put_pages_and_seal(&fx.service, &fx.scope, manifest, &prepared.manifest_id, now)
            .await
            .unwrap();
        prepared.manifest_id
    }

    #[tokio::test]
    async fn concurrent_layouts_same_oid_both_succeed() {
        let fx = local_fixture().await;
        let data: Vec<u8> = (0..chunker::MIN_SIZE + 128)
            .map(|i| (i % 251) as u8)
            .collect();
        let (m1, b1) = two_legal_layouts_same_oid(&data);
        let (m2, b2) = two_legal_layouts_same_oid_alt(&data);
        assert_eq!(m1.media_oid, m2.media_oid);
        assert_ne!(m1.id().unwrap(), m2.id().unwrap());

        let id1 = prepare_layout(&fx, &m1, &b1, 10).await;
        let id2 = prepare_layout(&fx, &m2, &b2, 10).await;
        assert_ne!(id1, id2);

        let media = fx.service.clone();
        let lfs = fx.lfs_db.clone();
        let scope = fx.scope.clone();
        let mid1 = id1.clone();
        let h1 = tokio::spawn(async move {
            finalize::finalize_at(
                &media,
                &lfs,
                &scope,
                &mid1,
                11,
                &StorageEventEmitter::disabled(),
            )
            .await
        });
        let media = fx.service.clone();
        let lfs = fx.lfs_db.clone();
        let scope = fx.scope.clone();
        let mid2 = id2.clone();
        let h2 = tokio::spawn(async move {
            finalize::finalize_at(
                &media,
                &lfs,
                &scope,
                &mid2,
                12,
                &StorageEventEmitter::disabled(),
            )
            .await
        });
        let r1 = h1.await.unwrap().expect("layout1");
        let r2 = h2.await.unwrap().expect("layout2");
        assert_eq!(r1.manifest_id, id1);
        assert_eq!(r2.manifest_id, id2);
        assert_eq!(r1.manifest.media_oid, r2.manifest.media_oid);

        let imm1 = load_immutable_by_id(&fx.service, &fx.scope, &id1)
            .await
            .unwrap();
        let imm2 = load_immutable_by_id(&fx.service, &fx.scope, &id2)
            .await
            .unwrap();
        assert_eq!(imm1.manifest_id, id1);
        assert_eq!(imm2.manifest_id, id2);

        let by = load_by_media(&fx.service, &fx.scope, &m1.media_oid)
            .await
            .unwrap();
        assert!(by.manifest_id == id1 || by.manifest_id == id2);
    }

    fn two_legal_layouts_same_oid(data: &[u8]) -> (MediaManifest, Vec<(String, Bytes)>) {
        layout_a(data)
    }

    fn two_legal_layouts_same_oid_alt(data: &[u8]) -> (MediaManifest, Vec<(String, Bytes)>) {
        // Manual split: first chunk MIN_SIZE (or remaining), rest as tail — if CDC
        // already produced that shape, shift the cut by 1 within MIN_SIZE..len.
        use bytes::BytesMut;
        let cdc = chunker::chunk_bytes(data);
        let cut = if cdc.len() >= 2 {
            let c0 = cdc[0].length as usize;
            if c0 > chunker::MIN_SIZE {
                c0 - 1
            } else if data.len() > chunker::MIN_SIZE + 1 {
                chunker::MIN_SIZE + 1
            } else {
                chunker::MIN_SIZE.min(data.len().saturating_sub(1)).max(1)
            }
        } else if data.len() > chunker::MIN_SIZE {
            chunker::MIN_SIZE
        } else {
            data.len().saturating_sub(1).max(1)
        };
        let cut = cut.min(data.len().saturating_sub(1)).max(1);
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut bodies = Vec::new();
        for (i, part) in [&data[..cut], &data[cut..]].into_iter().enumerate() {
            let is_tail = i == 1;
            let declared = if is_tail {
                part.len()
            } else {
                part.len().max(chunker::MIN_SIZE)
            };
            let body = BytesMut::from(part);
            // Do not pad — keep exact bytes so oid matches layout_a.
            assert_eq!(
                body.len(),
                declared,
                "alt layout must not pad when matching oid"
            );
            let hash = LfsDigest::sha256_of(&body).hex().to_owned();
            chunks.push(ChunkEntry {
                offset,
                length: declared as u64,
                chunk_hash: hash.clone(),
                encoded_length: declared as u64,
                compression: "none".to_string(),
                checksum: None,
            });
            bodies.push((hash, body.freeze()));
            offset += declared as u64;
        }
        let media_oid = LfsDigest::sha256_of(data).hex().to_owned();
        assert_eq!(offset, data.len() as u64);
        let manifest = MediaManifest {
            version: 1,
            algorithm: chunker::ALGORITHM.to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid,
            media_size: data.len() as u64,
            chunks,
            created_by: CreatedBy {
                client: "test-alt".to_string(),
                version: "0".to_string(),
                capabilities: vec![chunker::ALGORITHM.to_string()],
            },
            fallback_oid: None,
        };
        (manifest, bodies)
    }

    #[tokio::test]
    async fn same_id_retry_returns_equivalent_identity() {
        let fx = local_fixture().await;
        let data = b"same-id-retry-object";
        let (manifest, bodies) = layout_a(data);
        let id = prepare_layout(&fx, &manifest, &bodies, 20).await;
        let first = finalize::finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &id,
            21,
            &StorageEventEmitter::disabled(),
        )
        .await
        .unwrap();
        let again = finalize::finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &id,
            22,
            &StorageEventEmitter::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(first.manifest_id, again.manifest_id);
        assert_eq!(first.manifest.media_oid, again.manifest.media_oid);
        assert_eq!(first.manifest.chunks, again.manifest.chunks);
        let imm = load_immutable_by_id(&fx.service, &fx.scope, &id)
            .await
            .unwrap();
        assert_eq!(imm.manifest_id, id);
    }

    /// Faulty store: fail N metadata puts then succeed (C-04 retry windows).
    struct FaultyMetaStore {
        inner: Arc<dyn crate::orbit_api::factory::MegaObjectStorageWithLog>,
        fail_remaining: AtomicUsize,
        meta_puts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl MegaObjectStorage for FaultyMetaStore {
        async fn put_stream(
            &self,
            key: &ObjectKey,
            data: ObjectByteStream,
            meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.inner.put_stream(key, data, meta).await
        }

        async fn put_stream_bounded(
            &self,
            key: &ObjectKey,
            data: ObjectByteStream,
            meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.inner.put_stream_bounded(key, data, meta).await
        }

        async fn put_metadata_atomic(
            &self,
            key: &ObjectKey,
            bytes: Bytes,
            meta: ObjectMeta,
        ) -> OrbitResult<()> {
            let is_publish = key.key.contains("/finalized/") || key.key.contains("/manifest/");
            if is_publish {
                self.meta_puts.fetch_add(1, Ordering::SeqCst);
                if self.fail_remaining.load(Ordering::SeqCst) > 0 {
                    self.fail_remaining.fetch_sub(1, Ordering::SeqCst);
                    return Err(IoOrbitError::Other("injected metadata put failure".into()));
                }
            }
            self.inner.put_metadata_atomic(key, bytes, meta).await
        }

        async fn get_stream(&self, key: &ObjectKey) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
            self.inner.get_stream(key).await
        }

        async fn get_range_stream(
            &self,
            key: &ObjectKey,
            start: u64,
            end: Option<u64>,
        ) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
            self.inner.get_range_stream(key, start, end).await
        }

        async fn exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
            self.inner.exists(key).await
        }

        async fn signed_url(
            &self,
            key: &ObjectKey,
            method: reqwest::Method,
            expires_in: std::time::Duration,
        ) -> OrbitResult<Option<String>> {
            self.inner.signed_url(key, method, expires_in).await
        }

        async fn delete(&self, key: &ObjectKey) -> OrbitResult<()> {
            self.inner.delete(key).await
        }
    }

    #[async_trait::async_trait]
    impl LogStorage for FaultyMetaStore {
        async fn append(
            &self,
            key: &ObjectKey,
            data: ObjectByteStream,
            meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.inner.append(key, data, meta).await
        }

        async fn read_range(
            &self,
            key: &ObjectKey,
            offset: u64,
            length: u64,
        ) -> OrbitResult<ObjectByteStream> {
            self.inner.read_range(key, offset, length).await
        }

        async fn read_lines_range(
            &self,
            key: &ObjectKey,
            start_line: u64,
            end_line: u64,
        ) -> OrbitResult<ObjectByteStream> {
            self.inner.read_lines_range(key, start_line, end_line).await
        }

        async fn append_concurrently(
            &self,
            key: &ObjectKey,
            data: ObjectByteStream,
            meta: ObjectMeta,
        ) -> OrbitResult<()> {
            self.inner.append_concurrently(key, data, meta).await
        }

        async fn load_manifest(&self, key: &ObjectKey) -> OrbitResult<LogManifest> {
            self.inner.load_manifest(key).await
        }

        async fn log_exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
            self.inner.log_exists(key).await
        }
    }

    #[tokio::test]
    async fn publication_fault_windows_retry_converge() {
        let obj_dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: obj_dir.path().to_string_lossy().into_owned(),
            },
            ..Default::default()
        };
        let inner = build_object_storage(&cfg).await.unwrap();
        let faulty = Arc::new(FaultyMetaStore {
            inner: inner.inner.clone(),
            fail_remaining: AtomicUsize::new(2),
            meta_puts: AtomicUsize::new(0),
        });
        let store = crate::orbit_api::factory::MegaObjectStorageWrapper::new(faulty.clone());
        let fx = fixture_with_store(store).await;

        let data = b"fault-window-object";
        let (manifest, bodies) = layout_a(data);
        let id = prepare_layout(&fx, &manifest, &bodies, 30).await;

        // First attempts fail at metadata put; retry until convergent success.
        let mut last_err = None;
        let mut published = None;
        for attempt in 0..6 {
            match finalize::finalize_at(
                &fx.service,
                &fx.lfs_db,
                &fx.scope,
                &id,
                31 + attempt,
                &StorageEventEmitter::disabled(),
            )
            .await
            {
                Ok(r) => {
                    published = Some(r);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        assert!(
            published.is_some(),
            "must converge after injected faults; last={last_err:?} puts={}",
            faulty.meta_puts.load(Ordering::SeqCst)
        );
        let by = load_by_media(&fx.service, &fx.scope, &manifest.media_oid)
            .await
            .unwrap();
        assert_eq!(by.manifest_id, id);
        // Never expose a half-written immutable key: if present, must parse.
        let key = scope_key(&fx.scope, MediaObjectKind::Finalized, &id).unwrap();
        let bytes = fx
            .service
            .read_bytes(&key, MAX_ENVELOPE_SIZE)
            .await
            .unwrap();
        let _: ManifestResponse = serde_json::from_slice(&bytes).unwrap();
    }

    #[tokio::test]
    async fn by_media_hides_non_finalized() {
        let fx = local_fixture().await;
        let data = b"hide-pending";
        let (manifest, bodies) = layout_a(data);
        let id = prepare_layout(&fx, &manifest, &bodies, 40).await;
        // Manually write a by-media pointer before session is finalized.
        let spoof = ManifestResponse {
            manifest_id: id.clone(),
            manifest: manifest.clone(),
        };
        let key = scope_key(&fx.scope, MediaObjectKind::Manifest, &manifest.media_oid).unwrap();
        fx.service
            .put_metadata(&key, Bytes::from(serde_json::to_vec(&spoof).unwrap()))
            .await
            .unwrap();
        assert!(matches!(
            load_by_media(&fx.service, &fx.scope, &manifest.media_oid).await,
            Err(MediaError::NotFound)
        ));
        finalize::finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &id,
            41,
            &StorageEventEmitter::disabled(),
        )
        .await
        .unwrap();
        let by = load_by_media(&fx.service, &fx.scope, &manifest.media_oid)
            .await
            .unwrap();
        assert_eq!(by.manifest_id, id);
    }
}
