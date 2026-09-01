#[cfg(feature = "fastcdc")]
use bytes::Bytes;

#[cfg(feature = "fastcdc")]
use crate::ceres::lfs::media::{
    finalize,
    protocol::{ManifestResponse, MediaManifest, PrepareResponse},
    scope::MediaScope,
    service::{self, MediaServiceError},
};
use crate::jupiter::storage::{
    base_storage::{BaseStorage, StorageConnector},
    lfs_db_storage::LfsDbStorage,
    object_storage::{MegaObjectStorageWrapper, mock_object_storage},
};

#[derive(Clone)]
pub struct LfsService {
    pub lfs_storage: LfsDbStorage,
    pub obj_storage: MegaObjectStorageWrapper,
}

impl LfsService {
    pub fn mock() -> Self {
        let mock = BaseStorage::mock();

        Self {
            lfs_storage: LfsDbStorage { base: mock.clone() },
            obj_storage: mock_object_storage(),
        }
    }
}

#[cfg(feature = "fastcdc")]
impl LfsService {
    pub(crate) async fn prepare_media(
        &self,
        scope: &MediaScope,
        manifest: MediaManifest,
    ) -> Result<PrepareResponse, MediaServiceError> {
        service::prepare(self, scope, manifest).await
    }

    pub(crate) async fn upload_media_chunk(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        hash: &str,
        data: Bytes,
    ) -> Result<(), MediaServiceError> {
        service::upload_chunk(self, scope, manifest_id, hash, data).await
    }

    pub(crate) async fn pending_media_manifest(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
    ) -> Result<MediaManifest, MediaServiceError> {
        service::pending_manifest(self, scope, manifest_id).await
    }

    pub(crate) async fn read_pending_media_chunk(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
        hash: &str,
    ) -> Result<Bytes, MediaServiceError> {
        service::read_pending_chunk(self, scope, manifest_id, hash).await
    }

    pub(crate) async fn finalize_media(
        &self,
        scope: &MediaScope,
        manifest_id: &str,
    ) -> Result<(), MediaServiceError> {
        finalize::finalize(self, scope, manifest_id).await
    }

    pub(crate) async fn finalized_media_manifest(
        &self,
        scope: &MediaScope,
        media_oid: &str,
    ) -> Result<ManifestResponse, MediaServiceError> {
        finalize::finalized_manifest(self, scope, media_oid).await
    }
}
