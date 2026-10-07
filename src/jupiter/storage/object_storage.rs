use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use reqwest::Method;

pub use crate::orbit_api::factory::MegaObjectStorageWrapper;
use crate::{common::errors::MegaError, config::ObjectStorageConfig};
#[rustfmt::skip]
use crate::orbit_api::{
    error::{IoOrbitError, OrbitResult},
    log_storage::{LogManifest, LogStorage},
    object_storage::{MegaObjectStorage, ObjectByteStream, ObjectKey, ObjectMeta},
};

/// Build object storage for `cfg` via the inlined orbit factory.
pub async fn build_object_storage(
    cfg: &ObjectStorageConfig,
) -> Result<MegaObjectStorageWrapper, MegaError> {
    crate::orbit::factory::ObjectStorageFactory::build(cfg)
        .await
        .map_err(|e| {
            let redacted = crate::config::redaction::global_redactor().redact(&e.to_string());
            tracing::warn!(
                endpoint = %crate::config::redaction::redact_object_storage_endpoint(
                    &cfg.s3.endpoint_url
                ),
                access_key = %crate::config::redaction::redact_secret_value(&cfg.s3.access_key_id),
                "object storage build failed"
            );
            MegaError::Other(format!("object storage build failed: {redacted}"))
        })
}

#[cfg(test)]
mod exact_range_tests {
    use super::*;
    use crate::orbit_api::object_storage::ObjectNamespace;

    #[tokio::test]
    async fn memory_exact_range_has_no_clipped_or_empty_success() {
        let storage = mock_object_storage();
        let key = ObjectKey {
            namespace: ObjectNamespace::Git,
            key: "abcdef1234567890".to_string(),
        };
        storage
            .inner
            .put_stream(
                &key,
                Box::pin(futures::stream::iter([Ok(Bytes::from_static(b"abcdef"))])),
                ObjectMeta {
                    size: 6,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let (mut stream, meta) = storage
            .inner
            .get_range_stream_exact(&key, 2, 5)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.size, 6);
        assert_eq!(stream.next().await.unwrap().unwrap(), b"cde".as_slice());
        assert!(stream.next().await.is_none());
        for (start, end) in [(5, 7), (6, 7), (4, 4), (u64::MAX, u64::MAX)] {
            assert!(
                storage
                    .inner
                    .get_range_stream_exact(&key, start, end)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn memory_chunk_receipts_reject_all_mutation_routes() {
        let storage = mock_object_storage();
        super::assert_immutable_chunk_receipt_contract(storage.inner.as_ref()).await;
    }
}

#[cfg(test)]
pub(crate) async fn assert_immutable_chunk_receipt_contract(
    storage: &dyn crate::orbit_api::factory::MegaObjectStorageWithLog,
) {
    let key = ObjectKey {
        namespace: crate::orbit_api::object_storage::ObjectNamespace::ChunkMapReceipt,
        key: "a".repeat(64),
    };
    let first = Bytes::from_static(b"trusted immutable receipt");
    storage
        .put_metadata_atomic_create(&key, first.clone(), ObjectMeta::default())
        .await
        .unwrap();
    storage
        .put_metadata_atomic_create(
            &key,
            Bytes::from_static(b"conflicting replay"),
            ObjectMeta::default(),
        )
        .await
        .unwrap();
    let input = || {
        Box::pin(futures::stream::iter([Ok(Bytes::from_static(
            b"overwrite",
        ))])) as ObjectByteStream
    };
    assert!(
        storage
            .put_stream(&key, input(), ObjectMeta::default())
            .await
            .is_err()
    );
    assert!(
        storage
            .put_stream_bounded(&key, input(), ObjectMeta::default())
            .await
            .is_err()
    );
    assert!(
        storage
            .put_metadata_atomic(
                &key,
                Bytes::from_static(b"overwrite"),
                ObjectMeta::default()
            )
            .await
            .is_err()
    );
    assert!(
        storage
            .put_metadata_atomic_create(
                &key,
                Bytes::from(vec![
                    0;
                    crate::orbit_api::object_storage::MAX_METADATA_ATOMIC_BYTES
                        + 1
                ]),
                ObjectMeta::default()
            )
            .await
            .is_err()
    );
    assert!(
        storage
            .append(&key, input(), ObjectMeta::default())
            .await
            .is_err()
    );
    assert!(
        storage
            .append_concurrently(&key, input(), ObjectMeta::default())
            .await
            .is_err()
    );
    assert!(storage.delete(&key).await.is_err());
    for method in [Method::PUT, Method::POST, Method::DELETE, Method::PATCH] {
        assert!(
            storage
                .signed_url(&key, method, std::time::Duration::from_secs(60))
                .await
                .is_err()
        );
    }
    assert!(
        storage
            .signed_url(&key, Method::GET, std::time::Duration::from_secs(60))
            .await
            .is_ok()
    );
    let (mut stream, meta) = storage.get_stream(&key).await.unwrap();
    assert_eq!(meta.size, first.len() as i64);
    let mut observed = Vec::new();
    while let Some(part) = stream.next().await {
        observed.extend_from_slice(&part.unwrap());
    }
    assert_eq!(observed, first.as_ref());
}

#[derive(Default)]
struct InMemoryObjectStorage {
    objects: Mutex<HashMap<ObjectKey, (Bytes, ObjectMeta)>>,
}

impl InMemoryObjectStorage {
    async fn read_stream(mut data: ObjectByteStream) -> OrbitResult<Bytes> {
        let mut buf = BytesMut::new();
        while let Some(chunk) = data.next().await {
            buf.extend_from_slice(&chunk?);
        }
        Ok(buf.freeze())
    }

    fn stream_bytes(bytes: Bytes) -> ObjectByteStream {
        Box::pin(futures::stream::once(async move { Ok(bytes) }))
    }
}

pub fn mock_object_storage() -> MegaObjectStorageWrapper {
    MegaObjectStorageWrapper::new(Arc::new(InMemoryObjectStorage::default()))
}

#[async_trait::async_trait]
impl MegaObjectStorage for InMemoryObjectStorage {
    async fn put_stream(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        mut meta: ObjectMeta,
    ) -> OrbitResult<()> {
        reject_receipt_mutation(key)?;
        let bytes = Self::read_stream(data).await?;
        meta.size = bytes.len() as i64;
        self.objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .insert(key.clone(), (bytes, meta));
        Ok(())
    }

    async fn put_metadata_atomic(
        &self,
        key: &ObjectKey,
        bytes: Bytes,
        mut meta: ObjectMeta,
    ) -> OrbitResult<()> {
        reject_receipt_mutation(key)?;
        use crate::orbit_api::object_storage::MAX_METADATA_ATOMIC_BYTES;
        if bytes.len() > MAX_METADATA_ATOMIC_BYTES {
            return Err(IoOrbitError::Other(format!(
                "atomic metadata put exceeds {MAX_METADATA_ATOMIC_BYTES} byte limit"
            )));
        }
        meta.size = bytes.len() as i64;
        self.objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .insert(key.clone(), (bytes, meta));
        Ok(())
    }

    async fn put_metadata_atomic_create(
        &self,
        key: &ObjectKey,
        bytes: Bytes,
        mut meta: ObjectMeta,
    ) -> OrbitResult<()> {
        if bytes.len() > crate::orbit_api::object_storage::MAX_METADATA_ATOMIC_BYTES {
            return Err(IoOrbitError::Other(
                "immutable atomic metadata exceeds its byte limit".into(),
            ));
        }
        key.validate()?;
        meta.size = bytes.len() as i64;
        self.objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".into()))?
            .entry(key.clone())
            .or_insert((bytes, meta));
        Ok(())
    }

    async fn get_stream(&self, key: &ObjectKey) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
        let (bytes, meta) = self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .get(key)
            .cloned()
            .ok_or_else(|| IoOrbitError::object_store_not_found(key.default_sharding()))?;
        Ok((Self::stream_bytes(bytes), meta))
    }

    async fn get_range_stream(
        &self,
        key: &ObjectKey,
        start: u64,
        end: Option<u64>,
    ) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
        let (bytes, meta) = self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .get(key)
            .cloned()
            .ok_or_else(|| IoOrbitError::object_store_not_found(key.default_sharding()))?;
        let start = start as usize;
        let end = end.map_or(bytes.len(), |end| end as usize).min(bytes.len());
        if start > end || start > bytes.len() {
            return Err(IoOrbitError::object_store("invalid object range"));
        }
        Ok((Self::stream_bytes(bytes.slice(start..end)), meta))
    }

    async fn get_range_stream_exact(
        &self,
        key: &ObjectKey,
        start: u64,
        end: u64,
    ) -> OrbitResult<Option<(ObjectByteStream, ObjectMeta)>> {
        let (bytes, meta) = self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .get(key)
            .cloned()
            .ok_or_else(|| IoOrbitError::object_store_not_found(key.default_sharding()))?;
        let start =
            usize::try_from(start).map_err(|_| IoOrbitError::object_store("range overflow"))?;
        let end = usize::try_from(end).map_err(|_| IoOrbitError::object_store("range overflow"))?;
        if start >= end || end > bytes.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "exact object range is outside stored bytes",
            )
            .into());
        }
        Ok(Some((Self::stream_bytes(bytes.slice(start..end)), meta)))
    }

    async fn exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
        Ok(self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .contains_key(key))
    }

    async fn signed_url(
        &self,
        key: &ObjectKey,
        method: Method,
        _expires_in: std::time::Duration,
    ) -> OrbitResult<Option<String>> {
        if method != Method::GET {
            reject_receipt_mutation(key)?;
        }
        Ok(None)
    }

    async fn delete(&self, key: &ObjectKey) -> OrbitResult<()> {
        reject_receipt_mutation(key)?;
        let deleted = self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .remove(key)
            .is_some();
        if deleted {
            Ok(())
        } else {
            Err(IoOrbitError::object_store_not_found(key.default_sharding()))
        }
    }
}

fn reject_receipt_mutation(key: &ObjectKey) -> OrbitResult<()> {
    if key.namespace == crate::orbit_api::object_storage::ObjectNamespace::ChunkMapReceipt {
        Err(IoOrbitError::Other(
            "chunk map receipts require immutable atomic creation".into(),
        ))
    } else {
        Ok(())
    }
}

#[async_trait::async_trait]
impl LogStorage for InMemoryObjectStorage {
    async fn append(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        mut meta: ObjectMeta,
    ) -> OrbitResult<()> {
        reject_receipt_mutation(key)?;
        let bytes = Self::read_stream(data).await?;
        let mut objects = self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?;
        let (existing, _) = objects
            .entry(key.clone())
            .or_insert_with(|| (Bytes::new(), ObjectMeta::default()));
        let mut merged = BytesMut::with_capacity(existing.len() + bytes.len());
        merged.extend_from_slice(existing);
        merged.extend_from_slice(&bytes);
        meta.size = merged.len() as i64;
        *existing = merged.freeze();
        Ok(())
    }

    async fn read_range(
        &self,
        key: &ObjectKey,
        offset: u64,
        length: u64,
    ) -> OrbitResult<ObjectByteStream> {
        let (stream, _) = self
            .get_range_stream(key, offset, Some(offset.saturating_add(length)))
            .await?;
        Ok(stream)
    }

    async fn read_lines_range(
        &self,
        key: &ObjectKey,
        start_line: u64,
        end_line: u64,
    ) -> OrbitResult<ObjectByteStream> {
        let (bytes, _) = self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .get(key)
            .cloned()
            .ok_or_else(|| IoOrbitError::object_store_not_found(key.default_sharding()))?;
        let text = String::from_utf8_lossy(&bytes);
        let selected = text
            .lines()
            .skip(start_line as usize)
            .take(end_line.saturating_sub(start_line) as usize)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Self::stream_bytes(Bytes::from(selected)))
    }

    async fn append_concurrently(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        meta: ObjectMeta,
    ) -> OrbitResult<()> {
        self.append(key, data, meta).await
    }

    async fn load_manifest(&self, key: &ObjectKey) -> OrbitResult<LogManifest> {
        let len = self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .get(key)
            .map(|(bytes, _)| bytes.len() as u64)
            .unwrap_or_default();
        Ok(LogManifest {
            len,
            segments: Vec::new(),
        })
    }

    async fn log_exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
        self.exists(key).await
    }
}
