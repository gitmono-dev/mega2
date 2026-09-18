//! T05: MST/2 publication tables — receipts, outbox, namespace sequence.
//!
//! `mst2_publication` is keyed uniquely on `(namespace, operation_id)` so a
//! retried writer reuses the original receipt instead of double-advancing
//! while two namespaces may carry the same operation id. Databases that
//! applied an earlier revision of this migration (unique on `operation_id`
//! alone) are brought to this shape by
//! `m20260918_000100_fix_mst2_publication_unique`. The
//! sequence counter lives in `mst2_namespace_seq`; writers bump it with
//! `UPDATE ... RETURNING` **inside the same transaction as the ref CAS**
//! (spec 09 §1: receipt + outbox + head commit atomically).
//!
//! Rows are append-only; the migration never deletes publications.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Mst2Publication::Table)
                    .if_not_exists()
                    .col(
                        big_integer(Mst2Publication::Id)
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(Mst2Publication::OperationId).not_null())
                    .col(string(Mst2Publication::Namespace).not_null())
                    .col(big_integer(Mst2Publication::Sequence).not_null())
                    .col(string(Mst2Publication::OldOid).not_null())
                    .col(string(Mst2Publication::NewOid).not_null())
                    .col(
                        big_integer(Mst2Publication::WriterEpoch)
                            .not_null()
                            .default(1),
                    )
                    .col(string(Mst2Publication::WriterKind).not_null())
                    .col(timestamp_with_time_zone(Mst2Publication::CreatedAt))
                    .to_owned(),
            )
            .await?;
        // Unique per (namespace, operation_id): push operation ids are only
        // unique per repo, so two namespaces may legitimately carry the same
        // id; a replay is same-namespace, a cross-namespace hit is refused.
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_publication_ns_operation_unique")
                    .table(Mst2Publication::Table)
                    .col(Mst2Publication::Namespace)
                    .col(Mst2Publication::OperationId)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_publication_ns_seq")
                    .table(Mst2Publication::Table)
                    .col(Mst2Publication::Namespace)
                    .col(Mst2Publication::Sequence)
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Mst2PublicationOutbox::Table)
                    .if_not_exists()
                    .col(
                        big_integer(Mst2PublicationOutbox::Id)
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(Mst2PublicationOutbox::OperationId).not_null())
                    .col(string(Mst2PublicationOutbox::Namespace).not_null())
                    .col(big_integer(Mst2PublicationOutbox::Sequence).not_null())
                    .col(
                        string(Mst2PublicationOutbox::State)
                            .not_null()
                            .default("PENDING"),
                    )
                    .col(timestamp_with_time_zone(Mst2PublicationOutbox::CreatedAt))
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_outbox_operation_unique")
                    .table(Mst2PublicationOutbox::Table)
                    .col(Mst2PublicationOutbox::OperationId)
                    .unique()
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Mst2NamespaceSeq::Table)
                    .if_not_exists()
                    .col(string(Mst2NamespaceSeq::Namespace).not_null().primary_key())
                    .col(
                        big_integer(Mst2NamespaceSeq::Sequence)
                            .not_null()
                            .default(0),
                    )
                    .col(big_integer(Mst2NamespaceSeq::Epoch).not_null().default(1))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: publications are never auto-dropped.
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Mst2Publication {
    Table,
    Id,
    OperationId,
    Namespace,
    Sequence,
    OldOid,
    NewOid,
    WriterEpoch,
    WriterKind,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Mst2PublicationOutbox {
    Table,
    Id,
    OperationId,
    Namespace,
    Sequence,
    State,
    CreatedAt,
}

#[derive(DeriveIden)]
enum Mst2NamespaceSeq {
    Table,
    Namespace,
    Sequence,
    Epoch,
}
