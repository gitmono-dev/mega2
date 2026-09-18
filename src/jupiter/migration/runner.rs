use sea_orm::DatabaseConnection;
use sea_orm_migration::prelude::MigratorTrait;
use tracing::log;

use super::Migrator;
use crate::common::errors::MegaError;

/// Applies database migrations to the given database connection.
pub async fn apply_migrations(db: &DatabaseConnection, refresh: bool) -> Result<(), MegaError> {
    match refresh {
        true => Migrator::refresh(db).await,
        false => Migrator::up(db, None).await,
    }
    .map_err(|e| {
        log::error!("Failed to apply migrations: {e}");
        e.into()
    })
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, ConnectionTrait, DbBackend, EntityTrait, Set, Statement};
    use sea_orm_migration::prelude::{MigrationTrait, MigratorTrait};

    use super::*;
    use crate::{
        callisto::{
            notification_event_types, user_notification_preferences, user_notification_settings,
        },
        jupiter::tests::test_db_connection,
        notification::triggers::{
            EVENT_CL_COMMENT_CREATED, EVENT_CL_MERGED, EVENT_ITEM_REFERENCED,
        },
    };

    #[tokio::test]
    async fn test_apply_migrations() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temporary directory");
        let db = test_db_connection(temp_dir.path()).await;
        let result = apply_migrations(&db, false).await;
        assert!(
            result.is_ok(),
            "Failed to apply migrations: {:?}",
            result.err()
        );

        let applied_migrations = Migrator::get_applied_migrations(&db).await.unwrap();
        assert!(!applied_migrations.is_empty(), "No migrations were applied");
    }

    #[tokio::test]
    async fn test_drop_chat_and_notes_schema() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temporary directory");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        for table in [
            "message_notifications",
            "messages",
            "channel_membership_updates",
            "channel_memberships",
            "channels",
            "attachments",
            "open_graph_links",
            "non_member_note_views",
            "note_views",
            "notes",
        ] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to be dropped"
            );
        }

        for table in ["reactions", "custom_reactions"] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to be dropped"
            );
        }

        {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                "SELECT to_regclass('mega_cl_reviewer')::text AS table_name;".to_owned(),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table 'mega_cl_reviewer' to be dropped"
            );
        }

        {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                "SELECT to_regclass('user_inbox_notifications')::text AS table_name;".to_owned(),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected user_inbox_notifications to be dropped"
            );
        }

        for event_type_code in ["chat.mention.created", "chat.reply.created"] {
            let event_type = notification_event_types::Entity::find_by_id(event_type_code)
                .one(&db)
                .await
                .expect("query notification event type");
            assert!(
                event_type.is_none(),
                "expected '{event_type_code}' event type to be deleted"
            );
        }
    }

    #[tokio::test]
    async fn test_drop_email_jobs_schema() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temporary directory");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        for table in ["email_job_attachments", "email_jobs"] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to be dropped"
            );
        }

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('user_inbox_notifications')::text AS table_name;".to_owned(),
        );
        let row = db
            .query_one_raw(stmt)
            .await
            .expect("query PostgreSQL catalog")
            .expect("PostgreSQL catalog query should return one row");
        let table_name: Option<String> = row
            .try_get("", "table_name")
            .expect("PostgreSQL catalog query should expose table_name");
        assert!(
            table_name.is_none(),
            "expected user_inbox_notifications to be dropped"
        );

        // Forward-only: down must be a no-op (tables stay dropped).
        {
            use sea_orm_migration::SchemaManager;
            let manager = SchemaManager::new(&db);
            crate::jupiter::migration::m20260731_000001_drop_email_jobs::Migration
                .down(&manager)
                .await
                .expect("drop_email_jobs down should be a no-op Ok(())");
        }

        for table in ["email_job_attachments", "email_jobs"] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to remain dropped after down no-op"
            );
        }

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('user_inbox_notifications')::text AS table_name;".to_owned(),
        );
        let row = db
            .query_one_raw(stmt)
            .await
            .expect("query PostgreSQL catalog")
            .expect("PostgreSQL catalog query should return one row");
        let table_name: Option<String> = row
            .try_get("", "table_name")
            .expect("PostgreSQL catalog query should expose table_name");
        assert!(
            table_name.is_none(),
            "expected user_inbox_notifications to remain dropped after down no-op"
        );
    }

    #[tokio::test]
    async fn test_drop_user_inbox_notifications_schema() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temporary directory");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('user_inbox_notifications')::text AS table_name;".to_owned(),
        );
        let row = db
            .query_one_raw(stmt)
            .await
            .expect("query PostgreSQL catalog")
            .expect("PostgreSQL catalog query should return one row");
        let table_name: Option<String> = row
            .try_get("", "table_name")
            .expect("PostgreSQL catalog query should expose table_name");
        assert!(
            table_name.is_none(),
            "expected user_inbox_notifications to be dropped"
        );

        {
            use sea_orm_migration::SchemaManager;
            let manager = SchemaManager::new(&db);
            crate::jupiter::migration::m20260919_000100_drop_user_inbox_notifications::Migration
                .down(&manager)
                .await
                .expect("drop_user_inbox_notifications down should be a no-op Ok(())");
        }

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT to_regclass('user_inbox_notifications')::text AS table_name;".to_owned(),
        );
        let row = db
            .query_one_raw(stmt)
            .await
            .expect("query PostgreSQL catalog")
            .expect("PostgreSQL catalog query should return one row");
        let table_name: Option<String> = row
            .try_get("", "table_name")
            .expect("PostgreSQL catalog query should expose table_name");
        assert!(
            table_name.is_none(),
            "expected user_inbox_notifications to remain dropped after down no-op"
        );
    }

    #[tokio::test]
    async fn test_drop_reactions_schema() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temporary directory");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        for table in ["reactions", "custom_reactions"] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to be dropped"
            );
        }

        {
            use sea_orm_migration::SchemaManager;
            let manager = SchemaManager::new(&db);
            crate::jupiter::migration::m20260919_000300_drop_reactions::Migration
                .down(&manager)
                .await
                .expect("drop_reactions down should be a no-op Ok(())");
        }

        for table in ["reactions", "custom_reactions"] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to remain dropped after down no-op"
            );
        }
    }

    #[tokio::test]
    async fn test_drop_mega_cl_reviewer_schema() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temporary directory");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                "SELECT to_regclass('mega_cl_reviewer')::text AS table_name;".to_owned(),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected mega_cl_reviewer to be dropped"
            );
        }

        {
            use sea_orm_migration::SchemaManager;
            let manager = SchemaManager::new(&db);
            crate::jupiter::migration::m20260919_000400_drop_mega_cl_reviewer::Migration
                .down(&manager)
                .await
                .expect("drop_mega_cl_reviewer down should be a no-op Ok(())");
        }

        {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                "SELECT to_regclass('mega_cl_reviewer')::text AS table_name;".to_owned(),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected mega_cl_reviewer to remain dropped after down no-op"
            );
        }
    }

    #[tokio::test]
    async fn test_drop_mega_issue_tables_schema() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temporary directory");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        for table in ["mega_issue", "git_issue", "git_pr"] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to be dropped"
            );
        }

        for table in ["mega_cl", "mega_conversation", "label", "item_labels"] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_some(),
                "expected shared table '{table}' to remain"
            );
        }

        // Forward-only: down must be a no-op (Issue tables stay dropped).
        {
            use sea_orm_migration::SchemaManager;
            let manager = SchemaManager::new(&db);
            crate::jupiter::migration::m20260831_000000_drop_mega_issue_tables::Migration
                .down(&manager)
                .await
                .expect("drop_mega_issue_tables down should be a no-op Ok(())");
        }

        for table in ["mega_issue", "git_issue", "git_pr"] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(
                table_name.is_none(),
                "expected table '{table}' to remain dropped after down no-op"
            );
        }
    }

    #[tokio::test]
    async fn test_notification_center_schema_and_constraints() {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temporary directory");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        for table in [
            "notification_event_types",
            "user_notification_settings",
            "user_notification_preferences",
        ] {
            let stmt = Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT to_regclass('{table}')::text AS table_name;"),
            );
            let row = db
                .query_one_raw(stmt)
                .await
                .expect("query PostgreSQL catalog")
                .expect("PostgreSQL catalog query should return one row");
            let table_name: Option<String> = row
                .try_get("", "table_name")
                .expect("PostgreSQL catalog query should expose table_name");
            assert!(table_name.is_some(), "expected table '{table}' to exist");
        }

        let now = chrono::Utc::now().naive_utc();

        // Core event types are seeded by migrations.
        for event_type_code in [
            EVENT_CL_COMMENT_CREATED,
            EVENT_CL_MERGED,
            EVENT_ITEM_REFERENCED,
        ] {
            let seeded = notification_event_types::Entity::find_by_id(event_type_code)
                .one(&db)
                .await
                .expect("query seeded event type");
            assert!(
                seeded.is_some(),
                "{event_type_code} should be seeded by migration"
            );
        }

        // Insert a non-seeded event type for FK-probe tests below.
        notification_event_types::ActiveModel {
            code: Set("custom.event".to_owned()),
            category: Set("custom".to_owned()),
            description: Set("Custom test event".to_owned()),
            system_required: Set(false),
            default_enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("insert custom event type");

        user_notification_settings::ActiveModel {
            username: Set("alice".to_owned()),
            enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("insert user settings");

        user_notification_preferences::ActiveModel {
            username: Set("alice".to_owned()),
            event_type_code: Set("cl.comment.created".to_owned()),
            enabled: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await
        .expect("insert user preference");

        let res = user_notification_preferences::ActiveModel {
            username: Set("alice".to_owned()),
            event_type_code: Set("does.not.exist".to_owned()),
            enabled: Set(true),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&db)
        .await;
        assert!(res.is_err(), "expected FK violation for unknown event type");
    }
}
