use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(UserInboxNotifications::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(UserInboxNotifications::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(UserInboxNotifications::Username)
                            .string_len(255)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserInboxNotifications::EventTypeCode)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserInboxNotifications::Subject)
                            .string_len(1024)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserInboxNotifications::BodyHtml)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(UserInboxNotifications::BodyText)
                            .text()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(UserInboxNotifications::Read)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .col(
                        ColumnDef::new(UserInboxNotifications::CreatedAt)
                            .date_time()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_user_inbox_notifications_username")
                    .table(UserInboxNotifications::Table)
                    .col(UserInboxNotifications::Username)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(UserInboxNotifications::Table)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum UserInboxNotifications {
    Table,
    Id,
    Username,
    EventTypeCode,
    Subject,
    BodyHtml,
    BodyText,
    Read,
    CreatedAt,
}
