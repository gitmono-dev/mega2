use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

use crate::{
    callisto::reactions,
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct ReactionStorage {
    pub base: BaseStorage,
}

impl Deref for ReactionStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl ReactionStorage {
    pub async fn create_reaction(
        &self,
        public_id: String,
        content: Option<String>,
        subject_type: String,
        subject_id: i64,
        username: String,
        custom_reaction_id: Option<i64>,
    ) -> Result<reactions::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let active_model = reactions::ActiveModel {
            id: Set(IdInstance::next_id()),
            public_id: Set(public_id),
            content: Set(content),
            subject_type: Set(subject_type),
            subject_id: Set(subject_id),
            username: Set(username),
            custom_reaction_id: Set(custom_reaction_id),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let model = active_model.insert(self.get_connection()).await?;
        Ok(model)
    }

    pub async fn soft_delete_reaction(&self, public_id: &str) -> Result<(), MegaError> {
        let now = Utc::now().naive_utc();

        let model = reactions::Entity::find()
            .filter(reactions::Column::PublicId.eq(public_id))
            .one(self.get_connection())
            .await?;

        if let Some(model) = model {
            let mut active_model: reactions::ActiveModel = model.into();
            active_model.discarded_at = Set(Some(now));
            active_model.update(self.get_connection()).await?;
        }

        Ok(())
    }

    pub async fn get_active_reaction_by_public_id(
        &self,
        public_id: &str,
    ) -> Result<Option<reactions::Model>, MegaError> {
        let model = reactions::Entity::find()
            .filter(reactions::Column::PublicId.eq(public_id))
            .filter(reactions::Column::DiscardedAt.is_null())
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    pub async fn get_reactions_by_subject(
        &self,
        subject_type: &str,
        subject_id: i64,
    ) -> Result<Vec<reactions::Model>, MegaError> {
        let models = reactions::Entity::find()
            .filter(reactions::Column::SubjectType.eq(subject_type))
            .filter(reactions::Column::SubjectId.eq(subject_id))
            .filter(reactions::Column::DiscardedAt.is_null())
            .all(self.get_connection())
            .await?;
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use crate::{callisto::entity_ext::generate_public_id, common::errors::MegaError};

    fn assert_unique_constraint_error(err: &MegaError) {
        let msg = err.to_string().to_lowercase();
        assert!(
            matches!(err, MegaError::Db(_)) || msg.contains("unique") || msg.contains("duplicate"),
            "expected unique-constraint error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_reaction_creation_and_soft_delete() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = crate::jupiter::tests::test_storage(temp_dir.path()).await;
        let reaction_storage = storage.reaction_storage();

        let pub_id = generate_public_id();
        let reaction = reaction_storage
            .create_reaction(
                pub_id.clone(),
                Some("👍".to_string()),
                "Message".to_string(),
                123,
                "alice".to_string(),
                None,
            )
            .await
            .expect("failed to create reaction");

        assert_eq!(reaction.public_id, pub_id);

        let results = reaction_storage
            .get_reactions_by_subject("Message", 123)
            .await
            .expect("failed to query reactions");
        assert_eq!(results.len(), 1);

        reaction_storage
            .soft_delete_reaction(&pub_id)
            .await
            .expect("failed to delete");

        let results_after = reaction_storage
            .get_reactions_by_subject("Message", 123)
            .await
            .expect("failed to query reactions");
        assert_eq!(results_after.len(), 0);

        let deleted = reaction_storage
            .get_active_reaction_by_public_id(&pub_id)
            .await
            .expect("failed to query deleted reaction");
        assert!(deleted.is_none());
    }

    #[tokio::test]
    async fn test_duplicate_active_standard_reaction_is_rejected() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = crate::jupiter::tests::test_storage(temp_dir.path()).await;
        let reaction_storage = storage.reaction_storage();

        reaction_storage
            .create_reaction(
                generate_public_id(),
                Some("👍".to_string()),
                "Message".to_string(),
                123,
                "alice".to_string(),
                None,
            )
            .await
            .expect("first standard reaction should succeed");

        let err = reaction_storage
            .create_reaction(
                generate_public_id(),
                Some("👍".to_string()),
                "Message".to_string(),
                123,
                "alice".to_string(),
                None,
            )
            .await
            .expect_err("duplicate active standard reaction should be rejected");

        assert_unique_constraint_error(&err);
    }

    #[tokio::test]
    async fn test_soft_deleted_standard_reaction_can_be_recreated() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = crate::jupiter::tests::test_storage(temp_dir.path()).await;
        let reaction_storage = storage.reaction_storage();

        let pub_id = generate_public_id();
        reaction_storage
            .create_reaction(
                pub_id.clone(),
                Some("👍".to_string()),
                "Message".to_string(),
                123,
                "alice".to_string(),
                None,
            )
            .await
            .expect("first standard reaction should succeed");
        reaction_storage
            .soft_delete_reaction(&pub_id)
            .await
            .expect("soft delete should succeed");

        let recreated = reaction_storage
            .create_reaction(
                generate_public_id(),
                Some("👍".to_string()),
                "Message".to_string(),
                123,
                "alice".to_string(),
                None,
            )
            .await
            .expect("soft-deleted reaction should not block recreation");

        assert_eq!(recreated.content.as_deref(), Some("👍"));
    }

    #[tokio::test]
    async fn test_duplicate_active_custom_reaction_is_rejected() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = crate::jupiter::tests::test_storage(temp_dir.path()).await;
        let custom_reaction_storage = storage.custom_reaction_storage();
        let reaction_storage = storage.reaction_storage();
        let custom_reaction = custom_reaction_storage
            .create_custom_reaction(
                generate_public_id(),
                "party".to_string(),
                "path/to/party.png".to_string(),
                "image/png".to_string(),
                "alice".to_string(),
            )
            .await
            .expect("custom reaction should be created");

        reaction_storage
            .create_reaction(
                generate_public_id(),
                None,
                "Message".to_string(),
                123,
                "alice".to_string(),
                Some(custom_reaction.id),
            )
            .await
            .expect("first custom reaction should succeed");

        let err = reaction_storage
            .create_reaction(
                generate_public_id(),
                None,
                "Message".to_string(),
                123,
                "alice".to_string(),
                Some(custom_reaction.id),
            )
            .await
            .expect_err("duplicate active custom reaction should be rejected");

        assert_unique_constraint_error(&err);
    }
}
