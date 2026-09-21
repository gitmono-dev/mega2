//! Corrective migration for the git_tag uniqueness scope.
//!
//! `m20250314_025943_init` created `uniq_gtag_tag_id` as UNIQUE(tag_id)
//! alone, while git_commit/git_tree/git_blob are all UNIQUE(repo_id, <id>).
//! Object inserts use ON CONFLICT DO NOTHING, so pushing the same annotated
//! tag object into a second import repo silently skipped the git_tag rows
//! for that repo: the ref was advertised but upload-pack could not serve the
//! tag object, and clones failed with "remote did not send all necessary
//! objects". Scope the uniqueness to (repo_id, tag_id) like the other object
//! tables. Existing rows are already unique under the wider constraint, so
//! widening is always safe.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .if_exists()
                    .name("uniq_gtag_tag_id")
                    .table(GitTag::Table)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("uniq_gtag_repo_tag")
                    .unique()
                    .table(GitTag::Table)
                    .col(GitTag::RepoId)
                    .col(GitTag::TagId)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // No down: (repo_id, tag_id) is the intended invariant; restoring the
        // repo-blind constraint would reintroduce the silent tag loss.
        Ok(())
    }
}

#[derive(DeriveIden)]
enum GitTag {
    Table,
    RepoId,
    TagId,
}
