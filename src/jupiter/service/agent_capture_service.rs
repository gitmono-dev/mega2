use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use futures::StreamExt;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    callisto::{agent_capture_blob, agent_capture_session},
    common::errors::MegaError,
    jupiter::{
        service::{
            storage_event::{
                CommittedEvent, EventData, EventScope, EventSource, EventType,
                validate_canonical_path,
            },
            storage_event_emitter::StorageEventEmitter,
        },
        storage::{
            agent_capture_storage::{
                AgentCaptureStorage, CheckpointSnapshot, EventsBatchGroup, EventsBatchSnapshot,
                InsertCheckpoint, InsertEvent,
            },
            base_storage::{BaseStorage, StorageConnector},
            object_storage::{MegaObjectStorageWrapper, mock_object_storage},
        },
    },
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};

#[derive(Clone)]
pub struct AgentCaptureService {
    pub storage: AgentCaptureStorage,
    pub obj_storage: MegaObjectStorageWrapper,
    pub storage_event_emitter: StorageEventEmitter,
}

impl AgentCaptureService {
    pub fn mock() -> Self {
        Self {
            storage: AgentCaptureStorage {
                base: BaseStorage::mock(),
            },
            obj_storage: mock_object_storage(),
            storage_event_emitter: StorageEventEmitter::disabled(),
        }
    }

    pub fn staging_key(deployment_id: &str, tenant_id: &str, lease_id: &str) -> ObjectKey {
        ObjectKey {
            namespace: ObjectNamespace::Agent,
            key: format!("{deployment_id}/{tenant_id}/staging/{lease_id}"),
        }
    }

    pub fn committed_key(
        deployment_id: &str,
        tenant_id: &str,
        visibility: &str,
        hex: &str,
    ) -> ObjectKey {
        ObjectKey {
            namespace: ObjectNamespace::Agent,
            key: format!("{deployment_id}/{tenant_id}/{visibility}/sha256/{hex}"),
        }
    }

    pub async fn put_staging(
        &self,
        deployment_id: &str,
        tenant_id: &str,
        lease_id: &str,
        stream: ObjectByteStream,
    ) -> Result<ObjectKey, MegaError> {
        let key = Self::staging_key(deployment_id, tenant_id, lease_id);
        self.obj_storage
            .inner
            .put_stream(&key, stream, ObjectMeta::default())
            .await?;
        Ok(key)
    }

    /// Hash the staged object incrementally, then reopen its stream to write
    /// the server-derived committed key. Any client-supplied final key is ignored.
    pub async fn finalize_blob(
        &self,
        deployment_id: &str,
        tenant_id: &str,
        lease_id: &str,
        _claimed_final_key: Option<&str>,
    ) -> Result<String, MegaError> {
        let staging = Self::staging_key(deployment_id, tenant_id, lease_id);
        let (hash_stream, _) = self.obj_storage.inner.get_stream(&staging).await?;
        let hex = sha256_hex(hash_stream).await?;
        let digest = format!("sha256:{hex}");
        let key = Self::committed_key(deployment_id, tenant_id, "raw", &hex);
        let (copy_stream, meta) = self.obj_storage.inner.get_stream(&staging).await?;
        self.obj_storage
            .inner
            .put_stream(&key, copy_stream, meta)
            .await?;
        Ok(digest)
    }

    /// Commit an events batch and emit `agent_capture.events.committed` only
    /// when this transaction inserted new event rows. The outbound payload is
    /// built from the committed snapshot; send tasks never re-read session
    /// latest state (WH-07 / AC5).
    pub async fn commit_events_batch(
        &self,
        capture_id: i64,
        batch_id: &str,
        events: &[InsertEvent],
        completeness: Option<&str>,
        stream_kind: &str,
    ) -> Result<EventsBatchSnapshot, MegaError> {
        let snapshot = self
            .storage
            .insert_events_batch(capture_id, batch_id, events, completeness, stream_kind)
            .await?;
        for group in &snapshot.groups {
            if group.new_event_count > 0
                && let Ok(event) = Self::events_committed_event(&snapshot, group)
            {
                let _ = self.storage_event_emitter.try_emit(event);
            }
        }
        Ok(snapshot)
    }

    /// Commit a checkpoint and emit `agent_capture.checkpoint.committed` only
    /// when this transaction created a new checkpoint row. The outbound
    /// payload is built from the committed snapshot; send tasks never re-read
    /// session latest state (WH-08 / AC5).
    pub async fn commit_checkpoint(
        &self,
        capture_id: i64,
        checkpoint: &InsertCheckpoint,
        fingerprint: &str,
    ) -> Result<CheckpointSnapshot, MegaError> {
        let snapshot = self
            .storage
            .insert_checkpoint_ingest(capture_id, checkpoint, fingerprint)
            .await?;
        if snapshot.created
            && let Ok(event) = Self::checkpoint_committed_event(&snapshot)
        {
            let _ = self.storage_event_emitter.try_emit(event);
        }
        Ok(snapshot)
    }

    fn checkpoint_committed_event(
        snapshot: &CheckpointSnapshot,
    ) -> Result<CommittedEvent, MegaError> {
        validate_canonical_path(&snapshot.repo_path)?;
        Ok(CommittedEvent {
            event_id: Uuid::new_v4(),
            event_type: EventType::AgentCaptureCheckpointCommitted,
            occurred_at: u64::try_from(Utc::now().timestamp()).unwrap_or(0),
            source: EventSource::AgentCapture,
            scope: EventScope::AgentCapture {
                tenant_id: snapshot.tenant_id.clone(),
                repo_path: snapshot.repo_path.clone(),
            },
            data: EventData::AgentCaptureCheckpointCommitted {
                capture_id: snapshot.capture_id.to_string(),
                checkpoint_id: snapshot.checkpoint_id.clone(),
                completeness: snapshot.completeness.clone(),
                raw_committed: snapshot.raw_committed,
            },
        })
    }

    fn events_committed_event(
        snapshot: &EventsBatchSnapshot,
        group: &EventsBatchGroup,
    ) -> Result<CommittedEvent, MegaError> {
        validate_canonical_path(&snapshot.repo_path)?;
        Ok(CommittedEvent {
            event_id: Uuid::new_v4(),
            event_type: EventType::AgentCaptureEventsCommitted,
            occurred_at: u64::try_from(Utc::now().timestamp()).unwrap_or(0),
            source: EventSource::AgentCapture,
            scope: EventScope::AgentCapture {
                tenant_id: snapshot.tenant_id.clone(),
                repo_path: snapshot.repo_path.clone(),
            },
            data: EventData::AgentCaptureEventsCommitted {
                capture_id: snapshot.capture_id.to_string(),
                receipt_id: snapshot.receipt_id.to_string(),
                new_event_count: group.new_event_count,
                stream_kind: snapshot.stream_kind.clone(),
                generation: group.generation,
                completeness: snapshot.completeness.clone(),
            },
        })
    }

    pub async fn load_session(
        &self,
        capture_id: i64,
        deployment_id: &str,
        tenant_id: &str,
    ) -> Result<Option<agent_capture_session::Model>, MegaError> {
        Ok(agent_capture_session::Entity::find_by_id(capture_id)
            .filter(agent_capture_session::Column::DeploymentId.eq(deployment_id.to_owned()))
            .filter(agent_capture_session::Column::TenantId.eq(tenant_id.to_owned()))
            .one(self.storage.get_connection())
            .await?)
    }

    pub async fn stage_session_blob(
        &self,
        capture_id: i64,
        deployment_id: &str,
        tenant_id: &str,
        lease_ttl_seconds: u64,
        max_bytes: u64,
        stream: ObjectByteStream,
    ) -> Result<String, MegaError> {
        self.load_session(capture_id, deployment_id, tenant_id)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("agent_capture_session {capture_id} does not exist"))
            })?;
        let lease_id = Uuid::new_v4().to_string();
        let size = std::sync::Arc::new(AtomicU64::new(0));
        let object_key = Self::staging_key(deployment_id, tenant_id, &lease_id).key;
        let (blob_id, _) = self
            .storage
            .insert_staging_lease(
                capture_id,
                &lease_id,
                &object_key,
                0,
                i64::try_from(lease_ttl_seconds).unwrap_or(900),
            )
            .await?;
        let limited = limit_stream(stream, max_bytes, size.clone());
        self.put_staging(deployment_id, tenant_id, &lease_id, limited)
            .await
            .map_err(|err| {
                if err.to_string().contains("payload too large") {
                    MegaError::Other("payload too large".to_owned())
                } else {
                    err
                }
            })?;
        let size_bytes = i64::try_from(size.load(Ordering::Relaxed)).unwrap_or(i64::MAX);
        self.storage.set_staging_size(blob_id, size_bytes).await?;
        Ok(lease_id)
    }

    pub async fn check_finalize_lease(
        &self,
        capture_id: i64,
        deployment_id: &str,
        tenant_id: &str,
        lease_id: &str,
    ) -> Result<(), MegaError> {
        self.load_finalize_blob(capture_id, deployment_id, tenant_id, lease_id)
            .await
            .map(|_| ())
    }

    async fn load_finalize_blob(
        &self,
        capture_id: i64,
        deployment_id: &str,
        tenant_id: &str,
        lease_id: &str,
    ) -> Result<agent_capture_blob::Model, MegaError> {
        self.load_session(capture_id, deployment_id, tenant_id)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("agent_capture_session {capture_id} does not exist"))
            })?;
        let Some(blob) = agent_capture_blob::Entity::find()
            .filter(agent_capture_blob::Column::DeploymentId.eq(deployment_id.to_owned()))
            .filter(agent_capture_blob::Column::TenantId.eq(tenant_id.to_owned()))
            .filter(agent_capture_blob::Column::LeaseId.eq(lease_id.to_owned()))
            .one(self.storage.get_connection())
            .await?
        else {
            return Err(MegaError::Other(format!(
                "agent_capture_blob lease {lease_id} does not exist"
            )));
        };
        let now = Utc::now();
        let expired = blob
            .lease_expires_at
            .map(|expires| expires.with_timezone(&Utc) < now)
            .unwrap_or(false);
        if blob.capture_id != Some(capture_id) {
            if blob.lease_state == "staging" && !expired {
                return Err(MegaError::Other(format!(
                    "lease conflict for capture_id {capture_id}"
                )));
            }
            return Err(MegaError::Other(format!(
                "agent_capture_blob lease {lease_id} does not exist"
            )));
        }
        Ok(blob)
    }

    pub async fn finalize_session_blob(
        &self,
        capture_id: i64,
        deployment_id: &str,
        tenant_id: &str,
        lease_id: &str,
        claimed_digest: Option<&str>,
        _claimed_object_key: Option<&str>,
    ) -> Result<FinalizedBlob, MegaError> {
        let blob = self
            .load_finalize_blob(capture_id, deployment_id, tenant_id, lease_id)
            .await?;
        if blob.lease_state == "committed" {
            let committed = self
                .storage
                .load_committed_finalize(capture_id, lease_id)
                .await?;
            if let Some(claimed) = claimed_digest
                && claimed != committed.digest
            {
                return Err(MegaError::Other(format!(
                    "lease conflict: ingest receipt fingerprint conflict for capture_id {capture_id}"
                )));
            }
            let obj = self.obj_storage.clone();
            let staging = Self::staging_key(deployment_id, tenant_id, lease_id);
            let _ = self
                .storage
                .run_staging_cleanup(blob.id, move || async move {
                    delete_staging_object(&obj, &staging).await
                })
                .await;
            return Ok(FinalizedBlob {
                digest: committed.digest,
                object_key: committed.object_key,
            });
        }
        let staging = Self::staging_key(deployment_id, tenant_id, lease_id);
        let (hash_stream, _) = self.obj_storage.inner.get_stream(&staging).await?;
        let hex = sha256_hex(hash_stream).await?;
        let digest = format!("sha256:{hex}");
        if let Some(claimed) = claimed_digest
            && claimed != digest
        {
            return Err(MegaError::Other("digest mismatch".to_owned()));
        }
        let committed = Self::committed_key(deployment_id, tenant_id, "raw", &hex);
        let obj = self.obj_storage.clone();
        let staging_for_copy = staging.clone();
        let committed_for_copy = committed.clone();
        self.storage
            .run_object_write(blob.id, || async move {
                let (copy_stream, meta) = obj.inner.get_stream(&staging_for_copy).await?;
                obj.inner
                    .put_stream(&committed_for_copy, copy_stream, meta)
                    .await?;
                Ok(())
            })
            .await?;
        self.storage
            .commit_lease_fenced(
                capture_id,
                lease_id,
                blob.lease_generation,
                &digest,
                &committed.key,
                blob.size_bytes,
            )
            .await?;
        let obj = self.obj_storage.clone();
        let staging_for_cleanup = staging;
        let _ = self
            .storage
            .run_staging_cleanup(blob.id, move || async move {
                delete_staging_object(&obj, &staging_for_cleanup).await
            })
            .await;
        Ok(FinalizedBlob {
            digest,
            object_key: committed.key,
        })
    }
}

pub struct FinalizedBlob {
    pub digest: String,
    pub object_key: String,
}

async fn delete_staging_object(
    obj: &MegaObjectStorageWrapper,
    key: &ObjectKey,
) -> Result<(), MegaError> {
    match obj.inner.delete(key).await {
        Ok(()) => Ok(()),
        Err(err) => {
            let text = err.to_string().to_lowercase();
            if text.contains("not found") || text.contains("notfound") {
                Ok(())
            } else {
                Err(err.into())
            }
        }
    }
}

fn limit_stream(
    mut inner: ObjectByteStream,
    max_bytes: u64,
    size: std::sync::Arc<AtomicU64>,
) -> ObjectByteStream {
    Box::pin(async_stream::stream! {
        let mut seen = 0u64;
        while let Some(item) = inner.next().await {
            let chunk = item?;
            seen = seen.saturating_add(chunk.len() as u64);
            if seen > max_bytes {
                yield Err(std::io::Error::other("payload too large"));
                return;
            }
            size.store(seen, Ordering::Relaxed);
            yield Ok(chunk);
        }
    })
}

async fn sha256_hex(mut stream: ObjectByteStream) -> Result<String, MegaError> {
    let mut hasher = Sha256::new();
    while let Some(chunk) = stream.next().await {
        hasher.update(&chunk?);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::stream;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, Set};

    use super::*;
    use crate::{
        callisto::{agent_capture_blob, agent_capture_session},
        jupiter::{
            migration::apply_migrations,
            storage::{
                agent_capture_storage::{InsertCheckpoint, InsertEvent, SessionNaturalKey},
                base_storage::StorageConnector,
            },
            tests::test_db_connection,
        },
    };

    fn bytes_stream(chunks: Vec<Bytes>) -> ObjectByteStream {
        Box::pin(stream::iter(chunks.into_iter().map(Ok)))
    }

    async fn collect_bytes(mut stream: ObjectByteStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.expect("chunk"));
        }
        out
    }

    #[test]
    fn staging_key_includes_tenant() {
        let key = AgentCaptureService::staging_key("default", "acme", "lease-1");
        assert_eq!(key.namespace.to_string(), "agent");
        assert!(
            key.key.contains("acme/staging/"),
            "staging path must include tenant_id/staging/: {}",
            key.key
        );
    }

    #[test]
    fn committed_key_includes_tenant_and_raw() {
        let key = AgentCaptureService::committed_key("default", "acme", "raw", "abc");
        assert_eq!(key.namespace.to_string(), "agent");
        assert!(
            key.key.contains("acme/raw/sha256/"),
            "committed path must include tenant_id/raw/sha256/: {}",
            key.key
        );
    }

    #[tokio::test]
    async fn finalize_uses_server_digest() {
        let service = AgentCaptureService::mock();
        let payload = Bytes::from_static(b"agent-capture-bytes");
        service
            .put_staging(
                "default",
                "acme",
                "lease-1",
                bytes_stream(vec![payload.clone()]),
            )
            .await
            .expect("staging");
        let digest = service
            .finalize_blob("default", "acme", "lease-1", Some("client/final/key"))
            .await
            .expect("finalize");
        let expected = format!("sha256:{}", hex::encode(Sha256::digest(&payload)));
        assert_eq!(digest, expected);
        let hex = digest.strip_prefix("sha256:").expect("sha256 prefix");
        let stored = AgentCaptureService::committed_key("default", "acme", "raw", hex);
        assert!(
            service
                .obj_storage
                .inner
                .exists(&stored)
                .await
                .expect("exists")
        );
        let (stream, _) = service
            .obj_storage
            .inner
            .get_stream(&stored)
            .await
            .expect("committed stream");
        assert_eq!(collect_bytes(stream).await, payload.as_ref());
        let claimed = ObjectKey {
            namespace: ObjectNamespace::Agent,
            key: "client/final/key".to_owned(),
        };
        assert!(
            !service
                .obj_storage
                .inner
                .exists(&claimed)
                .await
                .expect("claimed key must not be stored")
        );
    }

    #[tokio::test]
    async fn finalize_hashes_multi_chunk_staging() {
        let service = AgentCaptureService::mock();
        let chunks = vec![
            Bytes::from_static(b"chunk-one-"),
            Bytes::from_static(b"chunk-two-"),
            Bytes::from_static(b"chunk-three"),
        ];
        let expected: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        service
            .put_staging("default", "acme", "lease-2", bytes_stream(chunks))
            .await
            .expect("staging");
        let digest = service
            .finalize_blob("default", "acme", "lease-2", Some("ignored/key"))
            .await
            .expect("finalize");
        assert_eq!(
            digest,
            format!("sha256:{}", hex::encode(Sha256::digest(&expected)))
        );
        let hex = digest.strip_prefix("sha256:").expect("sha256 prefix");
        let stored = AgentCaptureService::committed_key("default", "acme", "raw", hex);
        let (stream, _) = service
            .obj_storage
            .inner
            .get_stream(&stored)
            .await
            .expect("committed stream");
        assert_eq!(collect_bytes(stream).await, expected);
        let claimed = ObjectKey {
            namespace: ObjectNamespace::Agent,
            key: "ignored/key".to_owned(),
        };
        assert!(
            !service
                .obj_storage
                .inner
                .exists(&claimed)
                .await
                .expect("claimed key must not be stored")
        );
    }

    #[tokio::test]
    async fn finalize_retry_after_staging_cleanup() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        let service = AgentCaptureService {
            storage: AgentCaptureStorage {
                base: BaseStorage::new(std::sync::Arc::new(db)),
            },
            obj_storage: mock_object_storage(),
            storage_event_emitter: StorageEventEmitter::disabled(),
        };
        let capture_id = service
            .storage
            .upsert_session(SessionNaturalKey {
                deployment_id: "default".to_owned(),
                tenant_id: "default".to_owned(),
                repo_id: "/third-part/mega".to_owned(),
                producer_id: "hook".to_owned(),
                session_kind: "external_capture".to_owned(),
                client_session_id: "provider__retry".to_owned(),
            })
            .await
            .expect("session");
        let payload = Bytes::from_static(b"retry-after-cleanup");
        let lease_id = service
            .stage_session_blob(
                capture_id,
                "default",
                "default",
                900,
                1_048_576,
                bytes_stream(vec![payload.clone()]),
            )
            .await
            .expect("stage");
        let first = service
            .finalize_session_blob(capture_id, "default", "default", &lease_id, None, None)
            .await
            .expect("first finalize");
        let cas = agent_capture_blob::Entity::find()
            .filter(agent_capture_blob::Column::Digest.eq(first.digest.clone()))
            .one(service.storage.get_connection())
            .await
            .expect("load cas")
            .expect("cas");
        assert!(
            cas.size_bytes > 0,
            "committed blob must persist measured staging size"
        );
        let mismatch = service
            .finalize_session_blob(
                capture_id,
                "default",
                "default",
                &lease_id,
                Some("sha256:deadbeef"),
                None,
            )
            .await;
        assert!(mismatch.is_err(), "committed digest mismatch");
        let mismatch = mismatch.err().expect("error").to_string();
        assert!(mismatch.contains("lease conflict"));
        assert!(mismatch.contains("fingerprint"));
        let staging = AgentCaptureService::staging_key("default", "default", &lease_id);
        assert!(
            !service
                .obj_storage
                .inner
                .exists(&staging)
                .await
                .expect("staging deleted")
        );
        let second = service
            .finalize_session_blob(capture_id, "default", "default", &lease_id, None, None)
            .await
            .expect("retry after cleanup");
        assert_eq!(first.digest, second.digest);
        assert_eq!(first.object_key, second.object_key);
    }

    #[derive(Default)]
    struct RecordingTransport {
        calls: std::sync::atomic::AtomicUsize,
        bodies: std::sync::Mutex<Vec<bytes::Bytes>>,
        target_ids: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingTransport {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<bytes::Bytes> {
            self.bodies.lock().expect("bodies").clone()
        }

        fn target_ids(&self) -> Vec<String> {
            self.target_ids.lock().expect("ids").clone()
        }
    }

    impl crate::jupiter::service::storage_event_transport::EventTransport for RecordingTransport {
        fn post(
            &self,
            target: &crate::jupiter::service::storage_event_transport::EventTarget,
            body: bytes::Bytes,
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
            self.target_ids.lock().expect("ids").push(target.id.clone());
            Box::pin(async {
                Ok(
                    crate::jupiter::service::storage_event_transport::TransportSuccess::Accepted2xx {
                        status: 200,
                    },
                )
            })
        }
    }

    fn wh07_target(
        id: &str,
        tenants: Vec<String>,
        repos: Vec<String>,
    ) -> (
        crate::config::StorageEventsTargetConfig,
        crate::jupiter::service::storage_event_transport::EventTarget,
    ) {
        let config = crate::config::StorageEventsTargetConfig {
            id: id.to_owned(),
            url: "https://events.example.invalid/ingest".to_owned(),
            secret_ref: format!("vault://secret/config/it/storage_events/targets/{id}/hmac#value"),
            events: vec!["agent_capture.events.committed".to_owned()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: false,
            agent_tenants: tenants,
            agent_repo_paths: repos,
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

    fn wh07_emitter(
        transport: std::sync::Arc<RecordingTransport>,
        targets: Vec<(
            crate::config::StorageEventsTargetConfig,
            crate::jupiter::service::storage_event_transport::EventTarget,
        )>,
    ) -> StorageEventEmitter {
        let mut config =
            crate::config::testing::isolated_config(std::env::temp_dir().join("wh07-batch"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some("it-wh07".to_owned());
        StorageEventEmitter::new_with_transport(&config, transport, targets)
    }

    async fn wh07_wait_calls(transport: &RecordingTransport, n: usize) {
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

    fn wh07_event(capture_id: i64, uid: &str, n: i64) -> InsertEvent {
        InsertEvent {
            capture_id,
            event_uid: uid.to_owned(),
            event_kind: "message".to_owned(),
            native_id: None,
            lifecycle_seq: None,
            payload: serde_json::json!({ "n": n }),
        }
    }

    #[tokio::test]
    async fn storage_event_batch_snapshot() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        let transport = std::sync::Arc::new(RecordingTransport::default());
        let emitter = wh07_emitter(
            transport.clone(),
            vec![
                wh07_target(
                    "ops-main",
                    vec!["default".to_owned()],
                    vec!["/third-part/mega".to_owned()],
                ),
                wh07_target(
                    "other-tenant",
                    vec!["other".to_owned()],
                    vec!["/third-part/mega".to_owned()],
                ),
                wh07_target(
                    "other-repo",
                    vec!["default".to_owned()],
                    vec!["/other/repo".to_owned()],
                ),
            ],
        );
        let service = AgentCaptureService {
            storage: AgentCaptureStorage {
                base: BaseStorage::new(std::sync::Arc::new(db)),
            },
            obj_storage: mock_object_storage(),
            storage_event_emitter: emitter,
        };
        let capture_id = service
            .storage
            .upsert_session(SessionNaturalKey {
                deployment_id: "default".to_owned(),
                tenant_id: "default".to_owned(),
                repo_id: "/third-part/mega".to_owned(),
                producer_id: "hook".to_owned(),
                session_kind: "external_capture".to_owned(),
                client_session_id: "provider__wh07".to_owned(),
            })
            .await
            .expect("session");

        let snapshot = service
            .commit_events_batch(
                capture_id,
                "b1",
                &[wh07_event(capture_id, "0:0", 1)],
                Some("complete"),
                "jsonl",
            )
            .await
            .expect("commit");
        assert_eq!(snapshot.new_event_count, 1);
        assert_eq!(snapshot.completeness, "complete");
        assert_eq!(snapshot.stream_kind, "external_capture");
        assert_eq!(snapshot.groups.len(), 1);
        assert_eq!(snapshot.groups[0].generation, 0);
        wh07_wait_calls(&transport, 1).await;
        assert_eq!(transport.calls(), 1, "exactly one matching target");
        assert_eq!(transport.target_ids(), vec!["ops-main".to_owned()]);
        let envelope: serde_json::Value =
            serde_json::from_slice(&transport.bodies()[0]).expect("envelope");
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["event_type"], "agent_capture.events.committed");
        assert_eq!(envelope["source"], "agent_capture");
        assert_eq!(envelope["scope"]["tenant_id"], "default");
        assert_eq!(envelope["scope"]["repo_path"], "/third-part/mega");
        assert!(envelope["scope"]["oci_repository"].is_null());
        assert_eq!(envelope["data"]["capture_id"], capture_id.to_string());
        assert_eq!(
            envelope["data"]["receipt_id"],
            snapshot.receipt_id.to_string()
        );
        assert_eq!(envelope["data"]["new_event_count"], 1);
        assert_eq!(envelope["data"]["stream_kind"], "external_capture");
        assert_eq!(envelope["data"]["generation"], 0);
        assert_eq!(envelope["data"]["completeness"], "complete");

        let mut session = agent_capture_session::Entity::find_by_id(capture_id)
            .one(service.storage.get_connection())
            .await
            .expect("load")
            .expect("session");
        session.completeness = "truncated".to_owned();
        let mut active = session.into_active_model();
        active.completeness = sea_orm::Set("truncated".to_owned());
        active
            .update(service.storage.get_connection())
            .await
            .expect("mutate latest completeness");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let after: serde_json::Value =
            serde_json::from_slice(&transport.bodies()[0]).expect("frozen envelope");
        assert_eq!(
            after["data"]["completeness"], "complete",
            "delayed send must not re-read session latest"
        );

        let replay = service
            .commit_events_batch(
                capture_id,
                "b1",
                &[wh07_event(capture_id, "0:0", 1)],
                Some("complete"),
                "jsonl",
            )
            .await
            .expect("replay");
        assert_eq!(replay.new_event_count, 0);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "replay must not emit");

        let zero = service
            .commit_events_batch(
                capture_id,
                "b2",
                &[wh07_event(capture_id, "0:0", 1)],
                None,
                "jsonl",
            )
            .await
            .expect("old uid new receipt");
        assert_eq!(zero.new_event_count, 0);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "zero new rows must not emit");

        let two_gens = service
            .commit_events_batch(
                capture_id,
                "b3",
                &[
                    wh07_event(capture_id, "3:0", 3),
                    wh07_event(capture_id, "4:0", 4),
                ],
                None,
                "jsonl",
            )
            .await
            .expect("two generations");
        assert_eq!(two_gens.new_event_count, 2);
        assert_eq!(two_gens.groups.len(), 2);
        assert_eq!(two_gens.completeness, "truncated");
        wh07_wait_calls(&transport, 3).await;
        assert_eq!(
            transport.calls(),
            3,
            "one outbound summary per new generation"
        );
        assert!(
            transport.target_ids().iter().all(|id| id == "ops-main"),
            "AND isolation: only the matching target is selected"
        );
        let bodies = transport.bodies();
        let gens: Vec<u64> = bodies[1..]
            .iter()
            .map(|body| {
                let envelope: serde_json::Value = serde_json::from_slice(body).expect("envelope");
                envelope["data"]["generation"].as_u64().expect("generation")
            })
            .collect();
        assert_eq!(gens, vec![3, 4]);
        for body in &bodies[1..] {
            let envelope: serde_json::Value = serde_json::from_slice(body).expect("envelope");
            assert_eq!(envelope["data"]["new_event_count"], 1);
            assert_eq!(envelope["data"]["completeness"], "truncated");
        }
    }

    fn wh08_target(
        id: &str,
        tenants: Vec<String>,
        repos: Vec<String>,
    ) -> (
        crate::config::StorageEventsTargetConfig,
        crate::jupiter::service::storage_event_transport::EventTarget,
    ) {
        let config = crate::config::StorageEventsTargetConfig {
            id: id.to_owned(),
            url: "https://events.example.invalid/ingest".to_owned(),
            secret_ref: format!("vault://secret/config/it/storage_events/targets/{id}/hmac#value"),
            events: vec!["agent_capture.checkpoint.committed".to_owned()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: false,
            agent_tenants: tenants,
            agent_repo_paths: repos,
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

    fn wh08_emitter(
        transport: std::sync::Arc<RecordingTransport>,
        targets: Vec<(
            crate::config::StorageEventsTargetConfig,
            crate::jupiter::service::storage_event_transport::EventTarget,
        )>,
    ) -> StorageEventEmitter {
        let mut config =
            crate::config::testing::isolated_config(std::env::temp_dir().join("wh08-checkpoint"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some("it-wh08".to_owned());
        StorageEventEmitter::new_with_transport(&config, transport, targets)
    }

    async fn insert_committed_raw(storage: &AgentCaptureStorage, capture_id: i64, digest: &str) {
        agent_capture_blob::Entity::insert(agent_capture_blob::ActiveModel {
            deployment_id: Set("default".to_owned()),
            tenant_id: Set("default".to_owned()),
            digest: Set(digest.to_owned()),
            visibility: Set("raw".to_owned()),
            object_key: Set(format!("default/default/raw/{digest}")),
            size_bytes: Set(1),
            lease_state: Set("committed".to_owned()),
            lease_generation: Set(0),
            capture_id: Set(Some(capture_id)),
            ..Default::default()
        })
        .exec(storage.get_connection())
        .await
        .expect("insert committed raw");
    }

    #[tokio::test]
    async fn storage_event_checkpoint_incomplete() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        let transport = std::sync::Arc::new(RecordingTransport::default());
        let emitter = wh08_emitter(
            transport.clone(),
            vec![
                wh08_target(
                    "ops-main",
                    vec!["default".to_owned()],
                    vec!["/third-part/mega".to_owned()],
                ),
                wh08_target(
                    "other-tenant",
                    vec!["other".to_owned()],
                    vec!["/third-part/mega".to_owned()],
                ),
                wh08_target(
                    "other-repo",
                    vec!["default".to_owned()],
                    vec!["/other/repo".to_owned()],
                ),
            ],
        );
        let service = AgentCaptureService {
            storage: AgentCaptureStorage {
                base: BaseStorage::new(std::sync::Arc::new(db)),
            },
            obj_storage: mock_object_storage(),
            storage_event_emitter: emitter,
        };
        let capture_id = service
            .storage
            .upsert_session(SessionNaturalKey {
                deployment_id: "default".to_owned(),
                tenant_id: "default".to_owned(),
                repo_id: "/third-part/mega".to_owned(),
                producer_id: "hook".to_owned(),
                session_kind: "external_capture".to_owned(),
                client_session_id: "provider__wh08".to_owned(),
            })
            .await
            .expect("session");
        insert_committed_raw(&service.storage, capture_id, "sha256:shared-raw").await;

        let first = service
            .commit_checkpoint(
                capture_id,
                &InsertCheckpoint {
                    checkpoint_id: "cp-1".to_owned(),
                    transcript_digest: Some("sha256:shared-raw".to_owned()),
                    redacted_digest: None,
                    metadata: None,
                },
                "fp-1",
            )
            .await
            .expect("first");
        assert!(first.created);
        assert!(first.raw_committed);
        wh07_wait_calls(&transport, 1).await;
        assert_eq!(transport.calls(), 1, "exactly one matching target");
        assert_eq!(transport.target_ids(), vec!["ops-main".to_owned()]);
        let envelope: serde_json::Value =
            serde_json::from_slice(&transport.bodies()[0]).expect("envelope");
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["event_type"], "agent_capture.checkpoint.committed");
        assert_eq!(envelope["source"], "agent_capture");
        assert_eq!(envelope["scope"]["tenant_id"], "default");
        assert_eq!(envelope["scope"]["repo_path"], "/third-part/mega");
        assert!(envelope["scope"]["oci_repository"].is_null());
        assert_eq!(envelope["data"]["capture_id"], capture_id.to_string());
        assert_eq!(envelope["data"]["checkpoint_id"], "cp-1");
        assert_eq!(envelope["data"]["completeness"], first.completeness);
        assert_eq!(envelope["data"]["raw_committed"], true);

        let mut session = agent_capture_session::Entity::find_by_id(capture_id)
            .one(service.storage.get_connection())
            .await
            .expect("load")
            .expect("session");
        session.completeness = "truncated".to_owned();
        let mut active = session.into_active_model();
        active.completeness = sea_orm::Set("truncated".to_owned());
        active
            .update(service.storage.get_connection())
            .await
            .expect("mutate latest completeness");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let after: serde_json::Value =
            serde_json::from_slice(&transport.bodies()[0]).expect("frozen envelope");
        assert_eq!(
            after["data"]["completeness"], first.completeness,
            "delayed send must not re-read session latest"
        );
        assert_ne!(
            after["data"]["completeness"], "truncated",
            "frozen snapshot must not pick up the post-commit latest mutation"
        );

        let replay = service
            .commit_checkpoint(
                capture_id,
                &InsertCheckpoint {
                    checkpoint_id: "cp-1".to_owned(),
                    transcript_digest: Some("sha256:shared-raw".to_owned()),
                    redacted_digest: None,
                    metadata: None,
                },
                "fp-1",
            )
            .await
            .expect("replay");
        assert!(!replay.created);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "replay must not emit");

        let shared = service
            .commit_checkpoint(
                capture_id,
                &InsertCheckpoint {
                    checkpoint_id: "cp-2".to_owned(),
                    transcript_digest: Some("sha256:shared-raw".to_owned()),
                    redacted_digest: None,
                    metadata: None,
                },
                "fp-2",
            )
            .await
            .expect("shared blob");
        assert!(shared.created);
        assert!(shared.raw_committed);
        wh07_wait_calls(&transport, 2).await;

        let incomplete = service
            .commit_checkpoint(
                capture_id,
                &InsertCheckpoint {
                    checkpoint_id: "cp-redacted".to_owned(),
                    transcript_digest: None,
                    redacted_digest: Some("sha256:redacted-only".to_owned()),
                    metadata: None,
                },
                "fp-redacted",
            )
            .await
            .expect("redacted");
        assert!(incomplete.created);
        assert!(!incomplete.raw_committed);
        assert_eq!(incomplete.completeness, "incomplete");
        wh07_wait_calls(&transport, 3).await;
        assert_eq!(transport.calls(), 3, "three new checkpoints, zero replays");
        assert!(
            transport.target_ids().iter().all(|id| id == "ops-main"),
            "AND isolation: only the matching target is selected"
        );
        let last: serde_json::Value =
            serde_json::from_slice(&transport.bodies()[2]).expect("incomplete envelope");
        assert_eq!(last["data"]["checkpoint_id"], "cp-redacted");
        assert_eq!(last["data"]["completeness"], "incomplete");
        assert_eq!(last["data"]["raw_committed"], false);
    }
}
