use crate::jupiter::storage::{
    base_storage::{BaseStorage, StorageConnector},
    lfs_db_storage::LfsDbStorage,
    object_storage::{MegaObjectStorageWrapper, mock_object_storage},
};

#[derive(Clone)]
pub struct LfsService {
    pub lfs_storage: LfsDbStorage,
    pub obj_storage: MegaObjectStorageWrapper,
    /// The application emitter owner handle (WH-05): disabled by default and
    /// rebound to the real transport by `Storage::set_storage_event_emitter`.
    pub storage_event_emitter: crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
}

impl LfsService {
    pub fn mock() -> Self {
        let mock = BaseStorage::mock();

        Self {
            lfs_storage: LfsDbStorage { base: mock.clone() },
            obj_storage: mock_object_storage(),
            storage_event_emitter:
                crate::jupiter::service::storage_event_emitter::StorageEventEmitter::disabled(),
        }
    }

    #[cfg(feature = "fastcdc")]
    pub fn media(
        &self,
        paging: crate::jupiter::storage::media_paging_storage::MediaPagingStorage,
    ) -> crate::ceres::lfs::media::service::MediaService {
        crate::ceres::lfs::media::service::MediaService::new(self.obj_storage.clone(), paging)
    }
}
