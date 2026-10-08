use super::*;

#[async_trait::async_trait]
impl MegaObjectStorage for ObjectStoreAdapter {
    fn supports_presigned_urls(&self) -> bool {
        matches!(&self.store, BackendStore::S3(_) | BackendStore::Gcs(_))
    }

    fn supports_chunk_map_retention(&self) -> bool {
        matches!(&self.store, BackendStore::Local(_))
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

        Ok((
            crate::orbit_api::object_storage::fragment_object_stream(Box::pin(stream)),
            meta,
        ))
    }

    async fn chunk_map_receipt_inventory(
        &self,
    ) -> OrbitResult<crate::orbit_api::object_storage::ChunkMapReceiptInventory> {
        use crate::orbit_api::object_storage::{
            ChunkMapReceiptInventory, MAX_CHUNK_MAP_RECEIPT_BYTES, MAX_CHUNK_MAP_RECEIPTS,
        };
        // object_store 0.14 cannot inventory/delete retained cloud versions.
        // A current-object listing is not a physical backing-byte quota.
        if !self.supports_chunk_map_retention() {
            return Err(IoOrbitError::ChunkMapRetentionUnsupported);
        }
        let prefix = object_store::path::Path::from("chunk-map-receipt");
        let mut listing = self.to_store().list(Some(&prefix));
        let mut inventory = ChunkMapReceiptInventory {
            objects: Vec::new(),
            bytes: 0,
        };
        while let Some(entry) = listing.next().await {
            let meta = entry.map_err(IoOrbitError::from)?;
            if inventory.objects.len() == MAX_CHUNK_MAP_RECEIPTS {
                return Err(IoOrbitError::ChunkMapRetentionCapacityExceeded);
            }
            inventory.bytes = inventory
                .bytes
                .checked_add(meta.size)
                .filter(|bytes| *bytes <= MAX_CHUNK_MAP_RECEIPT_BYTES)
                .ok_or(IoOrbitError::ChunkMapRetentionCapacityExceeded)?;
            let location = meta.location.as_ref();
            let parts: Vec<_> = location.split('/').collect();
            if parts.len() != 5
                || parts[0] != "chunk-map-receipt"
                || parts[1..4].iter().any(|part| part.len() != 2)
                || parts[4].len() > 128
            {
                return Err(IoOrbitError::Other(
                    "invalid physical chunk-map receipt path".into(),
                ));
            }
            let key = ObjectKey {
                namespace: ObjectNamespace::ChunkMapReceipt,
                key: parts[1..].concat(),
            };
            key.validate()?;
            if key.default_sharding() != location {
                return Err(IoOrbitError::Other(
                    "noncanonical physical chunk-map receipt path".into(),
                ));
            }
            inventory.objects.push((key, meta.size));
        }
        Ok(inventory)
    }

    async fn delete_chunk_map_receipt(
        &self,
        authority: &crate::orbit_api::object_storage::ChunkMapReceiptDeletion,
    ) -> OrbitResult<bool> {
        if !self.supports_chunk_map_retention() {
            return Err(IoOrbitError::ChunkMapRetentionUnsupported);
        }
        let key = authority.key();
        if key.namespace != ObjectNamespace::ChunkMapReceipt {
            return Err(IoOrbitError::Other(
                "invalid sealed receipt namespace".into(),
            ));
        }
        let (mut stream, meta) = match self.get_stream(key).await {
            Ok(object) => object,
            Err(error) if error.is_not_found() => return Ok(false),
            Err(error) => return Err(error),
        };
        let expected = authority.expected_bytes();
        if meta.size != expected.len() as i64 {
            return Err(IoOrbitError::Other(
                "retired receipt body size changed".into(),
            ));
        }
        let mut offset: usize = 0;
        while let Some(part) = stream.next().await {
            let bytes = part?;
            let end = offset
                .checked_add(bytes.len())
                .filter(|end| *end <= expected.len())
                .ok_or_else(|| {
                    IoOrbitError::Other("retired receipt body exceeds its exact profile".into())
                })?;
            if expected[offset..end] != bytes[..] {
                return Err(IoOrbitError::Other("retired receipt body changed".into()));
            }
            offset = end;
        }
        if offset != expected.len() {
            return Err(IoOrbitError::Other(
                "retired receipt body is truncated".into(),
            ));
        }
        // Physical generation keys are never reused. A late create can only
        // restore this same retired body; persistent history will reconcile it.
        match self.to_store().delete(&Self::checked_path(key)?).await {
            Ok(()) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(IoOrbitError::from(error)),
        }
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
    async fn backend_chunk_retention_capabilities_match_cold_admission_without_cloud_io() {
        let s3 = object_store::aws::AmazonS3Builder::new()
            .with_bucket_name("capabilities-test")
            .with_region("us-east-1")
            .with_access_key_id("fixture-access")
            .with_secret_access_key("fixture-secret")
            .with_skip_signature(true)
            .build()
            .unwrap();
        let gcs = object_store::gcp::GoogleCloudStorageBuilder::new()
            .with_bucket_name("capabilities-test")
            .with_credentials(Arc::new(object_store::StaticCredentialProvider::new(
                object_store::gcp::GcpCredential {
                    bearer: "fixture-bearer".into(),
                },
            )))
            .with_skip_signature(true)
            .build()
            .unwrap();
        for store in [
            BackendStore::S3(Arc::new(s3)),
            BackendStore::Gcs(Arc::new(gcs)),
        ] {
            let backend = crate::orbit_api::factory::MegaObjectStorageWrapper::new(Arc::new(
                ObjectStoreAdapter {
                    store,
                    upload_strategy: UploadStrategy::SinglePut,
                    presign_store: None,
                },
            ));
            assert!(!backend.supports_chunk_map_retention());
            assert!(matches!(
                backend.inner.chunk_map_receipt_inventory().await,
                Err(IoOrbitError::ChunkMapRetentionUnsupported)
            ));
        }
    }

    #[tokio::test]
    async fn local_whole_stream_keeps_large_file_size_and_digest_with_bounded_fragments() {
        use sha2::{Digest, Sha256};

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
            key: "ab".repeat(20),
        };
        let raw = Bytes::from(vec![
            0xff;
            crate::orbit_api::object_storage::OBJECT_STREAM_ITEM_BYTES
                + 113
        ]);
        let expected: [u8; 32] = Sha256::digest(&raw).into();
        let size = raw.len();
        adapter
            .put_stream(
                &key,
                Box::pin(stream::iter([Ok(raw)])),
                ObjectMeta::default(),
            )
            .await
            .unwrap();
        let (mut input, meta) = adapter.get_stream(&key).await.unwrap();
        assert_eq!(meta.size, size as i64);
        assert!(adapter.supports_chunk_map_retention());
        let mut received = 0;
        let mut hashed = Sha256::new();
        while let Some(part) = input.next().await {
            let part = part.unwrap();
            assert!(part.len() <= crate::orbit_api::object_storage::OBJECT_STREAM_ITEM_BYTES);
            received += part.len();
            hashed.update(&part);
        }
        assert_eq!(received, size);
        assert_eq!(<[u8; 32]>::from(hashed.finalize()), expected);
    }

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
