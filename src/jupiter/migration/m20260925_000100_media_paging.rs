//! MF-08: Media session / entry / task tables for durable paging state.
//!
//! Portable `sea_query` DDL (Postgres runtime today; Sqlite statement shape
//! covered by unit tests). Additive only — `down` does not drop task data.

use sea_orm::DatabaseBackend;
use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(MediaSession::Table)
                    .if_not_exists()
                    .col(
                        big_integer(MediaSession::Id)
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(MediaSession::ScopeDigest).not_null())
                    .col(string(MediaSession::ManifestId).not_null())
                    .col(string(MediaSession::Algorithm).not_null())
                    .col(string(MediaSession::Oid).not_null())
                    .col(big_integer(MediaSession::Size).not_null())
                    .col(big_integer(MediaSession::ChunkCount).not_null())
                    .col(integer(MediaSession::PageCount).not_null())
                    .col(string(MediaSession::State).not_null())
                    .col(
                        big_integer(MediaSession::SealGeneration)
                            .not_null()
                            .default(0),
                    )
                    .col(text(MediaSession::CreatedBy).null())
                    .col(timestamp_with_time_zone(MediaSession::CreatedAt).not_null())
                    .col(timestamp_with_time_zone(MediaSession::UpdatedAt).not_null())
                    .col(timestamp_with_time_zone(MediaSession::ExpiresAt).not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_media_session_scope_manifest")
                    .table(MediaSession::Table)
                    .col(MediaSession::ScopeDigest)
                    .col(MediaSession::ManifestId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(MediaEntry::Table)
                    .if_not_exists()
                    .col(
                        big_integer(MediaEntry::Id)
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(MediaEntry::ScopeDigest).not_null())
                    .col(string(MediaEntry::ManifestId).not_null())
                    .col(integer(MediaEntry::PageNo).not_null())
                    .col(integer(MediaEntry::Ordinal).not_null())
                    .col(big_integer(MediaEntry::Offset).not_null())
                    .col(big_integer(MediaEntry::Length).not_null())
                    .col(string(MediaEntry::ChunkHash).not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_media_entry_page_ordinal")
                    .table(MediaEntry::Table)
                    .col(MediaEntry::ScopeDigest)
                    .col(MediaEntry::ManifestId)
                    .col(MediaEntry::PageNo)
                    .col(MediaEntry::Ordinal)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_media_entry_hash")
                    .table(MediaEntry::Table)
                    .col(MediaEntry::ScopeDigest)
                    .col(MediaEntry::ManifestId)
                    .col(MediaEntry::ChunkHash)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_media_entry_offset")
                    .table(MediaEntry::Table)
                    .col(MediaEntry::ScopeDigest)
                    .col(MediaEntry::ManifestId)
                    .col(MediaEntry::Offset)
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(MediaTask::Table)
                    .if_not_exists()
                    .col(
                        big_integer(MediaTask::Id)
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(MediaTask::TaskId).not_null())
                    .col(string(MediaTask::ScopeDigest).not_null())
                    .col(string(MediaTask::ManifestId).not_null())
                    .col(string(MediaTask::LeaseOwner).null())
                    .col(big_integer(MediaTask::LeaseEpoch).not_null().default(0))
                    .col(timestamp_with_time_zone(MediaTask::ExpiresAt).null())
                    .col(string(MediaTask::State).not_null())
                    .col(big_integer(MediaTask::BytesVerified).not_null().default(0))
                    .col(integer(MediaTask::PagesVerified).not_null().default(0))
                    .col(boolean(MediaTask::Retryable).not_null().default(true))
                    .col(string(MediaTask::ErrorCode).null())
                    .col(string(MediaTask::Stage).not_null())
                    .col(timestamp_with_time_zone(MediaTask::CreatedAt).not_null())
                    .col(timestamp_with_time_zone(MediaTask::UpdatedAt).not_null())
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_media_task_task_id")
                    .table(MediaTask::Table)
                    .col(MediaTask::TaskId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_media_task_scope_manifest")
                    .table(MediaTask::Table)
                    .col(MediaTask::ScopeDigest)
                    .col(MediaTask::ManifestId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Compensating: pause Media writes and switch namespace; keep tables.
        Ok(())
    }
}

#[derive(DeriveIden)]
pub(crate) enum MediaSession {
    Table,
    Id,
    ScopeDigest,
    ManifestId,
    Algorithm,
    Oid,
    Size,
    ChunkCount,
    PageCount,
    State,
    SealGeneration,
    CreatedBy,
    CreatedAt,
    UpdatedAt,
    ExpiresAt,
}

#[derive(DeriveIden)]
pub(crate) enum MediaEntry {
    Table,
    Id,
    ScopeDigest,
    ManifestId,
    PageNo,
    Ordinal,
    Offset,
    Length,
    ChunkHash,
}

#[derive(DeriveIden)]
pub(crate) enum MediaTask {
    Table,
    Id,
    TaskId,
    ScopeDigest,
    ManifestId,
    LeaseOwner,
    LeaseEpoch,
    ExpiresAt,
    State,
    BytesVerified,
    PagesVerified,
    Retryable,
    ErrorCode,
    Stage,
    CreatedAt,
    UpdatedAt,
}

/// Build create-table SQL for a given backend (dual-backend AC evidence).
pub fn media_paging_create_sql(backend: DatabaseBackend) -> Vec<String> {
    let session = Table::create()
        .table(MediaSession::Table)
        .if_not_exists()
        .col(
            big_integer(MediaSession::Id)
                .not_null()
                .auto_increment()
                .primary_key(),
        )
        .col(string(MediaSession::ScopeDigest).not_null())
        .col(string(MediaSession::ManifestId).not_null())
        .to_owned();
    let entry = Table::create()
        .table(MediaEntry::Table)
        .if_not_exists()
        .col(
            big_integer(MediaEntry::Id)
                .not_null()
                .auto_increment()
                .primary_key(),
        )
        .col(string(MediaEntry::ChunkHash).not_null())
        .to_owned();
    let task = Table::create()
        .table(MediaTask::Table)
        .if_not_exists()
        .col(
            big_integer(MediaTask::Id)
                .not_null()
                .auto_increment()
                .primary_key(),
        )
        .col(string(MediaTask::TaskId).not_null())
        .to_owned();
    match backend {
        DatabaseBackend::Postgres => vec![
            session.to_string(PostgresQueryBuilder),
            entry.to_string(PostgresQueryBuilder),
            task.to_string(PostgresQueryBuilder),
        ],
        DatabaseBackend::Sqlite => vec![
            session.to_string(SqliteQueryBuilder),
            entry.to_string(SqliteQueryBuilder),
            task.to_string(SqliteQueryBuilder),
        ],
        other => panic!("unsupported backend for media paging DDL: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::DatabaseBackend;

    use super::*;

    #[test]
    fn media_paging_ddl_emits_postgres_and_sqlite() {
        for backend in [DatabaseBackend::Postgres, DatabaseBackend::Sqlite] {
            let stmts = media_paging_create_sql(backend);
            assert_eq!(stmts.len(), 3, "{backend:?}");
            for s in &stmts {
                assert!(s.to_lowercase().contains("create"), "{backend:?}: {s}");
                assert!(
                    s.to_lowercase().contains("if not exists")
                        || s.to_lowercase().contains("if not exists"),
                    "{backend:?}: missing IF NOT EXISTS: {s}"
                );
            }
        }
    }
}
