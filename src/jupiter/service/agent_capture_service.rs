use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use futures::StreamExt;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    callisto::{agent_capture_blob, agent_capture_session},
    common::errors::MegaError,
    jupiter::storage::{
        agent_capture_storage::AgentCaptureStorage,
        base_storage::{BaseStorage, StorageConnector},
        object_storage::{MegaObjectStorageWrapper, mock_object_storage},
    },
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};

#[derive(Clone)]
pub struct AgentCaptureService {
    pub storage: AgentCaptureStorage,
    pub obj_storage: MegaObjectStorageWrapper,
}

impl AgentCaptureService {
    pub fn mock() -> Self {
        Self {
            storage: AgentCaptureStorage {
                base: BaseStorage::mock(),
            },
            obj_storage: mock_object_storage(),
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
        let session = self
            .load_session(capture_id, deployment_id, tenant_id)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("agent_capture_session {capture_id} does not exist"))
            })?;
        let lease_id = Uuid::new_v4().to_string();
        let size = std::sync::Arc::new(AtomicU64::new(0));
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
        let object_key = Self::staging_key(deployment_id, tenant_id, &lease_id).key;
        let expires = Utc::now() + chrono::Duration::seconds(lease_ttl_seconds as i64);
        agent_capture_blob::Entity::insert(agent_capture_blob::ActiveModel {
            deployment_id: Set(session.deployment_id),
            tenant_id: Set(session.tenant_id),
            digest: Set(format!("sha256:staging-{lease_id}")),
            visibility: Set("raw".to_owned()),
            object_key: Set(object_key),
            size_bytes: Set(size_bytes),
            lease_state: Set("staging".to_owned()),
            lease_generation: Set(0),
            lease_id: Set(Some(lease_id.clone())),
            lease_expires_at: Set(Some(expires.into())),
            capture_id: Set(Some(capture_id)),
            upload_intent: Set(Some("stage".to_owned())),
            ..Default::default()
        })
        .exec(self.storage.get_connection())
        .await?;
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
        let (copy_stream, meta) = self.obj_storage.inner.get_stream(&staging).await?;
        self.obj_storage
            .inner
            .put_stream(&committed, copy_stream, meta)
            .await?;
        self.storage
            .finalize_blob_with_session_ref(
                capture_id,
                &digest,
                "raw",
                &committed.key,
                blob.size_bytes,
            )
            .await?;
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

    use super::*;

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
}
