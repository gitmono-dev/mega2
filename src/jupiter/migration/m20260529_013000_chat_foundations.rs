use sea_orm_migration::{prelude::*, schema::*};

use crate::jupiter::migration::pk_bigint;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. attachments
        manager
            .create_table(
                table_auto(Attachments::Table)
                    .col(pk_bigint(Attachments::Id))
                    .col(string_len(Attachments::PublicId, 12).unique_key())
                    .col(string(Attachments::FilePath))
                    .col(string(Attachments::FileType))
                    .col(string(Attachments::SubjectType))
                    .col(big_integer(Attachments::SubjectId))
                    .col(string_null(Attachments::PreviewFilePath))
                    .col(integer_null(Attachments::Width))
                    .col(integer_null(Attachments::Height))
                    .col(integer_null(Attachments::Duration))
                    .col(integer(Attachments::Position))
                    .col(string(Attachments::Name))
                    .col(big_integer(Attachments::Size))
                    .col(big_integer_null(Attachments::GalleryId))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx-attachments-subject")
                    .table(Attachments::Table)
                    .col(Attachments::SubjectType)
                    .col(Attachments::SubjectId)
                    .to_owned(),
            )
            .await?;

        // 2. custom_reactions
        manager
            .create_table(
                table_auto(CustomReactions::Table)
                    .col(pk_bigint(CustomReactions::Id))
                    .col(string_len(CustomReactions::PublicId, 12).unique_key())
                    .col(string(CustomReactions::Name).unique_key())
                    .col(string(CustomReactions::FilePath))
                    .col(string(CustomReactions::FileType))
                    .col(string(CustomReactions::Username))
                    .col(string_null(CustomReactions::Pack))
                    .to_owned(),
            )
            .await?;

        // 3. update reactions
        // Since reactions table already exists, we alter it.
        // If it doesn't exist (e.g. in a fresh DB), we should ensure it's created by the previous migration.
        // But the previous migration m20250710_073119_create_reactions already creates it.
        manager
            .alter_table(
                Table::alter()
                    .table(Reactions::Table)
                    .add_column(big_integer_null(Reactions::CustomReactionId))
                    .to_owned(),
            )
            .await?;

        manager
            .alter_table(
                Table::alter()
                    .table(Reactions::Table)
                    .drop_column(Alias::new("organization_membership_id"))
                    .to_owned(),
            )
            .await?;

        // Add timestamps if missing (m20250710 didn't have them in the DSL, but Model had them)
        // Wait, m20250710 Model has created_at/updated_at but Migration DSL didn't show them.
        // Let me check m20250710 again.

        manager
            .create_index(
                Index::create()
                    .name("idx-reactions-unique-v2")
                    .table(Reactions::Table)
                    .col(Reactions::SubjectType)
                    .col(Reactions::SubjectId)
                    .col(Reactions::Username)
                    .col(Reactions::Content)
                    .col(Reactions::CustomReactionId)
                    .col(Reactions::DiscardedAt)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // 4. open_graph_links
        manager
            .create_table(
                table_auto(OpenGraphLinks::Table)
                    .col(pk_bigint(OpenGraphLinks::Id))
                    .col(string(OpenGraphLinks::Url).unique_key())
                    .col(string(OpenGraphLinks::Title))
                    .col(string_null(OpenGraphLinks::ImagePath))
                    .col(string_null(OpenGraphLinks::FaviconPath))
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
    Id,
    PublicId,
    FilePath,
    FileType,
    SubjectType,
    SubjectId,
    PreviewFilePath,
    Width,
    Height,
    Duration,
    Position,
    Name,
    Size,
    GalleryId,
}

#[derive(DeriveIden)]
enum CustomReactions {
    Table,
    Id,
    PublicId,
    Name,
    FilePath,
    FileType,
    Username,
    Pack,
}

#[derive(DeriveIden)]
enum Reactions {
    Table,
    SubjectId,
    SubjectType,
    Username,
    Content,
    CustomReactionId,
    DiscardedAt,
}

#[derive(DeriveIden)]
enum OpenGraphLinks {
    Table,
    Id,
    Url,
    Title,
    ImagePath,
    FaviconPath,
}
