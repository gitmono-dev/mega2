//! Guarded physical removal of the 12 legacy Capture tables (MTR-32).
//!
//! `execute` consumes a [`VerifiedLegacyPurgeReceipt`] (constructible only by
//! MTR-44), re-validates it against the current journal/fence/inventory, drops
//! the 12 tables in one transaction and records the maintenance version in the
//! independent ledger only after the DROP succeeded. A second call is
//! idempotent. The historic add-migration registration is untouched and the
//! migration's `down` is an explicit no-op.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use sea_orm_migration::prelude::*;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::jupiter::{
    migration::traces_maintenance::{MAINTENANCE_LEDGER, refuse_unverified_up},
    service::trace_legacy_receipt::VerifiedLegacyPurgeReceipt,
};

/// Ledger version recorded for this maintenance migration.
pub const MIGRATION_VERSION: &str = "drop_legacy_capture";

/// Children before parents; `CASCADE` makes the order forgiving, the order is
/// kept explicit for the single-transaction DROP.
const LEGACY_TABLES: [&str; 12] = [
    "agent_capture_blob_ref",
    "agent_capture_file_op",
    "agent_capture_event",
    "agent_capture_checkpoint",
    "agent_capture_source_stream",
    "agent_capture_stream_blob",
    "agent_capture_ingest_receipt",
    "agent_capture_access_audit",
    "agent_capture_tombstone",
    "agent_capture_deletion_ledger",
    "agent_capture_blob",
    "agent_capture_session",
];

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        refuse_unverified_up(manager).await
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Forward-only: a dropped legacy schema is not recreated.
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropOutcome {
    pub dropped: bool,
    pub already_applied: bool,
}

#[derive(Debug, Error)]
pub enum DropError {
    #[error("the purge journal is no longer frozen")]
    JournalNotFrozen,
    #[error("the receipt fence is no longer active")]
    FenceInactive,
    #[error("the frozen inventory changed since the receipt was issued")]
    InventoryChanged,
    #[error("the guarded DROP failed: {0}")]
    Database(String),
}

impl From<sea_orm::DbErr> for DropError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Database(error.to_string())
    }
}

/// Whether the guarded maintenance migration already ran. The CLI checks this
/// before verifying a receipt: after the DROP the legacy tables are gone, so a
/// repeated call must be a clean no-op.
pub async fn maintenance_applied(db: &DatabaseConnection) -> Result<bool, DropError> {
    ledger_applied(db).await
}

async fn ledger_applied(db: &DatabaseConnection) -> Result<bool, DropError> {
    db.execute_unprepared(&format!(
        "CREATE TABLE IF NOT EXISTS {MAINTENANCE_LEDGER} (
            version varchar(255) PRIMARY KEY,
            applied_at timestamptz NOT NULL DEFAULT now()
        )"
    ))
    .await?;
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            format!("SELECT 1 AS present FROM {MAINTENANCE_LEDGER} WHERE version = $1"),
            [MIGRATION_VERSION.into()],
        ))
        .await?;
    Ok(row.is_some())
}

async fn validate_receipt(
    db: &DatabaseConnection,
    receipt: &VerifiedLegacyPurgeReceipt,
) -> Result<(), DropError> {
    let journal = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT state FROM trace_legacy_purge WHERE id = $1",
            [receipt.journal_id().into()],
        ))
        .await?;
    match journal {
        Some(row) if row.try_get::<String>("", "state").as_deref() == Ok("frozen") => {}
        _ => return Err(DropError::JournalNotFrozen),
    }
    let fence = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT 1 AS present FROM trace_legacy_writer_fence
             WHERE id = $1 AND purge_id = $2 AND active",
            [receipt.fence_id().into(), receipt.journal_id().into()],
        ))
        .await?;
    if fence.is_none() {
        return Err(DropError::FenceInactive);
    }
    // The inventory digest must still match the receipt: an item changed or a
    // new object appeared after MTR-44 verified the journal.
    let rows = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT object_key FROM trace_legacy_purge_item
             WHERE purge_id = $1 AND deleted_at IS NOT NULL ORDER BY object_key",
            [receipt.journal_id().into()],
        ))
        .await?;
    let mut hasher = Sha256::new();
    for row in &rows {
        let key: String = row.try_get("", "object_key")?;
        hasher.update(key.as_bytes());
        hasher.update(b"\n");
    }
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if digest != receipt.inventory_digest() {
        return Err(DropError::InventoryChanged);
    }
    Ok(())
}

/// Consume a verified receipt, drop the 12 legacy tables in one transaction
/// and record the maintenance ledger row only after the DROP succeeded.
pub async fn execute(
    db: &DatabaseConnection,
    receipt: &VerifiedLegacyPurgeReceipt,
) -> Result<DropOutcome, DropError> {
    if ledger_applied(db).await? {
        return Ok(DropOutcome {
            dropped: false,
            already_applied: true,
        });
    }
    validate_receipt(db, receipt).await?;
    let txn = db.begin().await?;
    txn.execute_unprepared(&format!(
        "CREATE TABLE IF NOT EXISTS {MAINTENANCE_LEDGER} (
            version varchar(255) PRIMARY KEY,
            applied_at timestamptz NOT NULL DEFAULT now()
        )"
    ))
    .await?;
    // One transaction: either every legacy table is gone and the ledger row is
    // written, or nothing changed.
    for table in LEGACY_TABLES {
        txn.execute_unprepared(&format!("DROP TABLE IF EXISTS {table} CASCADE"))
            .await?;
    }
    txn.execute_unprepared(&format!(
        "INSERT INTO {MAINTENANCE_LEDGER} (version) VALUES ('{MIGRATION_VERSION}')"
    ))
    .await?;
    txn.commit().await?;
    Ok(DropOutcome {
        dropped: true,
        already_applied: false,
    })
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Statement};
    use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};

    use super::{DropError, LEGACY_TABLES, MIGRATION_VERSION, Migration, execute};
    use crate::{
        jupiter::{
            migration::{Migrator, traces_maintenance::TracesMaintenanceMigrator},
            service::trace_legacy_receipt::verify_purge_complete,
            tests::test_db_connection,
        },
        orbit::{ObjectStoreAdapter, factory::ObjectStorageFactory},
        orbit_api::factory::ObjectStorageConfig,
    };

    const JOURNAL: &str = "21212121-2121-4121-8121-212121212121";
    const FENCE: &str = "31313131-3131-4131-8131-313131313131";

    async fn seeded() -> (
        tempfile::TempDir,
        sea_orm::DatabaseConnection,
        ObjectStoreAdapter,
        String,
    ) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let root = temp_dir.path().join("objects");
        std::fs::create_dir_all(&root).expect("root");
        let db = test_db_connection(temp_dir.path()).await;
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .expect("migrations");
        let cfg: ObjectStorageConfig = toml::from_str(&format!(
            "storage_type = \"local\"\n[local]\nroot_dir = \"{}\"\n",
            root.display()
        ))
        .expect("storage");
        let adapter = ObjectStorageFactory::build_adapter(&cfg)
            .await
            .expect("adapter");
        db.execute_unprepared(
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind, completeness)
             VALUES ('deploy-x', 'tenant-x', 'repo', 'producer', 'drop-session', 'external_capture', 'complete')",
        )
        .await
        .expect("session");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge
             (id, backend_fingerprint, storage_location, deployment_id, manifest_digest, state)
             VALUES ('{JOURNAL}', 'local:{}', '{}', 'deploy-x', 'sha256:manifest', 'frozen')",
            root.display(),
            root.display()
        ))
        .await
        .expect("journal");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge_item (id, purge_id, scope, object_key, deleted_at)
             VALUES ('41414141-4141-4141-8141-414141414141', '{JOURNAL}', 'deploy-x', 'agent/raw/aa', now())"
        ))
        .await
        .expect("item");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_writer_fence (id, purge_id, deployment_id, active)
             VALUES ('{FENCE}', '{JOURNAL}', 'deploy-x', TRUE)"
        ))
        .await
        .expect("fence");
        let manifest = format!(
            r#"{{"bucket_scope":"mega2-legacy/","deployment_id":"deploy-x","backend_kind":"local","storage_location":"{}","historical_locations":["local:{}"],"owners":[{{"owner":"owner-a","deployment_id":"deploy-x","tenant_id":"tenant-x","receipt_id":"receipt-a","complete":true}}],"orphan_prefixes":[]}}"#,
            root.display(),
            root.display()
        );
        (temp_dir, db, adapter, manifest)
    }

    #[tokio::test]
    async fn db_contract() {
        // AC-1/AC-5: only the explicit maintenance migrator carries the DROP;
        // the normal startup list keeps the historic add migrations.
        let normal: Vec<String> = Migrator::migrations()
            .iter()
            .map(|migration| migration.name().to_string())
            .collect();
        assert!(!normal.iter().any(|name| name == MIGRATION_VERSION));
        assert!(
            normal
                .iter()
                .any(|name| name == "m20260913_000100_add_agent_capture_tables")
        );
        let maintenance: Vec<String> = TracesMaintenanceMigrator::migrations()
            .iter()
            .map(|migration| migration.name().to_string())
            .collect();
        assert_eq!(maintenance, vec![MIGRATION_VERSION.to_owned()]);

        let (_temp, db, adapter, manifest) = seeded().await;
        // AC-6: an ordinary migrated startup keeps every legacy table.
        for table in LEGACY_TABLES {
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DbBackend::Postgres,
                    "SELECT to_regclass($1) IS NOT NULL AS present",
                    [table.into()],
                ))
                .await
                .expect("table lookup")
                .expect("table row");
            assert!(row.try_get::<bool>("", "present").unwrap(), "{table}");
        }

        let receipt = verify_purge_complete(&db, &adapter, JOURNAL, &manifest)
            .await
            .expect("receipt");
        let outcome = execute(&db, &receipt).await.expect("drop");
        assert!(outcome.dropped);
        assert!(!outcome.already_applied);
        for table in LEGACY_TABLES {
            let row = db
                .query_one_raw(Statement::from_sql_and_values(
                    sea_orm::DbBackend::Postgres,
                    "SELECT to_regclass($1) IS NOT NULL AS present",
                    [table.into()],
                ))
                .await
                .expect("table lookup")
                .expect("table row");
            assert!(
                !row.try_get::<bool>("", "present").unwrap(),
                "{table} survived"
            );
        }
        let ledger: i64 = db
            .query_one_raw(Statement::from_string(
                sea_orm::DbBackend::Postgres,
                format!("SELECT count(*) AS n FROM {}", super::MAINTENANCE_LEDGER),
            ))
            .await
            .expect("ledger")
            .expect("ledger row")
            .try_get("", "n")
            .unwrap();
        assert_eq!(ledger, 1, "one maintenance ledger row");
        // AC-8: a repeated maintenance call is idempotent.
        let again = execute(&db, &receipt).await.expect("idempotent drop");
        assert!(!again.dropped);
        assert!(again.already_applied);
        db.close().await.ok();
    }

    #[tokio::test]
    async fn db_contract_requires_the_current_receipt() {
        let (_temp, db, adapter, manifest) = seeded().await;
        let receipt = verify_purge_complete(&db, &adapter, JOURNAL, &manifest)
            .await
            .expect("receipt");
        // The fence is released after verification: the receipt is stale.
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_writer_fence SET active = FALSE, released_at = now() WHERE id = '{FENCE}'"
        ))
        .await
        .expect("release");
        let error = execute(&db, &receipt).await.expect_err("stale receipt");
        assert!(matches!(error, DropError::FenceInactive), "{error}");
        let row = db
            .query_one_raw(Statement::from_string(
                sea_orm::DbBackend::Postgres,
                "SELECT to_regclass('agent_capture_session') IS NOT NULL AS present",
            ))
            .await
            .expect("table lookup")
            .expect("table row");
        assert!(row.try_get::<bool>("", "present").unwrap(), "tables stay");
        // AC-7: the migration's down is an explicit no-op.
        let manager = SchemaManager::new(&db);
        Migration.down(&manager).await.expect("no-op down");
        let row = db
            .query_one_raw(Statement::from_string(
                sea_orm::DbBackend::Postgres,
                "SELECT to_regclass('agent_capture_session') IS NOT NULL AS present",
            ))
            .await
            .expect("table lookup")
            .expect("table row");
        assert!(
            row.try_get::<bool>("", "present").unwrap(),
            "down is a no-op"
        );
        db.close().await.ok();
    }
}
