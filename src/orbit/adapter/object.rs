use super::*;

#[async_trait::async_trait]
impl MegaObjectStorage for ObjectStoreAdapter {
    fn supports_presigned_urls(&self) -> bool {
        matches!(&self.store, BackendStore::S3(_) | BackendStore::Gcs(_))
    }

    async fn put_stream(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        _meta: ObjectMeta,
    ) -> OrbitResult<()> {
        reject_receipt_mutation(key)?;
        let path = Self::checked_path(key)?;
        // Artifacts are keyed by UUID and must not depend on LFS/Git upload_strategy
        // (e.g. S3 often uses `Multipart` for LFS while we still need create-if-absent semantics).
        if matches!(key.namespace, ObjectNamespace::Artifact) {
            return self.put_idempotent(&path, data).await;
        }
        match (key.namespace, &self.upload_strategy) {
            (ObjectNamespace::Git, UploadStrategy::SinglePut) => {
                self.put_idempotent(&path, data).await
            }
            _ => match self.upload_strategy {
                UploadStrategy::Multipart => self.put_multipart(&path, data).await,
                UploadStrategy::SinglePut => self.put_single(&path, data).await,
            },
        }
    }

    async fn put_stream_bounded(
        &self,
        key: &ObjectKey,
        data: ObjectByteStream,
        _meta: ObjectMeta,
    ) -> OrbitResult<()> {
        reject_receipt_mutation(key)?;
        let path = Self::checked_path(key)?;
        self.put_multipart(&path, data).await
    }

    async fn put_metadata_atomic(
        &self,
        key: &ObjectKey,
        bytes: Bytes,
        _meta: ObjectMeta,
    ) -> OrbitResult<()> {
        reject_receipt_mutation(key)?;
        use crate::orbit_api::object_storage::MAX_METADATA_ATOMIC_BYTES;
        if bytes.len() > MAX_METADATA_ATOMIC_BYTES {
            return Err(IoOrbitError::Other(format!(
                "atomic metadata put exceeds {MAX_METADATA_ATOMIC_BYTES} byte limit"
            )));
        }
        let path = Self::checked_path(key)?;
        // Single complete-object PUT (Local/S3/GCS via object_store). Never
        // multipart: partial parts must not become edge-visible.
        self.to_store()
            .put(&path, PutPayload::from_bytes(bytes))
            .await
            .map_err(IoOrbitError::from)?;
        Ok(())
    }

    async fn put_metadata_atomic_create(
        &self,
        key: &ObjectKey,
        bytes: Bytes,
        _meta: ObjectMeta,
    ) -> OrbitResult<()> {
        if bytes.len() > crate::orbit_api::object_storage::MAX_METADATA_ATOMIC_BYTES {
            return Err(IoOrbitError::Other(
                "immutable atomic metadata exceeds its byte limit".into(),
            ));
        }
        let path = Self::checked_path(key)?;
        match self
            .to_store()
            .put_opts(
                &path,
                PutPayload::from_bytes(bytes),
                PutOptions::from(PutMode::Create),
            )
            .await
        {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(error) => Err(IoOrbitError::from(error)),
        }
    }

    async fn get_stream(&self, key: &ObjectKey) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
        let path = Self::checked_path(key)?;

        let res = self
            .to_store()
            .get(&path)
            .await
            .map_err(IoOrbitError::from)?;
        let meta = build_object_meta(&res.meta);
        let stream = res.into_stream().map_err(std::io::Error::other);

        Ok((Box::pin(stream), meta))
    }

    async fn get_range_stream(
        &self,
        key: &ObjectKey,
        start: u64,
        end: Option<u64>,
    ) -> OrbitResult<(ObjectByteStream, ObjectMeta)> {
        let path = Self::checked_path(key)?;

        // A single `get_opts` with a range returns both the ranged body stream
        // AND the full object's metadata (size/etag/version). This avoids a
        // separate `head()` and keeps the read streaming instead of fully
        // buffering the range.
        let range = match end {
            Some(end) => object_store::GetRange::Bounded(start..end),
            None => object_store::GetRange::Offset(start),
        };
        let opts = object_store::GetOptions {
            range: Some(range),
            ..Default::default()
        };
        let res = self
            .to_store()
            .get_opts(&path, opts)
            .await
            .map_err(IoOrbitError::from)?;
        let meta = build_object_meta(&res.meta);
        let stream = res.into_stream().map_err(std::io::Error::other);

        Ok((Box::pin(stream), meta))
    }

    async fn get_range_stream_exact(
        &self,
        key: &ObjectKey,
        start: u64,
        end: u64,
    ) -> OrbitResult<Option<(ObjectByteStream, ObjectMeta)>> {
        if start >= end {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid exact object range",
            )
            .into());
        }
        let path = Self::checked_path(key)?;
        let res = self
            .to_store()
            .get_opts(
                &path,
                object_store::GetOptions {
                    range: Some(object_store::GetRange::Bounded(start..end)),
                    ..Default::default()
                },
            )
            .await
            .map_err(IoOrbitError::from)?;
        if res.range != (start..end) || end > res.meta.size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "backend returned a different object range",
            )
            .into());
        }
        let meta = build_object_meta(&res.meta);
        let stream = res.into_stream().map_err(std::io::Error::other);
        Ok(Some((Box::pin(stream), meta)))
    }

    async fn signed_url(
        &self,
        key: &ObjectKey,
        method: Method,
        expires_in: Duration,
    ) -> OrbitResult<Option<String>> {
        if key.namespace == ObjectNamespace::ChunkMapReceipt && method != Method::GET {
            return Err(IoOrbitError::Other(
                "immutable chunk map receipts do not permit presigned mutation".into(),
            ));
        }
        let path = Self::checked_path(key)?;

        let url = match &self.store {
            BackendStore::S3(s3) => Some(
                self.presign_store
                    .as_ref()
                    .unwrap_or(s3)
                    .signed_url(method, &path, expires_in)
                    .await
                    .map_err(IoOrbitError::from)?
                    .to_string(),
            ),
            BackendStore::Gcs(gcs) => Some(
                gcs.signed_url(method, &path, expires_in)
                    .await
                    .map_err(IoOrbitError::from)?
                    .to_string(),
            ),
            BackendStore::Local(_) => None,
        };
        Ok(url)
    }

    async fn exists(&self, key: &ObjectKey) -> OrbitResult<bool> {
        let path = Self::checked_path(key)?;
        head_result_to_exists(self.to_store().head(&path).await)
    }

    async fn delete(&self, key: &ObjectKey) -> OrbitResult<()> {
        reject_receipt_mutation(key)?;
        let path = Self::checked_path(key)?;
        self.to_store()
            .delete(&path)
            .await
            .map_err(IoOrbitError::from)?;
        Ok(())
    }
}

pub(super) fn reject_receipt_mutation(key: &ObjectKey) -> OrbitResult<()> {
    if key.namespace == ObjectNamespace::ChunkMapReceipt {
        Err(IoOrbitError::Other(
            "chunk map receipts require immutable atomic creation".into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod exact_range_tests {
    use super::*;

    #[tokio::test]
    async fn local_chunk_receipts_reject_all_mutation_routes() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = ObjectStoreAdapter {
            store: BackendStore::Local(Arc::new(
                LocalFileSystem::new_with_prefix(directory.path()).unwrap(),
            )),
            upload_strategy: UploadStrategy::SinglePut,
            presign_store: None,
        };
        crate::jupiter::storage::object_storage::assert_immutable_chunk_receipt_contract(&adapter)
            .await;
    }

    #[tokio::test]
    async fn local_exact_range_returns_selected_bytes_and_full_object_size_without_clipping() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = ObjectStoreAdapter {
            store: BackendStore::Local(Arc::new(
                LocalFileSystem::new_with_prefix(directory.path()).unwrap(),
            )),
            upload_strategy: UploadStrategy::SinglePut,
            presign_store: None,
        };
        let key = ObjectKey {
            namespace: ObjectNamespace::Git,
            key: "abcdef1234567890".to_string(),
        };
        adapter
            .put_stream(
                &key,
                Box::pin(stream::iter([Ok(Bytes::from_static(b"0123456789"))])),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let (input, meta) = adapter
            .get_range_stream_exact(&key, 3, 7)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.size, 10);
        assert_eq!(
            ObjectStoreAdapter::buffer_stream(input, 4).await.unwrap(),
            b"3456".as_slice()
        );
        let (input, meta) = adapter
            .get_range_stream_exact(&key, 9, 10)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.size, 10);
        assert_eq!(
            ObjectStoreAdapter::buffer_stream(input, 1).await.unwrap(),
            b"9".as_slice()
        );
        assert!(adapter.get_range_stream_exact(&key, 9, 11).await.is_err());
        assert!(adapter.get_range_stream_exact(&key, 5, 5).await.is_err());
        assert!(adapter.get_range_stream_exact(&key, 10, 11).await.is_err());
    }
}
