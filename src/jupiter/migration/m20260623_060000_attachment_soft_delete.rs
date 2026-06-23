use sea_orm_migration::{prelude::*, schema::date_time_null};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Attachments::Table)
                    .add_column(date_time_null(Attachments::DiscardedAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx-attachments-discarded-at")
                    .table(Attachments::Table)
                    .col(Attachments::DiscardedAt)
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Attachments {
    Table,
    DiscardedAt,
}
