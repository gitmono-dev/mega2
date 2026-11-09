//! Explicit legacy-Capture maintenance migrator (MTR-32).
//!
//! The guarded DROP is never part of the normal `Migrator` list: an ordinary
//! service startup must keep the legacy tables and must not depend on a purge
//! having finished. It lives in its own migrator with its own ledger, and its
//! `up` refuses to run without a verified receipt (the real DROP runs through
//! [`drop_legacy_capture::execute`]).

pub mod drop_legacy_capture;

use sea_orm_migration::{
    MigrationTrait, MigratorTrait, SchemaManager,
    prelude::{DynIden, IntoIden},
    sea_orm::DbErr,
};

/// Independent ledger for the destructive maintenance migrations.
pub const MAINTENANCE_LEDGER: &str = "traces_maintenance_schema_migrations";

/// Migrator that contains only the guarded DROP. It is deliberately not
/// referenced by the normal startup `Migrator`.
pub struct TracesMaintenanceMigrator;

#[async_trait::async_trait]
impl MigratorTrait for TracesMaintenanceMigrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(drop_legacy_capture::Migration)]
    }

    fn migration_table_name() -> DynIden {
        MAINTENANCE_LEDGER.into_iden()
    }
}

/// The maintenance `up` without a receipt fails closed and writes no ledger row.
pub(crate) async fn refuse_unverified_up(_manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    Err(DbErr::Custom(
        "the guarded legacy DROP needs a verified receipt; use the maintenance entry point".into(),
    ))
}
