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
        // Do not inherit SinglePut: bounded writes must use the adapter's
        // multipart publication path for every concrete backend.
        let path = Self::checked_path(key)?;
        self.put_multipart_bounded(&path, data).await
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

    async fn signed_url(
        &self,
        key: &ObjectKey,
        method: Method,
        expires_in: Duration,
    ) -> OrbitResult<Option<String>> {
        let path = Self::checked_path(key)?;

        let url = match &self.store {
            BackendStore::S3(s3) => Some(
                s3.signed_url(method, &path, expires_in)
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
        let path = Self::checked_path(key)?;
        self.to_store()
            .delete(&path)
            .await
            .map_err(IoOrbitError::from)?;
        Ok(())
    }
}
