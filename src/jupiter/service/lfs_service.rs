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

    #[cfg(feature = "fastcdc")]
    pub fn media(&self) -> crate::ceres::lfs::media::service::MediaService {
        crate::ceres::lfs::media::service::MediaService::new(self.obj_storage.clone())
    }
}
