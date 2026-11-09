//! Whole-database legacy-Capture scope authorization preflight (MTR-62).
//!
//! Before any irreversible deletion the guard enumerates every legacy scope
//! across all 12 Capture tables, requires each one to belong to exactly one
//! explicitly authorized owner with a complete per-owner receipt, and only
//! then produces the whole-database aggregate receipt. Unknown owners,
//! unauthorized scopes, untrusted orphan prefixes, a shared bucket root and
//! any attempt to include the old Git traces all block the preflight.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// One owner's explicit authorization and its completed receipt.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OwnerAuthorization {
    pub owner: String,
    pub deployment_id: String,
    pub tenant_id: String,
    pub receipt_id: String,
    pub complete: bool,
}

/// An object prefix without a database row, admissible only with a trusted
/// ownership record.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct OrphanPrefix {
    pub prefix: String,
    pub owner: String,
    pub evidence_digest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ScopeAuthorizationManifest {
    pub bucket_scope: String,
    #[serde(default)]
    pub delete_git_traces: bool,
    #[serde(default)]
    pub owners: Vec<OwnerAuthorization>,
    #[serde(default)]
    pub orphan_prefixes: Vec<OrphanPrefix>,
}

/// The whole-database receipt produced only when every owner receipt is
/// complete and every legacy scope is covered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateScopeReceipt {
    pub digest: String,
    pub scope_count: usize,
    pub owners: Vec<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ScopeGuardError {
    #[error("the scope manifest is not valid JSON: {0}")]
    InvalidManifest(String),
    #[error("the bucket scope is the shared root; shared buckets are not deletable")]
    SharedBucketScope,
    #[error("the manifest attempts to delete the old Git traces")]
    LegacyGitTraces,
    #[error("a legacy scope has no authorized owner: {deployment_id}/{tenant_id}")]
    UnknownOwner {
        deployment_id: String,
        tenant_id: String,
    },
    #[error("a legacy scope is outside the authorized set: {deployment_id}/{tenant_id}")]
    UnauthorizedScope {
        deployment_id: String,
        tenant_id: String,
    },
    #[error("owner {owner} has an incomplete removal receipt")]
    IncompleteOwnerReceipt { owner: String },
    #[error("a legacy scope is authorized by more than one owner: {deployment_id}/{tenant_id}")]
    DuplicateOwner {
        deployment_id: String,
        tenant_id: String,
    },
    #[error("the orphan prefix {prefix} has no trusted ownership record")]
    UntrustedOrphanPrefix { prefix: String },
    #[error("the legacy database cannot be read: {0}")]
    SourceUnreadable(String),
}

impl From<sea_orm::DbErr> for ScopeGuardError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::SourceUnreadable(error.to_string())
    }
}

async fn legacy_scopes(db: &DatabaseConnection) -> Result<Vec<(String, String)>, ScopeGuardError> {
    // AC-1: every legacy scope is enumerated from all 12 tables. Eight tables
    // carry the columns directly; the four child tables inherit them from
    // their parent session (and blob) row.
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT deployment_id, tenant_id FROM agent_capture_session
             UNION SELECT deployment_id, tenant_id FROM agent_capture_blob
             UNION SELECT deployment_id, tenant_id FROM agent_capture_ingest_receipt
             UNION SELECT deployment_id, tenant_id FROM agent_capture_access_audit
             UNION SELECT deployment_id, tenant_id FROM agent_capture_tombstone
             UNION SELECT deployment_id, tenant_id FROM agent_capture_deletion_ledger
             UNION SELECT s.deployment_id, s.tenant_id FROM agent_capture_event e
               JOIN agent_capture_session s ON s.id = e.capture_id
             UNION SELECT s.deployment_id, s.tenant_id FROM agent_capture_source_stream x
               JOIN agent_capture_session s ON s.id = x.capture_id
             UNION SELECT s.deployment_id, s.tenant_id FROM agent_capture_checkpoint c
               JOIN agent_capture_session s ON s.id = c.capture_id
             UNION SELECT s.deployment_id, s.tenant_id FROM agent_capture_file_op f
               JOIN agent_capture_session s ON s.id = f.capture_id
             UNION SELECT s.deployment_id, s.tenant_id FROM agent_capture_stream_blob sb
               JOIN agent_capture_session s ON s.id = sb.capture_id
             UNION SELECT b.deployment_id, b.tenant_id FROM agent_capture_blob_ref r
               JOIN agent_capture_blob b ON b.id = r.blob_id",
        ))
        .await?;
    let mut scopes = Vec::with_capacity(rows.len());
    for row in rows {
        let deployment: String = row
            .try_get("", "deployment_id")
            .map_err(|error| ScopeGuardError::SourceUnreadable(error.to_string()))?;
        let tenant: String = row
            .try_get("", "tenant_id")
            .map_err(|error| ScopeGuardError::SourceUnreadable(error.to_string()))?;
        scopes.push((deployment, tenant));
    }
    scopes.sort_unstable();
    scopes.dedup();
    Ok(scopes)
}

/// Run the whole-database preflight. No deletion happens here.
pub async fn preflight_legacy_scopes(
    db: &DatabaseConnection,
    raw_manifest: &str,
) -> Result<AggregateScopeReceipt, ScopeGuardError> {
    let manifest: ScopeAuthorizationManifest = serde_json::from_str(raw_manifest)
        .map_err(|error| ScopeGuardError::InvalidManifest(error.to_string()))?;
    // AC-7: a shared bucket root can never be part of a destructive scope.
    if manifest.bucket_scope.trim().is_empty() || manifest.bucket_scope.trim() == "/" {
        return Err(ScopeGuardError::SharedBucketScope);
    }
    // AC-8: the old Git traces are never deleted by this maintenance path.
    if manifest.delete_git_traces {
        return Err(ScopeGuardError::LegacyGitTraces);
    }
    // AC-6: every owner receipt must already be complete.
    for owner in &manifest.owners {
        if !owner.complete {
            return Err(ScopeGuardError::IncompleteOwnerReceipt {
                owner: owner.owner.clone(),
            });
        }
    }
    // AC-5: an orphan prefix is deletable only with a trusted ownership record.
    let owner_names: Vec<&str> = manifest
        .owners
        .iter()
        .map(|owner| owner.owner.as_str())
        .collect();
    for orphan in &manifest.orphan_prefixes {
        if orphan.prefix.trim().is_empty()
            || orphan.evidence_digest.trim().is_empty()
            || !owner_names.contains(&orphan.owner.as_str())
        {
            return Err(ScopeGuardError::UntrustedOrphanPrefix {
                prefix: orphan.prefix.clone(),
            });
        }
    }
    let scopes = legacy_scopes(db).await?;
    let mut authorized = Vec::with_capacity(scopes.len());
    for (deployment, tenant) in &scopes {
        let matching: Vec<&OwnerAuthorization> = manifest
            .owners
            .iter()
            .filter(|owner| &owner.deployment_id == deployment && &owner.tenant_id == tenant)
            .collect();
        match matching.len() {
            0 => {
                // AC-4: a scope outside the authorized set blocks the whole
                // preflight instead of deleting the known part first.
                return Err(ScopeGuardError::UnauthorizedScope {
                    deployment_id: deployment.clone(),
                    tenant_id: tenant.clone(),
                });
            }
            1 => {
                // AC-3: the scope exists but its ownership is unknown.
                if matching[0].owner.trim().is_empty() {
                    return Err(ScopeGuardError::UnknownOwner {
                        deployment_id: deployment.clone(),
                        tenant_id: tenant.clone(),
                    });
                }
                authorized.push(matching[0]);
            }
            _ => {
                return Err(ScopeGuardError::DuplicateOwner {
                    deployment_id: deployment.clone(),
                    tenant_id: tenant.clone(),
                });
            }
        }
    }
    // AC-2: every authorized scope must also exist in the database; an owner
    // receipt without rows is a conflict, not an empty success.
    for owner in &manifest.owners {
        if !scopes
            .iter()
            .any(|(d, t)| d == &owner.deployment_id && t == &owner.tenant_id)
        {
            return Err(ScopeGuardError::UnauthorizedScope {
                deployment_id: owner.deployment_id.clone(),
                tenant_id: owner.tenant_id.clone(),
            });
        }
    }
    // AC-6: the aggregate receipt binds the owner set, their receipt IDs and
    // the enumerated scopes. A later owner change invalidates this digest.
    let mut lines: Vec<String> = authorized
        .iter()
        .map(|owner| {
            format!(
                "{}|{}|{}|{}|{}",
                owner.owner, owner.deployment_id, owner.tenant_id, owner.receipt_id, owner.complete
            )
        })
        .collect();
    lines.sort_unstable();
    lines.dedup();
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(manifest.bucket_scope.as_bytes());
    let digest = hasher.finalize();
    Ok(AggregateScopeReceipt {
        digest: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
        scope_count: scopes.len(),
        owners: authorized
            .iter()
            .map(|owner| owner.owner.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use sea_orm::ConnectionTrait;
    use serde_json::json;

    use super::{
        AggregateScopeReceipt, ScopeAuthorizationManifest, ScopeGuardError, preflight_legacy_scopes,
    };
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn seeded_db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        for (deployment, tenant, client) in [
            ("deploy-a", "tenant-a", "session-a"),
            ("deploy-b", "tenant-b", "session-b"),
        ] {
            db.execute_raw(sea_orm::Statement::from_sql_and_values(
                sea_orm::DbBackend::Postgres,
                "INSERT INTO agent_capture_session
                 (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind, completeness)
                 VALUES ($1, $2, 'repo', 'producer', $3, 'external_capture', 'complete')",
                [deployment.into(), tenant.into(), client.into()],
            ))
            .await
            .expect("legacy session");
        }
        (temp_dir, db)
    }

    fn owners() -> serde_json::Value {
        json!([
            {"owner": "owner-a", "deployment_id": "deploy-a", "tenant_id": "tenant-a", "receipt_id": "receipt-a", "complete": true},
            {"owner": "owner-b", "deployment_id": "deploy-b", "tenant_id": "tenant-b", "receipt_id": "receipt-b", "complete": true}
        ])
    }

    fn manifest() -> serde_json::Value {
        json!({
            "bucket_scope": "mega2-legacy/",
            "delete_git_traces": false,
            "owners": owners(),
            "orphan_prefixes": [
                {"prefix": "mega2-legacy/agent/orphan", "owner": "owner-a", "evidence_digest": "sha256:orphan"}
            ]
        })
    }

    #[tokio::test]
    async fn legacy_db_scope_preflight_aggregates_complete_owners() {
        let (_temp_dir, db) = seeded_db().await;
        let receipt: AggregateScopeReceipt = preflight_legacy_scopes(&db, &manifest().to_string())
            .await
            .expect("preflight");
        assert_eq!(receipt.scope_count, 2);
        assert_eq!(
            receipt.owners,
            vec!["owner-a".to_owned(), "owner-b".to_owned()]
        );
        assert_eq!(receipt.digest.len(), 64);
        // The digest binds the owner set: a different receipt changes it.
        let mut changed = manifest();
        changed["owners"][0]["receipt_id"] = json!("receipt-a2");
        let changed_receipt = preflight_legacy_scopes(&db, &changed.to_string())
            .await
            .expect("changed preflight");
        assert_ne!(receipt.digest, changed_receipt.digest);
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_db_scope_preflight_blocks_unauthorized_and_unknown() {
        let (_temp_dir, db) = seeded_db().await;
        let mut missing = manifest();
        missing["owners"] = json!([{
            "owner": "owner-a", "deployment_id": "deploy-a", "tenant_id": "tenant-a",
            "receipt_id": "receipt-a", "complete": true
        }]);
        let error = preflight_legacy_scopes(&db, &missing.to_string())
            .await
            .expect_err("an unlisted scope must block");
        assert!(matches!(error, ScopeGuardError::UnauthorizedScope { .. }));

        let mut unknown = manifest();
        unknown["owners"][1]["owner"] = json!("  ");
        let error = preflight_legacy_scopes(&db, &unknown.to_string())
            .await
            .expect_err("an empty owner must block");
        assert!(matches!(error, ScopeGuardError::UnknownOwner { .. }));

        let mut incomplete = manifest();
        incomplete["owners"][1]["complete"] = json!(false);
        let error = preflight_legacy_scopes(&db, &incomplete.to_string())
            .await
            .expect_err("an incomplete owner receipt must block");
        assert!(matches!(
            error,
            ScopeGuardError::IncompleteOwnerReceipt { .. }
        ));
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_db_scope_preflight_blocks_untrusted_prefixes_and_shared_scope() {
        let (_temp_dir, db) = seeded_db().await;
        let mut untrusted = manifest();
        untrusted["orphan_prefixes"][0]["evidence_digest"] = json!("");
        let error = preflight_legacy_scopes(&db, &untrusted.to_string())
            .await
            .expect_err("an untrusted orphan prefix must block");
        assert!(matches!(
            error,
            ScopeGuardError::UntrustedOrphanPrefix { .. }
        ));

        let mut shared = manifest();
        shared["bucket_scope"] = json!("/");
        assert_eq!(
            preflight_legacy_scopes(&db, &shared.to_string())
                .await
                .expect_err("shared bucket"),
            ScopeGuardError::SharedBucketScope
        );

        let mut traces = manifest();
        traces["delete_git_traces"] = json!(true);
        assert_eq!(
            preflight_legacy_scopes(&db, &traces.to_string())
                .await
                .expect_err("git traces"),
            ScopeGuardError::LegacyGitTraces
        );
        db.close().await.ok();
    }

    #[test]
    fn legacy_db_scope_preflight_manifest_round_trips() {
        let manifest: ScopeAuthorizationManifest =
            serde_json::from_value(manifest()).expect("manifest");
        assert_eq!(manifest.owners.len(), 2);
        assert_eq!(manifest.orphan_prefixes.len(), 1);
        assert!(!manifest.delete_git_traces);
    }
}
