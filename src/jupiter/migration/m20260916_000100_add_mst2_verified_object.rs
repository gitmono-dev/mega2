//! T03: `mst2_verified_object` — verified content records (spec 08 §2).
//!
//! Insert-only verified facts about object content (raw SHA-256, 64-bit
//! size). Created via write-through verification; never derived from legacy
//! size columns.

use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Mst2VerifiedObject::Table)
                    .if_not_exists()
                    .col(
                        big_integer(Mst2VerifiedObject::Id)
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(string(Mst2VerifiedObject::StorageDomain))
                    .col(string(Mst2VerifiedObject::GitOid))
                    .col(string(Mst2VerifiedObject::ObjectKind))
                    .col(binary(Mst2VerifiedObject::RawSha256))
                    .col(big_integer(Mst2VerifiedObject::Size))
                    .col(integer(Mst2VerifiedObject::VerificationVersion).default(1))
                    .col(string(Mst2VerifiedObject::State).default("VERIFIED"))
                    .col(timestamp_with_time_zone(Mst2VerifiedObject::CreatedAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_mst2_verified_object_natural_key")
                    .table(Mst2VerifiedObject::Table)
                    .col(Mst2VerifiedObject::StorageDomain)
                    .col(Mst2VerifiedObject::GitOid)
                    .col(Mst2VerifiedObject::ObjectKind)
                    .unique()
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: verified facts are never auto-dropped.
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Mst2VerifiedObject {
    Table,
    Id,
    StorageDomain,
    GitOid,
    ObjectKind,
    RawSha256,
    Size,
    VerificationVersion,
    State,
    CreatedAt,
}
