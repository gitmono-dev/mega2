//! Cross-instance persistent legacy writer fence (MTR-33).
//!
//! Activating the fence re-runs the whole-database scope preflight inside the
//! same transaction, so a scope that appeared after the manifest freeze blocks
//! the activation instead of being silently included. The fence row is the
//! durable barrier: while it is active the trigger refuses runtime DML on all
//! 12 legacy tables and the HTTP ingest surface answers a stable rejection.
//! Activating an already-active fence for the same journal is idempotent.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, Statement, TransactionTrait,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    callisto::{trace_legacy_purge, trace_legacy_writer_fence},
    jupiter::service::trace_legacy_scope_guard::{
        AggregateScopeReceipt, ScopeGuardError, preflight_legacy_scopes,
    },
};

/// Stable code the legacy capture HTTP surface answers while fenced.
pub const LEGACY_CAPTURE_FENCED: &str = "legacy_capture_fenced";

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FenceReceipt {
    pub journal_id: String,
    pub fence_id: String,
    pub aggregate_digest: String,
    pub receipt_digest: String,
    pub already_active: bool,
}

#[derive(Debug, Error)]
pub enum LegacyFenceError {
    #[error("the purge journal does not exist")]
    JournalMissing,
    #[error("the purge journal is not in the frozen state")]
    JournalNotFrozen,
    #[error("the caller is not a member of the legacy table maintenance role")]
    NotMaintenance,
    #[error("the whole-database preflight failed: {0}")]
    Preflight(#[from] ScopeGuardError),
    #[error("the legacy capture write surface is fenced")]
    Fenced,
    #[error("the fence cannot be written: {0}")]
    Database(String),
}

impl From<sea_orm::DbErr> for LegacyFenceError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Database(error.to_string())
    }
}

/// The current fence state, if any. The barrier reads this relation directly.
pub async fn fence_state(
    db: &DatabaseConnection,
) -> Result<Option<trace_legacy_writer_fence::Model>, LegacyFenceError> {
    trace_legacy_writer_fence::Entity::find()
        .one(db)
        .await
        .map_err(LegacyFenceError::from)
}

/// Fail closed for the legacy capture HTTP write surface while fenced.
pub async fn ensure_unfenced(db: &DatabaseConnection) -> Result<(), LegacyFenceError> {
    if fence_state(db).await?.is_some_and(|fence| fence.active) {
        return Err(LegacyFenceError::Fenced);
    }
    Ok(())
}

async fn maintenance_role(db: &DatabaseConnection) -> Result<String, LegacyFenceError> {
    // The maintenance role is the owner of the legacy tables; ownership is
    // transferred by MTR-61 and the app role never owns them afterwards.
    let row = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT tableowner FROM pg_tables
             WHERE schemaname = current_schema() AND tablename = 'agent_capture_session'",
        ))
        .await?;
    row.and_then(|row| row.try_get::<String>("", "tableowner").ok())
        .ok_or(LegacyFenceError::NotMaintenance)
}

pub(crate) async fn caller_is_maintenance<C: ConnectionTrait>(
    db: &C,
    role: &str,
) -> Result<bool, LegacyFenceError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_has_role(current_user, $1, 'MEMBER') AS member",
            [role.into()],
        ))
        .await?;
    Ok(row
        .map(|row| row.try_get::<bool>("", "member").unwrap_or(false))
        .unwrap_or(false))
}

fn receipt_digest(journal: &str, fence: &str, aggregate: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(journal.as_bytes());
    hasher.update(b"\n");
    hasher.update(fence.as_bytes());
    hasher.update(b"\n");
    hasher.update(aggregate.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Activate the persistent fence for one frozen journal.
pub async fn activate_fence(
    db: &DatabaseConnection,
    journal_id: &str,
    raw_manifest: &str,
) -> Result<FenceReceipt, LegacyFenceError> {
    // AC-6: only the maintenance identity may toggle the fence.
    let role = maintenance_role(db).await?;
    if !caller_is_maintenance(db, &role).await? {
        return Err(LegacyFenceError::NotMaintenance);
    }
    let txn = db.begin().await?;
    let journal = trace_legacy_purge::Entity::find_by_id(journal_id)
        .one(&txn)
        .await?
        .ok_or(LegacyFenceError::JournalMissing)?;
    if journal.state != "frozen" {
        return Err(LegacyFenceError::JournalNotFrozen);
    }
    // AC-8: the activation re-runs the whole-database authorization inside
    // this transaction; an unknown or new scope produces no receipt.
    let receipt: AggregateScopeReceipt = preflight_legacy_scopes(&txn, raw_manifest).await?;
    // AC-7: the same journal's active fence is reused instead of duplicated.
    if let Some(existing) = trace_legacy_writer_fence::Entity::find()
        .filter(trace_legacy_writer_fence::Column::PurgeId.eq(journal_id))
        .one(&txn)
        .await?
    {
        if existing.active {
            txn.commit().await?;
            let digest = receipt_digest(journal_id, &existing.id, &receipt.digest);
            return Ok(FenceReceipt {
                journal_id: journal_id.to_owned(),
                fence_id: existing.id,
                aggregate_digest: receipt.digest,
                receipt_digest: digest,
                already_active: true,
            });
        }
        txn.rollback().await.ok();
        return Err(LegacyFenceError::JournalNotFrozen);
    }
    let fence_id = uuid::Uuid::new_v4().to_string();
    let fence = trace_legacy_writer_fence::ActiveModel {
        id: sea_orm::Set(fence_id.clone()),
        purge_id: sea_orm::Set(journal_id.to_owned()),
        deployment_id: sea_orm::Set(journal.deployment_id.clone()),
        active: sea_orm::Set(true),
        activated_at: sea_orm::Set(chrono::Utc::now().fixed_offset()),
        released_at: sea_orm::Set(None),
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    // AC-1: the active row is durable; read it back rather than trusting the
    // insert result.
    let persisted = trace_legacy_writer_fence::Entity::find_by_id(&fence.id)
        .one(db)
        .await?
        .filter(|row| row.active)
        .ok_or(LegacyFenceError::JournalNotFrozen)?;
    let digest = receipt_digest(journal_id, &persisted.id, &receipt.digest);
    Ok(FenceReceipt {
        journal_id: journal_id.to_owned(),
        fence_id: persisted.id,
        aggregate_digest: receipt.digest,
        receipt_digest: digest,
        already_active: false,
    })
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Statement};

    use super::{LegacyFenceError, activate_fence, ensure_unfenced, fence_state};
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn seeded() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        (temp_dir, db)
    }

    const JOURNAL: &str = "44444444-4444-4444-8444-444444444444";
    const MANIFEST: &str = r#"{
        "bucket_scope": "mega2-legacy/",
        "owners": [{"owner": "owner-a", "deployment_id": "deploy-f", "tenant_id": "tenant-f", "receipt_id": "receipt-a", "complete": true}],
        "orphan_prefixes": []
    }"#;

    async fn seed_journal(db: &sea_orm::DatabaseConnection) {
        db.execute_unprepared(
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind, completeness)
             VALUES ('deploy-f', 'tenant-f', 'repo', 'producer', 'fence-unit', 'external_capture', 'complete')",
        )
        .await
        .expect("legacy session");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge
             (id, backend_fingerprint, storage_location, deployment_id, manifest_digest, state)
             VALUES ('{JOURNAL}', 'local:/srv', 'local:/srv', 'deploy-f', 'sha256:manifest', 'frozen')"
        ))
        .await
        .expect("journal");
    }

    #[tokio::test]
    async fn legacy_fence_activation_and_http_guard() {
        let (_temp_dir, db) = seeded().await;
        seed_journal(&db).await;
        // AC-5: unfenced writes pass the guard.
        ensure_unfenced(&db).await.expect("unfenced guard");
        let first = activate_fence(&db, JOURNAL, MANIFEST)
            .await
            .expect("activate");
        assert!(!first.already_active);
        assert!(first.receipt_digest.len() == 64);
        assert!(
            fence_state(&db)
                .await
                .expect("state")
                .is_some_and(|f| f.active)
        );
        // AC-5: the HTTP guard fails closed with the stable fenced state.
        assert!(matches!(
            ensure_unfenced(&db).await.expect_err("fenced"),
            LegacyFenceError::Fenced
        ));
        // AC-7: idempotent.
        let again = activate_fence(&db, JOURNAL, MANIFEST).await.expect("again");
        assert!(again.already_active);
        assert_eq!(first.fence_id, again.fence_id);
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_fence_requires_the_maintenance_role() {
        let (_temp_dir, db) = seeded().await;
        seed_journal(&db).await;
        let role = format!("mega2_m33_app_{}", std::process::id());
        db.execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
            .await
            .ok();
        db.execute_unprepared(&format!("CREATE ROLE {role} NOLOGIN"))
            .await
            .expect("app role");
        // AC-6: the caller is resolved from the session role, so an unrelated
        // role is not a member of the owner (maintenance) role. `SET LOCAL`
        // inside a rolled-back transaction keeps the pooled connections clean.
        use sea_orm::TransactionTrait;
        let owner: String = db
            .query_one_raw(Statement::from_string(
                sea_orm::DbBackend::Postgres,
                "SELECT tableowner FROM pg_tables
                 WHERE schemaname = current_schema() AND tablename = 'agent_capture_session'",
            ))
            .await
            .expect("owner lookup")
            .expect("owner row")
            .try_get("", "tableowner")
            .expect("owner");
        let txn = db.begin().await.expect("txn");
        txn.execute_unprepared(&format!("SET LOCAL ROLE {role}"))
            .await
            .expect("set role");
        assert!(
            !super::caller_is_maintenance(&txn, &owner)
                .await
                .expect("membership"),
            "an unrelated role must not count as the maintenance role"
        );
        txn.rollback().await.expect("rollback");
        db.execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
            .await
            .expect("drop role");
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_fence_refuses_unknown_journal_states() {
        let (_temp_dir, db) = seeded().await;
        seed_journal(&db).await;
        let error = activate_fence(&db, "55555555-5555-4555-8555-555555555555", MANIFEST)
            .await
            .expect_err("missing journal");
        assert!(matches!(error, LegacyFenceError::JournalMissing));
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_purge SET state = 'complete' WHERE id = '{JOURNAL}'"
        ))
        .await
        .expect("state");
        let error = activate_fence(&db, JOURNAL, MANIFEST)
            .await
            .expect_err("completed journal");
        assert!(matches!(error, LegacyFenceError::JournalNotFrozen));
        let rows = db
            .query_one_raw(Statement::from_string(
                sea_orm::DbBackend::Postgres,
                "SELECT count(*) AS n FROM trace_legacy_writer_fence",
            ))
            .await
            .expect("fence count")
            .expect("fence row");
        assert_eq!(rows.try_get::<i64>("", "n").unwrap(), 0);
        db.close().await.ok();
    }
}
