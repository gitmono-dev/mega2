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
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
) -> Result<ManifestResponse, MediaError> {
    finalize_at(media, lfs_db, scope, manifest_id, unix_now(), emitter).await
}

pub async fn finalize_at(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    scope: &MediaScope,
    manifest_id: &str,
    now: u64,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
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
        publish_finalized(media, scope, manifest_id, &manifest, now, emitter).await
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
    now: u64,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
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

    // WH-06 (plan-20260912 / ADR-WH-04/05): exactly one `lfs.media.finalized`
    // when THIS finalize actually put the finalized manifest (the exists
    // no-op above returns before this point; intermediate chunk/fallback
    // steps never emit). Scope metadata comes from the server-side
    // MediaScope's canonical repository, never from the request body; the
    // actor never leaves the process. A repeat cross-process finalize may
    // still rewrite the fallback and re-notify (no uniqueness lock added).
    let event = crate::jupiter::service::storage_event::CommittedEvent {
        event_id: uuid::Uuid::new_v4(),
        event_type: crate::jupiter::service::storage_event::EventType::LfsMediaFinalized,
        occurred_at: now,
        source: crate::jupiter::service::storage_event::EventSource::Lfs,
        scope: crate::jupiter::service::storage_event::EventScope::Media {
            repo_path: scope.repository().to_owned(),
        },
        data: crate::jupiter::service::storage_event::EventData::LfsMediaFinalized {
            oid: manifest.media_oid.clone(),
            size: manifest.media_size,
            manifest_id: manifest_id.to_owned(),
        },
    };
    let _ = emitter.try_emit(event);
    Ok(response)
}

fn map_db(err: crate::common::errors::MegaError) -> MediaError {
    let _ = redact_storage_error(&err);
    MediaError::Storage
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::{
        ceres::lfs::media::{
            chunker,
            protocol::{ChunkEntry, CreatedBy},
            service::MediaService,
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
        orbit_api::factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig},
    };

    /// Recording fake transport (WH-09 seam): exact bodies + call counter.
    #[derive(Default)]
    struct RecordingTransport {
        calls: std::sync::atomic::AtomicUsize,
        bodies: std::sync::Mutex<Vec<Bytes>>,
        fail: bool,
    }

    impl RecordingTransport {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<Bytes> {
            self.bodies.lock().expect("bodies").clone()
        }
    }

    impl crate::jupiter::service::storage_event_transport::EventTransport for RecordingTransport {
        fn post(
            &self,
            _target: &crate::jupiter::service::storage_event_transport::EventTarget,
            body: Bytes,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::jupiter::service::storage_event_transport::TransportSuccess,
                            crate::jupiter::service::storage_event_transport::TransportError,
                        >,
                    > + Send,
            >,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.bodies.lock().expect("bodies").push(body);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(crate::jupiter::service::storage_event_transport::TransportError::Timeout)
                } else {
                    Ok(
                        crate::jupiter::service::storage_event_transport::TransportSuccess::Accepted2xx {
                            status: 200,
                        },
                    )
                }
            })
        }
    }

    const WH06_INSTALLATION: &str = "it-wh06";

    fn wh06_target(
        id: &str,
        lfs_paths: Vec<String>,
    ) -> (
        crate::config::StorageEventsTargetConfig,
        crate::jupiter::service::storage_event_transport::EventTarget,
    ) {
        let config = crate::config::StorageEventsTargetConfig {
            id: id.to_owned(),
            url: "https://events.example.invalid/ingest".to_owned(),
            secret_ref: format!("vault://secret/config/it/storage_events/targets/{id}/hmac#value"),
            events: vec!["lfs.media.finalized".to_owned()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths,
            include_unscoped_lfs: false,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        };
        let secret = crate::config::secret::SecretString::new(
            "hex:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let compiled = crate::jupiter::service::storage_event_transport::EventTarget::compile(
            &config.id,
            &config.url,
            &secret,
        )
        .expect("compile target");
        (config, compiled)
    }

    fn wh06_emitter(
        transport: std::sync::Arc<RecordingTransport>,
        targets: Vec<(
            crate::config::StorageEventsTargetConfig,
            crate::jupiter::service::storage_event_transport::EventTarget,
        )>,
    ) -> StorageEventEmitter {
        let mut config =
            crate::config::testing::isolated_config(std::env::temp_dir().join("wh06-finalize"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some(WH06_INSTALLATION.to_owned());
        StorageEventEmitter::new_with_transport(&config, transport, targets)
    }

    /// Self-contained FastCDC fixture: local object store + real test DB +
    /// MediaService + server-side scope (mirrors media/mod.rs's db_fixture).
    struct Wh06Fixture {
        _obj_dir: tempfile::TempDir,
        _db_dir: tempfile::TempDir,
        service: MediaService,
        lfs_db: LfsDbStorage,
        scope: MediaScope,
    }

    async fn wh06_fixture(repo: &str) -> Wh06Fixture {
        let obj_dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: obj_dir.path().to_string_lossy().into_owned(),
            },
            ..Default::default()
        };
        let store = build_object_storage(&cfg).await.unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let db = test_db_connection(db_dir.path()).await;
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .unwrap();
        let lfs_db = LfsDbStorage {
            base: BaseStorage::new(std::sync::Arc::new(db)),
        };
        Wh06Fixture {
            _obj_dir: obj_dir,
            _db_dir: db_dir,
            service: MediaService::new(store),
            lfs_db,
            scope: MediaScope::from_server("user-1", repo).unwrap(),
        }
    }

    fn wh06_manifest(data: &[u8]) -> (MediaManifest, Vec<(String, Bytes)>) {
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
            algorithm: "fastcdc-v1".to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid: LfsDigest::sha256_of(data).hex().to_owned(),
            media_size: data.len() as u64,
            chunks,
            created_by: CreatedBy {
                client: "test".to_string(),
                version: "0".to_string(),
                capabilities: vec!["fastcdc-v1".to_string()],
            },
            fallback_oid: None,
        };
        (manifest, bodies)
    }

    /// Prepare + upload all chunks, ready for finalize.
    async fn wh06_prepare(fx: &Wh06Fixture, data: &[u8], now: u64) -> (MediaManifest, String) {
        let (manifest, bodies) = wh06_manifest(data);
        let prepared = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), now)
            .await
            .unwrap();
        for (hash, body) in &bodies {
            fx.service
                .upload_chunk_at(&fx.scope, &prepared.manifest_id, hash, body.clone(), now)
                .await
                .unwrap();
        }
        (manifest, prepared.manifest_id)
    }

    async fn wh06_wait_calls(transport: &RecordingTransport, n: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if transport.calls() >= n {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delivery within 2s");
    }

    #[tokio::test]
    async fn storage_event_finalize_matrix() {
        // A real finalize that actually puts the finalized manifest delivers
        // exactly one event with the committed snapshot from the server-side
        // MediaScope's canonical repository.
        let transport = std::sync::Arc::new(RecordingTransport::default());
        let emitter = wh06_emitter(
            transport.clone(),
            vec![wh06_target("ops-main", vec!["/".to_owned()])],
        );
        let fx = wh06_fixture("/acme/app").await;
        let data = b"wh06 finalize matrix object";
        let (manifest, manifest_id) = wh06_prepare(&fx, data, 10).await;
        let published = finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &manifest_id,
            11,
            &emitter,
        )
        .await
        .expect("finalize");
        assert_eq!(published.manifest.media_oid, manifest.media_oid);
        wh06_wait_calls(&transport, 1).await;
        let bodies = transport.bodies();
        assert_eq!(bodies.len(), 1, "exactly one finalized event");
        let envelope: serde_json::Value = serde_json::from_slice(&bodies[0]).expect("envelope");
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["event_type"], "lfs.media.finalized");
        assert_eq!(envelope["source"], "lfs");
        assert_eq!(envelope["scope"]["repo_path"], "/acme/app");
        assert!(envelope["scope"]["tenant_id"].is_null());
        assert!(envelope["scope"]["oci_repository"].is_null());
        assert_eq!(envelope["data"]["oid"], manifest.media_oid);
        assert_eq!(
            envelope["data"]["size"].as_u64().expect("size"),
            manifest.media_size
        );
        assert_eq!(envelope["data"]["manifest_id"], manifest_id);
        assert_eq!(envelope["data"]["transfer"], "fastcdc");
        assert_eq!(envelope["occurred_at"].as_u64().expect("ts"), 11);

        // Repeat finalize is the exists no-op: no second event.
        let again = finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &manifest_id,
            12,
            &emitter,
        )
        .await
        .expect("repeat finalize is a no-op");
        assert_eq!(again.manifest_id, manifest_id);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "repeat finalize delivers nothing");

        // A finalize that fails BEFORE the finalized manifest put (corrupt
        // chunk) delivers nothing.
        let (manifest2, manifest_id2) = wh06_prepare(&fx, b"wh06 corrupt", 20).await;
        if let Some((hash, _)) = manifest2.chunks.first().map(|c| (c.chunk_hash.clone(), ())) {
            fx.service
                .overwrite_chunk_for_test(&fx.scope, &hash, Bytes::from_static(b"tampered"))
                .await
                .unwrap();
        }
        let failed = finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &manifest_id2,
            21,
            &emitter,
        )
        .await;
        assert!(failed.is_err(), "corrupt chunk must fail finalize");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "failed finalize delivers nothing");

        tokio::time::timeout(std::time::Duration::from_secs(3), emitter.shutdown())
            .await
            .expect("shutdown within 3s");
        assert_eq!(transport.calls(), 1, "no late delivery after drain");

        // Filter isolation: a target scoped to another repository receives
        // nothing for this repo's finalize.
        let other_transport = std::sync::Arc::new(RecordingTransport::default());
        let other_emitter = wh06_emitter(
            other_transport.clone(),
            vec![wh06_target("ops-other", vec!["/other/repo".to_owned()])],
        );
        let fx2 = wh06_fixture("/acme/app").await;
        let (_manifest3, manifest_id3) = wh06_prepare(&fx2, b"wh06 isolation", 30).await;
        finalize_at(
            &fx2.service,
            &fx2.lfs_db,
            &fx2.scope,
            &manifest_id3,
            31,
            &other_emitter,
        )
        .await
        .expect("finalize");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            other_transport.calls(),
            0,
            "a non-matching lfs_paths filter receives nothing"
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), other_emitter.shutdown())
            .await
            .expect("shutdown within 3s");
        assert_eq!(other_transport.calls(), 0);

        // Emitter failure never changes the finalize result.
        let failing = std::sync::Arc::new(RecordingTransport {
            fail: true,
            ..Default::default()
        });
        let failing_emitter = wh06_emitter(
            failing.clone(),
            vec![wh06_target("ops-main", vec!["/".to_owned()])],
        );
        let fx3 = wh06_fixture("/acme/app").await;
        let (manifest4, manifest_id4) = wh06_prepare(&fx3, b"wh06 failing emitter", 40).await;
        let published = finalize_at(
            &fx3.service,
            &fx3.lfs_db,
            &fx3.scope,
            &manifest_id4,
            41,
            &failing_emitter,
        )
        .await
        .expect("finalize succeeds despite emitter failure");
        assert_eq!(published.manifest.media_oid, manifest4.media_oid);
        wh06_wait_calls(&failing, 1).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            failing_emitter.shutdown(),
        )
        .await
        .expect("shutdown within 3s");
        assert_eq!(failing.calls(), 1);
    }
}
