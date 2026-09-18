//! RM-02D: forward-only drop of leftover notification settings columns.
//!
//! Preference HTTP already dropped these fields (RM-02). This migration
//! removes `email`, `delivery_mode`, and `preferred_locale` from
//! `user_notification_settings`. `down` is a no-op: recovery is a new
//! forward migration, not a reconstructed column.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [
            UserNotificationSettings::Email,
            UserNotificationSettings::DeliveryMode,
            UserNotificationSettings::PreferredLocale,
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(UserNotificationSettings::Table)
                        .drop_column(column)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[derive(Iden)]
enum UserNotificationSettings {
    Table,
    Email,
    DeliveryMode,
    PreferredLocale,
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};

    use crate::jupiter::{migration::runner::apply_migrations, tests::test_db_connection};

    #[tokio::test]
    async fn drop_settings_delivery_columns_after_apply_migrations() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;

        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");

        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "SELECT column_name FROM information_schema.columns \
             WHERE table_schema = current_schema() \
             AND table_name = 'user_notification_settings' \
             AND column_name IN ('email', 'delivery_mode', 'preferred_locale') \
             ORDER BY column_name"
                .to_owned(),
        );
        let rows = db
            .query_all_raw(stmt)
            .await
            .expect("query information_schema");
        assert!(
            rows.is_empty(),
            "expected email/delivery_mode/preferred_locale to be dropped"
        );
    }
}
