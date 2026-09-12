use std::ops::Deref;

use sea_orm::{DbErr, EntityTrait, InsertResult, IntoActiveModel, Set, sea_query::OnConflict};

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
        match lfs_objects::Entity::insert(object.into_active_model())
            .on_conflict(
                OnConflict::column(lfs_objects::Column::Oid)
                    .do_nothing()
                    .to_owned(),
            )
            .exec(self.get_connection())
            .await
        {
            Ok(_) | Err(DbErr::RecordNotInserted) => Ok(true),
            Err(e) => Err(e.into()),
        }
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
    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn storage() -> (tempfile::TempDir, LfsDbStorage) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        (
            temp_dir,
            LfsDbStorage {
                base: BaseStorage::new(std::sync::Arc::new(db)),
            },
        )
    }

    #[tokio::test]
    async fn insert_is_idempotent_on_oid_conflict() {
        let (_dir, storage) = storage().await;
        let row = lfs_objects::Model {
            oid: "a".repeat(64),
            size: 12,
            exist: true,
        };
        assert!(storage.new_lfs_object(row.clone()).await.unwrap());
        assert!(storage.new_lfs_object(row.clone()).await.unwrap());
        let got = storage.get_lfs_object(&row.oid).await.unwrap().unwrap();
        assert_eq!(got.size, 12);
        assert!(got.exist);
    }
}
