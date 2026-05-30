use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
};

use crate::{
    callisto::attachment,
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct AttachmentStorage {
    pub base: BaseStorage,
}

impl Deref for AttachmentStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl AttachmentStorage {
    #[allow(clippy::too_many_arguments)]
    // 9 args mirror the attachments table columns for this internal storage API;
    // introducing a builder would be overkill for the current usage patterns.
    pub async fn create_attachment(
        &self,
        public_id: String,
        file_path: String,
        file_type: String,
        subject_type: String,
        subject_id: i64,
        name: String,
        size: i64,
        position: i32,
    ) -> Result<attachment::Model, MegaError> {
        let now = Utc::now().naive_utc();
        let active_model = attachment::ActiveModel {
            id: Set(IdInstance::next_id()),
            public_id: Set(public_id),
            file_path: Set(file_path),
            file_type: Set(file_type),
            subject_type: Set(subject_type),
            subject_id: Set(subject_id),
            name: Set(name),
            size: Set(size),
            position: Set(position),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let model = active_model.insert(self.get_connection()).await?;
        Ok(model)
    }

    pub async fn get_attachments_by_subject(
        &self,
        subject_type: &str,
        subject_id: i64,
    ) -> Result<Vec<attachment::Model>, MegaError> {
        let models = attachment::Entity::find()
            .filter(attachment::Column::SubjectType.eq(subject_type))
            .filter(attachment::Column::SubjectId.eq(subject_id))
            .order_by_asc(attachment::Column::Position)
            .all(self.get_connection())
            .await?;
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use crate::callisto::entity_ext::generate_public_id;

    #[tokio::test]
    async fn test_attachment_creation_and_query() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = crate::jupiter::tests::test_storage(temp_dir.path()).await;
        let attachment_storage = storage.attachment_storage();

        let pub_id = generate_public_id();
        let att = attachment_storage
            .create_attachment(
                pub_id.clone(),
                "path/to/file".to_string(),
                "image/png".to_string(),
                "Message".to_string(),
                123,
                "test.png".to_string(),
                1024,
                1,
            )
            .await
            .expect("failed to create attachment");

        assert_eq!(att.public_id, pub_id);
        assert_eq!(att.subject_id, 123);

        let results = attachment_storage
            .get_attachments_by_subject("Message", 123)
            .await
            .expect("failed to query attachments");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].public_id, pub_id);
    }
}
