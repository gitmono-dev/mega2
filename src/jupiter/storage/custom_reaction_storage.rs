use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

use crate::{
    callisto::custom_reaction,
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct CustomReactionStorage {
    pub base: BaseStorage,
}

impl Deref for CustomReactionStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl CustomReactionStorage {
    pub async fn create_custom_reaction(
        &self,
        public_id: String,
        name: String,
        file_path: String,
        file_type: String,
        username: String,
    ) -> Result<custom_reaction::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let active_model = custom_reaction::ActiveModel {
            id: Set(IdInstance::next_id()),
            public_id: Set(public_id),
            name: Set(name),
            file_path: Set(file_path),
            file_type: Set(file_type),
            username: Set(username),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let model = active_model.insert(self.get_connection()).await?;
        Ok(model)
    }

    pub async fn get_custom_reaction_by_name(
        &self,
        name: &str,
    ) -> Result<Option<custom_reaction::Model>, MegaError> {
        let model = custom_reaction::Entity::find()
            .filter(custom_reaction::Column::Name.eq(name))
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    pub async fn get_custom_reaction_by_public_id(
        &self,
        public_id: &str,
    ) -> Result<Option<custom_reaction::Model>, MegaError> {
        let model = custom_reaction::Entity::find()
            .filter(custom_reaction::Column::PublicId.eq(public_id))
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    pub async fn get_custom_reaction_by_id(
        &self,
        id: i64,
    ) -> Result<Option<custom_reaction::Model>, MegaError> {
        let model = custom_reaction::Entity::find()
            .filter(custom_reaction::Column::Id.eq(id))
            .one(self.get_connection())
            .await?;
        Ok(model)
    }
}

#[cfg(test)]
mod tests {
    use crate::callisto::entity_ext::generate_public_id;

    #[tokio::test]
    async fn test_custom_reaction_creation_and_query() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = crate::jupiter::tests::test_storage(temp_dir.path()).await;
        let cr_storage = storage.custom_reaction_storage();

        let pub_id = generate_public_id();
        let cr = cr_storage
            .create_custom_reaction(
                pub_id.clone(),
                "test-emoji".to_string(),
                "path/to/emoji".to_string(),
                "image/png".to_string(),
                "alice".to_string(),
            )
            .await
            .expect("failed to create custom reaction");

        assert_eq!(cr.public_id, pub_id);
        assert_eq!(cr.name, "test-emoji");

        let found = cr_storage
            .get_custom_reaction_by_name("test-emoji")
            .await
            .expect("failed to query");
        assert!(found.is_some());
        assert_eq!(found.unwrap().public_id, pub_id);
    }
}
