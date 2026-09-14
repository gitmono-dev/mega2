use futures::StreamExt;
use sha2::{Digest, Sha256};

use crate::{
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
