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
    #[error("the old app role still has an inherited {privilege} grant on {table}")]
    InheritedGrant { table: String, privilege: String },
    #[error("the revoked app role still executed a {privilege} write on {table}")]
    RevocationNotEffective { table: String, privilege: String },
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

/// The verified state after the three effective DML privileges are revoked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyDmlRevocation {
    pub app_role: String,
    pub tables_revoked: usize,
    pub insert_revoked: bool,
    pub update_revoked: bool,
    pub delete_revoked: bool,
    pub public_insert_revoked: bool,
    pub public_update_revoked: bool,
    pub public_delete_revoked: bool,
}

async fn effective_privilege(
    db: &DatabaseConnection,
    app_role: &str,
    table: &str,
    privilege: &str,
) -> Result<bool, LegacyRoleError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT has_table_privilege($1, $2, $3) AS allowed",
            [app_role.into(), table.into(), privilege.into()],
        ))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: app_role.to_owned(),
            reason: error.to_string(),
        })?;
    Ok(row
        .map(|row| row.try_get::<bool>("", "allowed").unwrap_or(false))
        .unwrap_or(false))
}

async fn public_privilege(
    db: &DatabaseConnection,
    table: &str,
    privilege: &str,
) -> Result<bool, LegacyRoleError> {
    let row = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT has_table_privilege('public', $1, $2) AS allowed",
            [table.into(), privilege.into()],
        ))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: "PUBLIC".to_owned(),
            reason: error.to_string(),
        })?;
    Ok(row
        .map(|row| row.try_get::<bool>("", "allowed").unwrap_or(false))
        .unwrap_or(false))
}

/// Revoke the three effective DML privileges from the old app role and from
/// PUBLIC, then prove the revocation with `has_table_privilege` and a
/// rolled-back write that must fail with `permission denied` while the fence
/// is inactive.
pub async fn revoke_legacy_dml(
    db: &DatabaseConnection,
    plan: &LegacyRolePlan,
) -> Result<LegacyDmlRevocation, LegacyRoleError> {
    if !plain_identifier(&plan.app_role) || !plain_identifier(&plan.maintenance_role) {
        return Err(LegacyRoleError::InvalidRoleName);
    }
    if plan.app_role == plan.maintenance_role {
        return Err(LegacyRoleError::SharedRole);
    }
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
    // The revocation is only meaningful while no fence hides the result.
    let active = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*) AS n FROM trace_legacy_writer_fence WHERE active",
        ))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: plan.maintenance_role.clone(),
            reason: error.to_string(),
        })?
        .map(|row| row.try_get::<i64>("", "n").unwrap_or(0))
        .unwrap_or(0);
    if active > 0 {
        return Err(LegacyRoleError::AppRoleStillPrivileged {
            reason: "an active fence hides the DML revocation".to_owned(),
        });
    }
    for table in LEGACY_TABLES {
        db.execute_unprepared(&format!(
            "REVOKE INSERT, UPDATE, DELETE ON {table} FROM {}",
            plan.app_role
        ))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: plan.app_role.clone(),
            reason: format!("{table}: {error}"),
        })?;
        db.execute_unprepared(&format!(
            "REVOKE INSERT, UPDATE, DELETE ON {table} FROM PUBLIC"
        ))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: "PUBLIC".to_owned(),
            reason: format!("{table}: {error}"),
        })?;
    }
    // Inherited memberships can re-grant what was revoked from the role
    // directly; this must fail loudly instead of reporting success.
    for table in LEGACY_TABLES {
        for privilege in ["INSERT", "UPDATE", "DELETE"] {
            if effective_privilege(db, &plan.app_role, table, privilege).await? {
                return Err(LegacyRoleError::InheritedGrant {
                    table: table.to_owned(),
                    privilege: privilege.to_owned(),
                });
            }
        }
    }
    // The real oracle: an inactive fence plus a rolled-back write by the app
    // role must be refused by the permission system, not by the barrier.
    // The probe is schema-qualified: the old app role may legitimately have
    // no `USAGE` on the schema, and the oracle here is the table DML grant.
    let schema = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT current_schema() AS schema",
        ))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: plan.app_role.clone(),
            reason: error.to_string(),
        })?
        .and_then(|row| row.try_get::<String>("", "schema").ok())
        .unwrap_or_else(|| "public".to_owned());
    let txn = db
        .begin()
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: plan.app_role.clone(),
            reason: error.to_string(),
        })?;
    txn.execute_unprepared(&format!("SET LOCAL ROLE {}", plan.app_role))
        .await
        .map_err(|error| LegacyRoleError::TransferFailed {
            role: plan.app_role.clone(),
            reason: error.to_string(),
        })?;
    let denied = txn
        .execute_unprepared(&format!(
            "INSERT INTO \"{schema}\".agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind)
             VALUES ('d', 't', 'r', 'p', 'revocation-probe', 'external_capture')"
        ))
        .await;
    let refused_by_permission = match &denied {
        Err(error) => {
            let text = error.to_string().to_ascii_lowercase();
            text.contains("permission denied") || text.contains("42501")
        }
        Ok(_) => false,
    };
    let observed = match &denied {
        Ok(_) => "the write succeeded".to_owned(),
        Err(error) => error.to_string(),
    };
    let _ = txn.rollback().await;
    if !refused_by_permission {
        return Err(LegacyRoleError::RevocationNotEffective {
            table: format!("agent_capture_session ({observed})"),
            privilege: "INSERT".to_owned(),
        });
    }
    Ok(LegacyDmlRevocation {
        app_role: plan.app_role.clone(),
        tables_revoked: LEGACY_TABLES.len(),
        insert_revoked: true,
        update_revoked: true,
        delete_revoked: true,
        public_insert_revoked: true,
        public_update_revoked: true,
        public_delete_revoked: true,
    })
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

#[cfg(test)]
mod revocation_tests {
    use sea_orm::ConnectionTrait;

    use super::{
        LEGACY_TABLES, LegacyRoleError, LegacyRolePlan, prepare_legacy_roles, revoke_legacy_dml,
    };
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn seeded_db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        (temp_dir, db)
    }

    async fn create_role(db: &sea_orm::DatabaseConnection, role: &str) {
        db.execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
            .await
            .ok();
        db.execute_unprepared(&format!("CREATE ROLE {role} NOLOGIN"))
            .await
            .expect("create role");
    }

    async fn cleanup(db: &sea_orm::DatabaseConnection, roles: &[&str]) {
        for table in LEGACY_TABLES {
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

    async fn table_privilege(
        db: &sea_orm::DatabaseConnection,
        role: &str,
        table: &str,
        privilege: &str,
    ) -> bool {
        db.query_one_raw(sea_orm::Statement::from_sql_and_values(
            sea_orm::DbBackend::Postgres,
            "SELECT has_table_privilege($1, $2, $3) AS allowed",
            [role.into(), table.into(), privilege.into()],
        ))
        .await
        .expect("privilege lookup")
        .expect("privilege row")
        .try_get::<bool>("", "allowed")
        .unwrap_or(true)
    }

    #[tokio::test]
    async fn legacy_dml_revocation_removes_effective_grants() {
        let (_temp_dir, db) = seeded_db().await;
        let app = format!("mega2_l67_app_{}", std::process::id());
        let maintenance = format!("mega2_l67_maint_{}", std::process::id());
        create_role(&db, &app).await;
        create_role(&db, &maintenance).await;
        let plan = LegacyRolePlan {
            app_role: app.clone(),
            maintenance_role: maintenance.clone(),
        };
        prepare_legacy_roles(&db, &plan).await.expect("ownership");
        // Give the old app role real DML, directly and through PUBLIC, then
        // revoke it.
        for table in LEGACY_TABLES {
            db.execute_unprepared(&format!("GRANT INSERT, UPDATE, DELETE ON {table} TO {app}"))
                .await
                .expect("grant app");
            db.execute_unprepared(&format!(
                "GRANT INSERT, UPDATE, DELETE ON {table} TO PUBLIC"
            ))
            .await
            .expect("grant public");
        }
        let revoked = revoke_legacy_dml(&db, &plan).await.expect("revoke");
        assert_eq!(revoked.tables_revoked, 12);
        for table in LEGACY_TABLES {
            for privilege in ["INSERT", "UPDATE", "DELETE"] {
                assert!(
                    !table_privilege(&db, &app, table, privilege).await,
                    "{app} still has {privilege} on {table}"
                );
                let public_allowed = db
                    .query_one_raw(sea_orm::Statement::from_sql_and_values(
                        sea_orm::DbBackend::Postgres,
                        "SELECT has_table_privilege('public', $1, $2) AS allowed",
                        [table.into(), privilege.into()],
                    ))
                    .await
                    .expect("public privilege")
                    .expect("public row")
                    .try_get::<bool>("", "allowed")
                    .unwrap_or(true);
                assert!(!public_allowed, "PUBLIC still has {privilege} on {table}");
            }
        }
        cleanup(&db, &[&app, &maintenance]).await;
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_dml_revocation_rejects_inherited_grants_and_active_fence() {
        let (_temp_dir, db) = seeded_db().await;
        let app = format!("mega2_l67_inherit_{}", std::process::id());
        let parent = format!("mega2_l67_parent_{}", std::process::id());
        let maintenance = format!("mega2_l67_maint2_{}", std::process::id());
        for role in [&app, &parent, &maintenance] {
            create_role(&db, role).await;
        }
        let plan = LegacyRolePlan {
            app_role: app.clone(),
            maintenance_role: maintenance.clone(),
        };
        prepare_legacy_roles(&db, &plan).await.expect("ownership");
        db.execute_unprepared(&format!(
            "GRANT INSERT ON agent_capture_session TO {parent}"
        ))
        .await
        .expect("grant parent");
        db.execute_unprepared(&format!("GRANT {parent} TO {app}"))
            .await
            .expect("membership");
        let error = revoke_legacy_dml(&db, &plan)
            .await
            .expect_err("an inherited grant must block");
        assert!(matches!(error, LegacyRoleError::InheritedGrant { .. }));

        db.execute_unprepared(&format!("REVOKE {parent} FROM {app}"))
            .await
            .expect("revoke membership");
        db.execute_unprepared(&format!("GRANT INSERT ON agent_capture_session TO {app}"))
            .await
            .expect("grant app");
        db.execute_unprepared(
            "INSERT INTO trace_legacy_purge
             (id, backend_fingerprint, storage_location, deployment_id, manifest_digest, state)
             VALUES ('11111111-1111-4111-8111-111111111111', 'b', 'l', 'd', 'm', 'frozen')",
        )
        .await
        .expect("journal");
        db.execute_unprepared(
            "INSERT INTO trace_legacy_writer_fence (id, purge_id, deployment_id, active)
             VALUES ('22222222-2222-4222-8222-222222222222', '11111111-1111-4111-8111-111111111111', 'd', TRUE)",
        )
        .await
        .expect("fence");
        let error = revoke_legacy_dml(&db, &plan)
            .await
            .expect_err("an active fence must block the revocation");
        assert!(matches!(
            error,
            LegacyRoleError::AppRoleStillPrivileged { .. }
        ));
        cleanup(&db, &[&app, &parent, &maintenance]).await;
        db.close().await.ok();
    }
}
