use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};

use crate::callisto::{
    notification_event_types, user_notification_preferences, user_notification_settings,
};

#[derive(Clone)]
pub struct NotificationStorage {
    db: Arc<DatabaseConnection>,
}

impl NotificationStorage {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }

    pub fn db(&self) -> &DatabaseConnection {
        &self.db
    }

    // Event types
    pub async fn list_event_types(
        &self,
    ) -> Result<Vec<notification_event_types::Model>, sea_orm::DbErr> {
        notification_event_types::Entity::find()
            .order_by_asc(notification_event_types::Column::Code)
            .all(self.db())
            .await
    }

    pub async fn get_event_type(
        &self,
        code: &str,
    ) -> Result<Option<notification_event_types::Model>, sea_orm::DbErr> {
        notification_event_types::Entity::find()
            .filter(notification_event_types::Column::Code.eq(code))
            .one(self.db())
            .await
    }

    pub async fn upsert_event_type(
        &self,
        code: &str,
        category: &str,
        description: &str,
        system_required: bool,
        default_enabled: bool,
    ) -> Result<notification_event_types::Model, sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();

        if let Some(existing) = self.get_event_type(code).await? {
            let mut model: notification_event_types::ActiveModel = existing.into();
            model.category = Set(category.to_string());
            model.description = Set(description.to_string());
            model.system_required = Set(system_required);
            model.default_enabled = Set(default_enabled);
            model.updated_at = Set(now);
            return model.update(self.db()).await;
        }

        notification_event_types::ActiveModel {
            code: Set(code.to_string()),
            category: Set(category.to_string()),
            description: Set(description.to_string()),
            system_required: Set(system_required),
            default_enabled: Set(default_enabled),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(self.db())
        .await
    }

    // User notification settings
    pub async fn get_user_settings(
        &self,
        username: &str,
    ) -> Result<Option<user_notification_settings::Model>, sea_orm::DbErr> {
        user_notification_settings::Entity::find()
            .filter(user_notification_settings::Column::Username.eq(username))
            .one(self.db())
            .await
    }

    pub async fn upsert_user_settings(&self, username: &str) -> Result<(), sea_orm::DbErr> {
        if self.get_user_settings(username).await?.is_some() {
            return Ok(());
        }

        let now = chrono::Utc::now().naive_utc();
        user_notification_settings::ActiveModel {
            username: Set(username.to_string()),
            enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(self.db())
        .await?;

        Ok(())
    }

    // Notification preferences
    pub async fn get_user_preference(
        &self,
        username: &str,
        event_type_code: &str,
    ) -> Result<Option<user_notification_preferences::Model>, sea_orm::DbErr> {
        user_notification_preferences::Entity::find()
            .filter(user_notification_preferences::Column::Username.eq(username))
            .filter(user_notification_preferences::Column::EventTypeCode.eq(event_type_code))
            .one(self.db())
            .await
    }

    pub async fn set_user_preference(
        &self,
        username: &str,
        event_type_code: &str,
        enabled: bool,
    ) -> Result<(), sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();

        if let Some(existing) = self.get_user_preference(username, event_type_code).await? {
            let mut model: user_notification_preferences::ActiveModel = existing.into();
            model.enabled = Set(enabled);
            model.updated_at = Set(now);
            model.update(self.db()).await?;
        } else {
            user_notification_preferences::ActiveModel {
                username: Set(username.to_string()),
                event_type_code: Set(event_type_code.to_string()),
                enabled: Set(enabled),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(self.db())
            .await?;
        }

        Ok(())
    }

    pub async fn list_user_preferences(
        &self,
        username: &str,
    ) -> Result<Vec<user_notification_preferences::Model>, sea_orm::DbErr> {
        user_notification_preferences::Entity::find()
            .filter(user_notification_preferences::Column::Username.eq(username))
            .all(self.db())
            .await
    }

    pub async fn set_global_enabled(
        &self,
        username: &str,
        enabled: bool,
    ) -> Result<(), sea_orm::DbErr> {
        if let Some(existing) = self.get_user_settings(username).await? {
            let mut model: user_notification_settings::ActiveModel = existing.into();
            model.enabled = Set(enabled);
            model.updated_at = Set(chrono::Utc::now().naive_utc());
            model.update(self.db()).await?;
        }
        Ok(())
    }

    // Main logic of whether to send a notification for a given user and event type
    pub async fn should_send(
        &self,
        username: &str,
        event_type_code: &str,
    ) -> Result<bool, sea_orm::DbErr> {
        let event_type = match self.get_event_type(event_type_code).await? {
            Some(e) => e,
            None => return Ok(false),
        };

        let settings = match self.get_user_settings(username).await? {
            Some(s) => s,
            None => return Ok(false),
        };

        if !settings.enabled {
            return Ok(false);
        }

        if event_type.system_required {
            return Ok(true);
        }

        if let Some(pref) = self.get_user_preference(username, event_type_code).await? {
            return Ok(pref.enabled);
        }

        Ok(event_type.default_enabled)
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, Set};

    use super::*;
    use crate::{
        callisto::notification_event_types,
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    #[tokio::test]
    async fn upsert_event_type_inserts_and_updates_existing_row() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        let inserted = storage
            .upsert_event_type("test.event", "test", "First", false, true)
            .await
            .unwrap();

        assert_eq!(inserted.code, "test.event");
        assert_eq!(inserted.category, "test");
        assert_eq!(inserted.description, "First");
        assert!(!inserted.system_required);
        assert!(inserted.default_enabled);

        let updated = storage
            .upsert_event_type("test.event", "updated", "Second", true, false)
            .await
            .unwrap();

        assert_eq!(updated.code, "test.event");
        assert_eq!(updated.category, "updated");
        assert_eq!(updated.description, "Second");
        assert!(updated.system_required);
        assert!(!updated.default_enabled);
        assert_eq!(updated.created_at, inserted.created_at);
        assert!(updated.updated_at >= inserted.updated_at);

        let event_types = storage.list_event_types().await.unwrap();
        assert_eq!(event_types.len(), 4);
        assert!(event_types.iter().any(|et| et.code == "test.event"));
    }

    #[tokio::test]
    async fn test_should_send_logic() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));

        let now = chrono::Utc::now().naive_utc();
        notification_event_types::ActiveModel {
            code: Set("test.event".to_string()),
            category: Set("test".to_string()),
            description: Set("desc".to_string()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();

        storage.upsert_user_settings("alice").await.unwrap();

        assert!(storage.should_send("alice", "test.event").await.unwrap());

        storage
            .set_user_preference("alice", "test.event", false)
            .await
            .unwrap();

        assert!(!storage.should_send("alice", "test.event").await.unwrap());
    }

    #[tokio::test]
    async fn upsert_user_settings_is_idempotent_without_delivery_columns() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        storage.upsert_user_settings("alice").await.unwrap();
        storage.upsert_user_settings("alice").await.unwrap();

        let settings = storage.get_user_settings("alice").await.unwrap().unwrap();
        assert_eq!(settings.username, "alice");
        assert!(settings.enabled);
    }
}
