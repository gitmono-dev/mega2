//! Legacy writer DB identity and ownership preparation (MTR-61).
//!
//! The deployment DBA runs this against the existing maintenance connection
//! before the fence is activated: ownership of the 12 legacy tables and of the
//! barrier function moves to a dedicated maintenance role, and the old app
//! role is proved to be a plain role (no superuser, no BYPASSRLS, no
//! CREATEROLE) that can neither ALTER the tables nor disable their triggers.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The 12 legacy Capture tables, in FK-safe name order.
pub const LEGACY_TABLES: [&str; 12] = [
    "agent_capture_session",
    "agent_capture_event",
    "agent_capture_source_stream",
    "agent_capture_checkpoint",
    "agent_capture_file_op",
    "agent_capture_blob",
    "agent_capture_blob_ref",
    "agent_capture_ingest_receipt",
    "agent_capture_stream_blob",
    "agent_capture_access_audit",
    "agent_capture_tombstone",
    "agent_capture_deletion_ledger",
];

pub const BARRIER_FUNCTION: &str = "trace_legacy_write_fence";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LegacyRolePlan {
    pub app_role: String,
    pub maintenance_role: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LegacyRoleError {
    #[error("the role names must be distinct plain identifiers")]
    InvalidRoleName,
    #[error("the app role and the maintenance role must differ")]
    SharedRole,
    #[error("the role {role} does not exist")]
    RoleMissing { role: String },
    #[error("the app role {role} is a superuser, has BYPASSRLS or has CREATEROLE")]
    PrivilegedAppRole { role: String },
    #[error("ownership transfer to {role} failed: {reason}")]
    TransferFailed { role: String, reason: String },
    #[error("the old app role can still change the legacy schema: {reason}")]
    AppRoleStillPrivileged { reason: String },
}

impl From<sea_orm::DbErr> for LegacyRoleError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::TransferFailed {
            role: "database".to_owned(),
            reason: error.to_string(),
        }
    }
}

/// The verified state after ownership preparation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyRolePreparation {
    pub app_role: String,
    pub maintenance_role: String,
    pub tables_transferred: usize,
    pub barrier_transferred: bool,
    pub app_role_plain: bool,
    pub app_cannot_alter: bool,
    pub app_cannot_disable_trigger: bool,
}

fn plain_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !value.as_bytes()[0].is_ascii_digit()
}

async fn role_attributes(
    db: &DatabaseConnection,
    role: &str,
) -> Result<Option<(bool, bool, bool)>, LegacyRoleError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT rolsuper, rolbypassrls, rolcreaterole FROM pg_roles WHERE rolname = $1",
            [role.into()],
        ))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: role.to_owned(),
            reason: error.to_string(),
        })?;
    Ok(row.map(|row| {
        (
            row.try_get::<bool>("", "rolsuper").unwrap_or(false),
            row.try_get::<bool>("", "rolbypassrls").unwrap_or(false),
            row.try_get::<bool>("", "rolcreaterole").unwrap_or(false),
        )
    }))
}

async fn expect_failure(
    db: &DatabaseConnection,
    app_role: &str,
    sql: &str,
) -> Result<bool, LegacyRoleError> {
    // Runs the attempted mutation inside a transaction that is always rolled
    // back, so a successful attempt leaves no trace.
    let txn = db
        .begin()
        .await
        .map_err(|error| LegacyRoleError::AppRoleStillPrivileged {
            reason: error.to_string(),
        })?;
    txn.execute_unprepared(&format!("SET LOCAL ROLE {app_role}"))
        .await
        .map_err(|error| LegacyRoleError::AppRoleStillPrivileged {
            reason: error.to_string(),
        })?;
    let result = txn.execute_unprepared(sql).await;
    let _ = txn.rollback().await;
    Ok(result.is_err())
}

/// Prepare ownership and prove the old app role cannot change the schema.
/// Idempotent: ownership already on the maintenance role is transferred again
/// harmlessly.
pub async fn prepare_legacy_roles(
    db: &DatabaseConnection,
    plan: &LegacyRolePlan,
) -> Result<LegacyRolePreparation, LegacyRoleError> {
    if !plain_identifier(&plan.app_role) || !plain_identifier(&plan.maintenance_role) {
        return Err(LegacyRoleError::InvalidRoleName);
    }
    // AC-8: the maintenance identity is never the old app identity.
    if plan.app_role == plan.maintenance_role {
        return Err(LegacyRoleError::SharedRole);
    }
    let app =
        role_attributes(db, &plan.app_role)
            .await?
            .ok_or_else(|| LegacyRoleError::RoleMissing {
                role: plan.app_role.clone(),
            })?;
    role_attributes(db, &plan.maintenance_role)
        .await?
        .ok_or_else(|| LegacyRoleError::RoleMissing {
            role: plan.maintenance_role.clone(),
        })?;
    // AC-3/AC-4/AC-5: the old app role is a plain role.
    if app.0 || app.1 || app.2 {
        return Err(LegacyRoleError::PrivilegedAppRole {
            role: plan.app_role.clone(),
        });
    }
    // AC-1: table ownership moves to the maintenance role.
    for table in LEGACY_TABLES {
        db.execute_unprepared(&format!(
            "ALTER TABLE {table} OWNER TO {}",
            plan.maintenance_role
        ))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: plan.maintenance_role.clone(),
            reason: format!("{table}: {error}"),
        })?;
    }
    // AC-2: the barrier function is owned by the same maintenance role, so
    // the app role cannot replace it.
    db.execute_unprepared(&format!(
        "ALTER FUNCTION {BARRIER_FUNCTION}() OWNER TO {}",
        plan.maintenance_role
    ))
    .await
    .map_err(|error| LegacyRoleError::TransferFailed {
        role: plan.maintenance_role.clone(),
        reason: format!("{BARRIER_FUNCTION}: {error}"),
    })?;
    // AC-6/AC-7: the app role really lost the DDL and trigger switch.
    let app_cannot_alter = expect_failure(
        db,
        &plan.app_role,
        "ALTER TABLE agent_capture_session ADD COLUMN mtr61_probe integer",
    )
    .await?;
    let app_cannot_disable_trigger = expect_failure(
        db,
        &plan.app_role,
        "ALTER TABLE agent_capture_session DISABLE TRIGGER agent_capture_session_legacy_write_fence",
    )
    .await?;
    if !app_cannot_alter || !app_cannot_disable_trigger {
        return Err(LegacyRoleError::AppRoleStillPrivileged {
            reason: format!(
                "alter_blocked={app_cannot_alter} disable_trigger_blocked={app_cannot_disable_trigger}"
            ),
        });
    }
    Ok(LegacyRolePreparation {
        app_role: plan.app_role.clone(),
        maintenance_role: plan.maintenance_role.clone(),
        tables_transferred: LEGACY_TABLES.len(),
        barrier_transferred: true, // the ALTER FUNCTION above succeeded
        app_role_plain: !(app.0 || app.1 || app.2),
        app_cannot_alter,
        app_cannot_disable_trigger,
    })
}

#[cfg(test)]
mod tests {
    use sea_orm::ConnectionTrait;

    use super::{LegacyRoleError, LegacyRolePlan, prepare_legacy_roles};
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn seeded_db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        (temp_dir, db)
    }

    async fn create_role(db: &sea_orm::DatabaseConnection, role: &str, options: &str) {
        db.execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
            .await
            .ok();
        db.execute_unprepared(&format!("CREATE ROLE {role} NOLOGIN {options}"))
            .await
            .expect("create role");
    }

    async fn cleanup(db: &sea_orm::DatabaseConnection, roles: &[&str]) {
        for table in super::LEGACY_TABLES {
            let _ = db
                .execute_unprepared(&format!("ALTER TABLE {table} OWNER TO CURRENT_USER"))
                .await;
        }
        let _ = db
            .execute_unprepared("ALTER FUNCTION trace_legacy_write_fence() OWNER TO CURRENT_USER")
            .await;
        for role in roles {
            let _ = db
                .execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
                .await;
        }
    }

    #[tokio::test]
    async fn legacy_role_preparation_transfers_ownership() {
        let (_temp_dir, db) = seeded_db().await;
        let app = format!("mega2_l61_app_{}", std::process::id());
        let maintenance = format!("mega2_l61_maint_{}", std::process::id());
        create_role(&db, &app, "").await;
        create_role(&db, &maintenance, "").await;
        let plan = LegacyRolePlan {
            app_role: app.clone(),
            maintenance_role: maintenance.clone(),
        };
        let prepared = prepare_legacy_roles(&db, &plan)
            .await
            .expect("prepare roles");
        assert_eq!(prepared.tables_transferred, 12);
        assert!(prepared.barrier_transferred);
        assert!(prepared.app_role_plain);
        assert!(prepared.app_cannot_alter);
        assert!(prepared.app_cannot_disable_trigger);
        let owner = db
            .query_one_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "SELECT tableowner FROM pg_tables WHERE schemaname = current_schema() AND tablename = 'agent_capture_session'",
                [],
            ))
            .await
            .expect("owner lookup")
            .expect("owner row");
        assert_eq!(
            owner.try_get::<String>("", "tableowner").unwrap(),
            maintenance
        );
        cleanup(&db, &[&app, &maintenance]).await;
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_role_preparation_rejects_unsafe_plans() {
        let (_temp_dir, db) = seeded_db().await;
        let app = format!("mega2_l61_privileged_{}", std::process::id());
        create_role(&db, &app, "CREATEROLE").await;
        let maintenance = format!("mega2_l61_safe_{}", std::process::id());
        create_role(&db, &maintenance, "").await;

        let shared = LegacyRolePlan {
            app_role: app.clone(),
            maintenance_role: app.clone(),
        };
        assert_eq!(
            prepare_legacy_roles(&db, &shared)
                .await
                .expect_err("shared"),
            LegacyRoleError::SharedRole
        );

        let privileged = LegacyRolePlan {
            app_role: app.clone(),
            maintenance_role: maintenance.clone(),
        };
        assert_eq!(
            prepare_legacy_roles(&db, &privileged)
                .await
                .expect_err("privileged app role"),
            LegacyRoleError::PrivilegedAppRole { role: app.clone() }
        );

        let missing = LegacyRolePlan {
            app_role: "mega2_l61_missing_role".to_owned(),
            maintenance_role: maintenance.clone(),
        };
        assert!(matches!(
            prepare_legacy_roles(&db, &missing)
                .await
                .expect_err("missing"),
            LegacyRoleError::RoleMissing { .. }
        ));

        let invalid = LegacyRolePlan {
            app_role: "Bad-Role".to_owned(),
            maintenance_role: maintenance.clone(),
        };
        assert_eq!(
            prepare_legacy_roles(&db, &invalid)
                .await
                .expect_err("invalid"),
            LegacyRoleError::InvalidRoleName
        );
        cleanup(&db, &[&app, &maintenance]).await;
        db.close().await.ok();
    }
}
