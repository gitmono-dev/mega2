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
        let bytes = Self::read_stream(data).await?;
        meta.size = bytes.len() as i64;
        self.objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .insert(key.clone(), (bytes, meta));
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

    async fn exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
        Ok(self
            .objects
            .lock()
            .map_err(|_| IoOrbitError::Other("object storage lock poisoned".to_string()))?
            .contains_key(key))
    }

    async fn signed_url(
        &self,
        _key: &ObjectKey,
        _method: Method,
        _expires_in: std::time::Duration,
    ) -> OrbitResult<Option<String>> {
        Ok(None)
    }

    async fn delete(&self, key: &ObjectKey) -> OrbitResult<()> {
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

#[async_trait::async_trait]
impl LogStorage for InMemoryObjectStorage {
    async fn append(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        mut meta: ObjectMeta,
    ) -> OrbitResult<()> {
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
