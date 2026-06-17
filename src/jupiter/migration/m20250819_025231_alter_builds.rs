use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Builds::Table)
                    .drop_column("output")
                    .add_column_if_not_exists(text(Builds::OutputFile))
                    .add_column_if_not_exists(text(Builds::Arguments))
                    .add_column_if_not_exists(text(Builds::Mr))
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
enum Builds {
    Table,
    OutputFile,
    Arguments,
    Mr,
}
