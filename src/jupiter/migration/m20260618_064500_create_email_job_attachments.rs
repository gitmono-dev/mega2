use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(EmailJobAttachments::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(EmailJobAttachments::Id)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(EmailJobAttachments::EmailJobId)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EmailJobAttachments::Filename)
                            .string_len(255)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EmailJobAttachments::ContentType)
                            .string_len(128)
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EmailJobAttachments::Content)
                            .binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(EmailJobAttachments::CreatedAt)
                            .date_time()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_email_job_attachments_job")
                            .from(EmailJobAttachments::Table, EmailJobAttachments::EmailJobId)
                            .to(EmailJobs::Table, EmailJobs::Id)
                            .on_delete(ForeignKeyAction::Cascade)
                            .on_update(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_email_job_attachments_job_id")
                    .table(EmailJobAttachments::Table)
                    .col(EmailJobAttachments::EmailJobId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(EmailJobAttachments::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum EmailJobAttachments {
    Table,
    Id,
    EmailJobId,
    Filename,
    ContentType,
    Content,
    CreatedAt,
}

#[derive(DeriveIden)]
enum EmailJobs {
    Table,
    Id,
}
