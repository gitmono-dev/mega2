use std::io;

use futures::{StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};

use crate::{
    common::errors::MegaError,
    jupiter::storage::{
        base_storage::{BaseStorage, StorageConnector},
        object_storage::{MegaObjectStorageWrapper, mock_object_storage},
        oci_db_storage::OciDbStorage,
    },
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};

#[derive(Clone)]
pub struct OciService {
    pub oci_storage: OciDbStorage,
    pub obj_storage: MegaObjectStorageWrapper,
}

impl OciService {
    pub fn mock() -> Self {
        let mock = BaseStorage::mock();
        Self {
            oci_storage: OciDbStorage { base: mock },
            obj_storage: mock_object_storage(),
        }
    }

    fn blob_key(hex: &str) -> ObjectKey {
        Self::key(format!("blobs/sha256/{hex}"))
    }

    fn manifest_key(hex: &str) -> ObjectKey {
        Self::key(format!("manifests/sha256/{hex}"))
    }

    fn chunk_key(uuid: &str, sequence: u64) -> ObjectKey {
        Self::key(format!("uploads/{uuid}/{sequence}"))
    }

    fn key(key: String) -> ObjectKey {
        ObjectKey {
            namespace: ObjectNamespace::Oci,
            key,
        }
    }

    fn digest_hex(digest: &str) -> Result<&str, MegaError> {
        digest
            .strip_prefix("sha256:")
            .filter(|hex| !hex.is_empty())
            .ok_or_else(|| MegaError::Other("OCI digest must use the sha256:<hex> format".into()))
    }

    fn stream_error(error: MegaError) -> io::Error {
        io::Error::other(error.to_string())
    }

    pub async fn put_blob(&self, hex: &str, stream: ObjectByteStream) -> Result<(), MegaError> {
        self.obj_storage
            .inner
            .put_stream(&Self::blob_key(hex), stream, ObjectMeta::default())
            .await?;
        Ok(())
    }

    pub async fn get_blob(&self, hex: &str) -> Result<(ObjectByteStream, ObjectMeta), MegaError> {
        Ok(self
            .obj_storage
            .inner
            .get_stream(&Self::blob_key(hex))
            .await?)
    }

    pub async fn get_blob_range(
        &self,
        hex: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(ObjectByteStream, ObjectMeta), MegaError> {
        let key = Self::blob_key(hex);
        let (stream, meta) = self.obj_storage.inner.get_stream(&key).await?;
        if start >= meta.size.max(0) as u64 {
            return Ok((Box::pin(futures::stream::empty()), meta));
        }
        drop(stream);
        Ok(self
            .obj_storage
            .inner
            .get_range_stream(&key, start, end)
            .await?)
    }

    pub async fn blob_exists(&self, hex: &str) -> Result<bool, MegaError> {
        Ok(self.obj_storage.inner.exists(&Self::blob_key(hex)).await?)
    }

    pub async fn delete_blob(&self, hex: &str) -> Result<(), MegaError> {
        self.obj_storage.inner.delete(&Self::blob_key(hex)).await?;
        Ok(())
    }

    pub async fn put_manifest(&self, hex: &str, stream: ObjectByteStream) -> Result<(), MegaError> {
        self.obj_storage
            .inner
            .put_stream(&Self::manifest_key(hex), stream, ObjectMeta::default())
            .await?;
        Ok(())
    }

    pub async fn get_manifest(
        &self,
        hex: &str,
    ) -> Result<(ObjectByteStream, ObjectMeta), MegaError> {
        Ok(self
            .obj_storage
            .inner
            .get_stream(&Self::manifest_key(hex))
            .await?)
    }

    pub async fn put_chunk(
        &self,
        uuid: &str,
        sequence: u64,
        stream: ObjectByteStream,
    ) -> Result<(), MegaError> {
        self.obj_storage
            .inner
            .put_stream(
                &Self::chunk_key(uuid, sequence),
                stream,
                ObjectMeta::default(),
            )
            .await?;
        Ok(())
    }

    pub async fn get_chunk_stream(
        &self,
        uuid: &str,
        sequence: u64,
    ) -> Result<ObjectByteStream, MegaError> {
        Ok(self
            .obj_storage
            .inner
            .get_stream(&Self::chunk_key(uuid, sequence))
            .await?
            .0)
    }

    pub async fn delete_chunks(&self, uuid: &str, count: u64) -> Result<(), MegaError> {
        for sequence in 0..count {
            self.obj_storage
                .inner
                .delete(&Self::chunk_key(uuid, sequence))
                .await?;
        }
        Ok(())
    }

    pub async fn finalize_blob(
        &self,
        uuid: &str,
        chunks: u64,
        expected_digest: &str,
    ) -> Result<String, MegaError> {
        let mut hasher = Sha256::new();
        for sequence in 0..chunks {
            let mut stream = self.get_chunk_stream(uuid, sequence).await?;
            while let Some(chunk) = stream.next().await {
                hasher.update(chunk?);
            }
        }
        let actual_digest = format!("sha256:{}", hex::encode(hasher.finalize()));
        if actual_digest != expected_digest {
            return Err(MegaError::Other(format!(
                "OCI blob digest mismatch: expected {expected_digest}, got {actual_digest}"
            )));
        }

        let hex = Self::digest_hex(&actual_digest)?;
        let source = self.clone();
        let uuid = uuid.to_owned();
        let chunk_streams = futures::stream::iter(0..chunks).then(move |sequence| {
            let source = source.clone();
            let uuid = uuid.clone();
            async move {
                source
                    .get_chunk_stream(&uuid, sequence)
                    .await
                    .map_err(Self::stream_error)
            }
        });
        let stream: ObjectByteStream = Box::pin(chunk_streams.try_flatten());
        self.put_blob(hex, stream).await?;
        Ok(actual_digest)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::TryStreamExt;
    use sha2::{Digest, Sha256};

    use super::OciService;
    use crate::jupiter::utils::into_obj_stream::IntoObjectStream;

    async fn collect(stream: crate::orbit_api::object_storage::ObjectByteStream) -> Vec<u8> {
        stream
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            })
            .await
            .expect("stream should be readable")
    }

    #[tokio::test]
    async fn blob_roundtrip() {
        let service = OciService::mock();
        service
            .put_blob("abc123", Bytes::from_static(b"blob data").into_stream())
            .await
            .expect("put blob");

        let (stream, meta) = service.get_blob("abc123").await.expect("get blob");
        assert_eq!(meta.size, 9);
        assert_eq!(collect(stream).await, b"blob data");
        assert!(service.blob_exists("abc123").await.expect("blob exists"));
    }

    #[tokio::test]
    async fn range_boundary() {
        let service = OciService::mock();
        service
            .put_blob("abc123", Bytes::from_static(b"blob").into_stream())
            .await
            .expect("put blob");

        let (stream, meta) = service
            .get_blob_range("abc123", 4, None)
            .await
            .expect("get boundary range");
        assert_eq!(meta.size, 4);
        assert!(collect(stream).await.is_empty());
    }

    #[tokio::test]
    async fn finalize_multipart_sha256() {
        let service = OciService::mock();
        let uuid = "upload-1";
        service
            .put_chunk(uuid, 0, Bytes::from_static(b"hello ").into_stream())
            .await
            .expect("put first chunk");
        service
            .put_chunk(uuid, 1, Bytes::from_static(b"world").into_stream())
            .await
            .expect("put second chunk");
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(b"hello world")));

        assert_eq!(
            service
                .finalize_blob(uuid, 2, &digest)
                .await
                .expect("finalize blob"),
            digest
        );
        let (stream, _) = service
            .get_blob(digest.strip_prefix("sha256:").expect("sha256 prefix"))
            .await
            .expect("get finalized blob");
        assert_eq!(collect(stream).await, b"hello world");
    }

    #[tokio::test]
    async fn finalize_pass_a_rejects() {
        let service = OciService::mock();
        let uuid = "upload-1";
        service
            .put_chunk(uuid, 0, Bytes::from_static(b"actual").into_stream())
            .await
            .expect("put chunk");
        let expected = format!("sha256:{}", hex::encode(Sha256::digest(b"expected")));
        let actual = format!("sha256:{}", hex::encode(Sha256::digest(b"actual")));

        assert!(service.finalize_blob(uuid, 1, &expected).await.is_err());
        assert!(
            !service
                .blob_exists(expected.strip_prefix("sha256:").expect("sha256 prefix"))
                .await
                .expect("check CAS key")
        );
        assert!(
            !service
                .blob_exists(actual.strip_prefix("sha256:").expect("sha256 prefix"))
                .await
                .expect("check actual CAS key")
        );
    }
}
