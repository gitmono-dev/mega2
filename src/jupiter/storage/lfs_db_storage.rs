use std::ops::Deref;

use sea_orm::{EntityTrait, InsertResult, IntoActiveModel, Set, TryInsertResult};

use crate::{
    callisto::{lfs_locks, lfs_objects},
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct LfsDbStorage {
    pub base: BaseStorage,
}

impl Deref for LfsDbStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl LfsDbStorage {
    pub async fn new_lfs_object(&self, object: lfs_objects::Model) -> Result<bool, MegaError> {
        let res = lfs_objects::Entity::insert(object.into_active_model())
            .on_conflict_do_nothing()
            .exec(self.get_connection())
            .await?;
        Ok(matches!(res, TryInsertResult::Inserted(_)))
    }

    pub async fn get_lfs_object(&self, oid: &str) -> Result<Option<lfs_objects::Model>, MegaError> {
        let result = lfs_objects::Entity::find_by_id(oid)
            .one(self.get_connection())
            .await?;
        Ok(result)
    }

    pub async fn delete_lfs_object(&self, oid: String) -> Result<(), MegaError> {
        lfs_objects::Entity::delete_by_id(oid)
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn new_lock(
        &self,
        lfs_lock: lfs_locks::Model,
    ) -> Result<InsertResult<lfs_locks::ActiveModel>, MegaError> {
        Ok(lfs_locks::Entity::insert(lfs_lock.into_active_model())
            .exec(self.get_connection())
            .await?)
    }

    pub async fn get_lock_by_id(
        &self,
        refspec: &str,
    ) -> Result<Option<lfs_locks::Model>, MegaError> {
        let result = lfs_locks::Entity::find_by_id(refspec)
            .one(self.get_connection())
            .await?;
        Ok(result)
    }

    pub async fn update_lock(
        &self,
        lfs_lock: lfs_locks::Model,
        data: &str,
    ) -> Result<lfs_locks::Model, MegaError> {
        let mut val = lfs_lock.into_active_model();
        val.data = Set(data.to_owned());
        Ok(lfs_locks::Entity::update(val)
            .exec(self.get_connection())
            .await?)
    }

    pub async fn delete_lock_by_id(&self, id: String) -> Result<(), MegaError> {
        lfs_locks::Entity::delete_by_id(id)
            .exec(self.get_connection())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::jupiter::{
        migration::apply_migrations, storage::base_storage::StorageConnector,
        tests::test_db_connection,
    };

    #[tokio::test]
    async fn lfs_object_insert_is_idempotent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let connection = test_db_connection(temp_dir.path()).await;
        apply_migrations(&connection, true).await.unwrap();
        let storage = LfsDbStorage {
            base: BaseStorage::new(Arc::new(connection)),
        };
        let object = lfs_objects::Model {
            oid: "a".repeat(64),
            size: 42,
            exist: true,
        };

        assert!(storage.new_lfs_object(object.clone()).await.unwrap());
        assert!(!storage.new_lfs_object(object.clone()).await.unwrap());
        assert_eq!(
            storage.get_lfs_object(&object.oid).await.unwrap(),
            Some(object)
        );
    }
}
