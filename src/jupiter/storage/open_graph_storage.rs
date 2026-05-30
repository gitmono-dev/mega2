use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

use crate::{
    callisto::open_graph_link,
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct OpenGraphStorage {
    pub base: BaseStorage,
}

impl Deref for OpenGraphStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl OpenGraphStorage {
    pub async fn upsert_open_graph_link(
        &self,
        url: String,
        title: String,
        image_path: Option<String>,
        favicon_path: Option<String>,
    ) -> Result<open_graph_link::Model, MegaError> {
        let now = Utc::now().naive_utc();

        let existing = open_graph_link::Entity::find()
            .filter(open_graph_link::Column::Url.eq(&url))
            .one(self.get_connection())
            .await?;

        if let Some(model) = existing {
            let mut active_model: open_graph_link::ActiveModel = model.into();
            active_model.title = Set(title);
            active_model.image_path = Set(image_path);
            active_model.favicon_path = Set(favicon_path);
            active_model.updated_at = Set(now);
            let updated = active_model.update(self.get_connection()).await?;
            Ok(updated)
        } else {
            let active_model = open_graph_link::ActiveModel {
                id: Set(IdInstance::next_id()),
                url: Set(url),
                title: Set(title),
                image_path: Set(image_path),
                favicon_path: Set(favicon_path),
                created_at: Set(now),
                updated_at: Set(now),
            };
            let inserted = active_model.insert(self.get_connection()).await?;
            Ok(inserted)
        }
    }

    pub async fn get_open_graph_link_by_url(
        &self,
        url: &str,
    ) -> Result<Option<open_graph_link::Model>, MegaError> {
        let model = open_graph_link::Entity::find()
            .filter(open_graph_link::Column::Url.eq(url))
            .one(self.get_connection())
            .await?;
        Ok(model)
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn test_open_graph_upsert_and_query() {
        let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let storage = crate::jupiter::tests::test_storage(temp_dir.path()).await;
        let og_storage = storage.open_graph_storage();

        let url = "https://example.com".to_string();
        og_storage
            .upsert_open_graph_link(url.clone(), "Example".to_string(), None, None)
            .await
            .expect("failed to upsert");

        let found = og_storage
            .get_open_graph_link_by_url(&url)
            .await
            .expect("failed to query");
        assert!(found.is_some());
        assert_eq!(found.unwrap().title, "Example");

        // Test update
        og_storage
            .upsert_open_graph_link(url.clone(), "New Title".to_string(), None, None)
            .await
            .expect("failed to upsert update");

        let found2 = og_storage
            .get_open_graph_link_by_url(&url)
            .await
            .expect("failed to query again");
        assert_eq!(found2.unwrap().title, "New Title");
    }
}
