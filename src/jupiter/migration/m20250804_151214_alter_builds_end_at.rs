use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ALTER TABLE builds ALTER COLUMN end_at DROP NOT NULL
        manager
            .alter_table(
                Table::alter()
                    .table(Builds::Table)
                    .modify_column(timestamp_null(Builds::EndAt).to_owned())
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Reverting the change - making end_at NOT NULL again.
        manager
            .alter_table(
                Table::alter()
                    .table(Builds::Table)
                    .modify_column(timestamp(Builds::EndAt).to_owned())
                    .to_owned(),
            )
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum Builds {
    Table,
    EndAt,
}
