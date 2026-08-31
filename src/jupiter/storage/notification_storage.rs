use std::sync::Arc;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};

use crate::callisto::{
    notification_event_types, user_inbox_notifications, user_notification_preferences,
    user_notification_settings,
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

    // In-app (inbox) notifications

    /// Persist an in-app notification row for a user (in-app delivery channel).
    pub async fn create_inbox_notification(
        &self,
        username: &str,
        event_type_code: &str,
        subject: &str,
        body_html: &str,
        body_text: Option<&str>,
    ) -> Result<(), sea_orm::DbErr> {
        user_inbox_notifications::ActiveModel {
            username: Set(username.to_string()),
            event_type_code: Set(event_type_code.to_string()),
            subject: Set(subject.to_string()),
            body_html: Set(body_html.to_string()),
            body_text: Set(body_text.map(str::to_string)),
            read: Set(false),
            created_at: Set(chrono::Utc::now().naive_utc()),
            ..Default::default()
        }
        .insert(self.db())
        .await?;

        Ok(())
    }

    /// List a user's in-app notifications, newest first.
    pub async fn list_inbox_notifications(
        &self,
        username: &str,
        limit: u64,
    ) -> Result<Vec<user_inbox_notifications::Model>, sea_orm::DbErr> {
        user_inbox_notifications::Entity::find()
            .filter(user_inbox_notifications::Column::Username.eq(username))
            .order_by_desc(user_inbox_notifications::Column::Id)
            .limit(limit)
            .all(self.db())
            .await
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

    pub async fn upsert_user_settings(
        &self,
        username: &str,
        email: &str,
    ) -> Result<(), sea_orm::DbErr> {
        let now = chrono::Utc::now().naive_utc();

        if let Some(existing) = self.get_user_settings(username).await? {
            let mut model: user_notification_settings::ActiveModel = existing.into();
            model.email = Set(email.to_string());
            model.updated_at = Set(now);
            model.update(self.db()).await?;
        } else {
            user_notification_settings::ActiveModel {
                username: Set(username.to_string()),
                email: Set(email.to_string()),
                enabled: Set(true),
                delivery_mode: Set(crate::notification::service::current_default_delivery_mode()),
                preferred_locale: Set(None),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(self.db())
            .await?;
        }

        Ok(())
    }

    pub async fn set_preferred_locale(
        &self,
        username: &str,
        preferred_locale: Option<&str>,
    ) -> Result<(), sea_orm::DbErr> {
        if let Some(existing) = self.get_user_settings(username).await? {
            let mut model: user_notification_settings::ActiveModel = existing.into();
            model.preferred_locale = Set(preferred_locale.map(str::to_string));
            model.updated_at = Set(chrono::Utc::now().naive_utc());
            model.update(self.db()).await?;
        }
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

    pub async fn set_delivery_mode(
        &self,
        username: &str,
        mode: &str,
    ) -> Result<(), sea_orm::DbErr> {
        if let Some(existing) = self.get_user_settings(username).await? {
            let mut model: user_notification_settings::ActiveModel = existing.into();
            model.delivery_mode = Set(mode.to_string());
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

        storage
            .upsert_user_settings("alice", "alice@test.com")
            .await
            .unwrap();

        assert!(storage.should_send("alice", "test.event").await.unwrap());

        storage
            .set_user_preference("alice", "test.event", false)
            .await
            .unwrap();

        assert!(!storage.should_send("alice", "test.event").await.unwrap());
    }

    #[tokio::test]
    async fn user_settings_preferred_locale_can_be_updated_and_cleared() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true).await.unwrap();

        let storage = NotificationStorage::new(Arc::new(db.clone()));
        storage
            .upsert_user_settings("alice", "alice@test.com")
            .await
            .unwrap();

        assert_eq!(
            storage
                .get_user_settings("alice")
                .await
                .unwrap()
                .unwrap()
                .preferred_locale,
            None
        );

        storage
            .set_preferred_locale("alice", Some("zh-CN"))
            .await
            .unwrap();
        assert_eq!(
            storage
                .get_user_settings("alice")
                .await
                .unwrap()
                .unwrap()
                .preferred_locale
                .as_deref(),
            Some("zh-CN")
        );

        storage.set_preferred_locale("alice", None).await.unwrap();
        assert_eq!(
            storage
                .get_user_settings("alice")
                .await
                .unwrap()
                .unwrap()
                .preferred_locale,
            None
        );
    }
}
