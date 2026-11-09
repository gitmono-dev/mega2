//! Trusted provenance for a legacy-Capture maintenance scope (MTR-56).
//!
//! A destructive purge may only start from a controlled maintenance manifest
//! whose sources have been verified against the running configuration, the
//! legacy database metadata, the historical deployment locations and the
//! operator declaration. Any unknown historical location, conflicting source
//! or duplicate scope blocks the freeze. The frozen manifest digest is stored
//! in the durable purge journal before deletion can run.

use sea_orm::{ActiveModelTrait, ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{callisto::trace_legacy_purge, config::ObjectStorageConfig};

/// The durable authorization produced by a verified manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenLegacyManifest {
    pub journal_id: String,
    pub digest: String,
    pub deployment_id: String,
}

/// The backend the running process actually uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentBackend {
    pub kind: String,
    pub location: String,
}

/// A controlled maintenance manifest supplied by the operator.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LegacyMaintenanceManifest {
    pub deployment_id: String,
    pub backend_kind: String,
    pub storage_location: String,
    #[serde(default)]
    pub historical_locations: Vec<String>,
    #[serde(default)]
    pub scopes: Vec<LegacyScopeClaim>,
    pub operator: LegacyOperatorDeclaration,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LegacyScopeClaim {
    pub deployment_id: String,
    pub tenant_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LegacyOperatorDeclaration {
    pub identity: String,
    pub statement_digest: String,
}

#[derive(Debug, Error)]
pub enum LegacyManifestError {
    #[error("the maintenance manifest is not valid JSON: {0}")]
    InvalidManifest(String),
    #[error(
        "the running object-storage backend does not match the manifest: config {config}, manifest {manifest}"
    )]
    BackendMismatch { config: String, manifest: String },
    #[error("the operator declaration is incomplete")]
    OperatorMissing,
    #[error("a historical deployment location is unknown or empty")]
    HistoricalLocationUnknown,
    #[error("the manifest repeats one maintenance scope")]
    DuplicateScope,
    #[error("the legacy database contains a scope outside the authorized manifest")]
    UnauthorizedDatabaseScope,
    #[error("a source cannot be read: {0}")]
    SourceUnreadable(String),
    #[error("the purge journal cannot be written: {0}")]
    Journal(String),
}

impl From<sea_orm::DbErr> for LegacyManifestError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::SourceUnreadable(error.to_string())
    }
}

/// Map the running configuration to the backend identity recorded in a
/// manifest. Secrets never enter this value.
pub fn current_backend_from_config(config: &ObjectStorageConfig) -> CurrentBackend {
    let kind = match config.storage_type {
        crate::orbit_api::factory::ObjectStorageBackend::Local => "local",
        crate::orbit_api::factory::ObjectStorageBackend::S3 => "s3",
        crate::orbit_api::factory::ObjectStorageBackend::S3Compatible => "s3compatible",
        crate::orbit_api::factory::ObjectStorageBackend::Gcs => "gcs",
    };
    let location = match config.storage_type {
        crate::orbit_api::factory::ObjectStorageBackend::Local => config.local.root_dir.clone(),
        crate::orbit_api::factory::ObjectStorageBackend::S3
        | crate::orbit_api::factory::ObjectStorageBackend::S3Compatible => {
            if config.s3.region.is_empty() {
                config.s3.bucket.clone()
            } else {
                format!("{}/{}", config.s3.region, config.s3.bucket)
            }
        }
        crate::orbit_api::factory::ObjectStorageBackend::Gcs => config.gcs.bucket.clone(),
    };
    CurrentBackend {
        kind: kind.to_owned(),
        location,
    }
}

fn manifest_digest(manifest: &LegacyMaintenanceManifest) -> Result<String, LegacyManifestError> {
    // A stable canonical digest: the serde struct order is fixed and scopes
    // are sorted, so two byte-equal declarations hash the same.
    let mut canonical = manifest.clone();
    canonical.scopes.sort_by(|left, right| {
        (left.deployment_id.as_str(), left.tenant_id.as_str())
            .cmp(&(right.deployment_id.as_str(), right.tenant_id.as_str()))
    });
    canonical.historical_locations.sort_unstable();
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| LegacyManifestError::InvalidManifest(error.to_string()))?;
    let digest = Sha256::digest(bytes);
    Ok(format!(
        "sha256:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

/// Verify every provenance source and freeze the manifest in the purge
/// journal. Nothing is deleted here: the returned frozen manifest is the
/// durable authorization the later maintenance steps consume.
pub async fn verify_and_freeze(
    db: &DatabaseConnection,
    current: &CurrentBackend,
    raw_manifest: &str,
) -> Result<FrozenLegacyManifest, LegacyManifestError> {
    let manifest: LegacyMaintenanceManifest = serde_json::from_str(raw_manifest)
        .map_err(|error| LegacyManifestError::InvalidManifest(error.to_string()))?;
    // AC-1: the running configuration is one of the verified sources.
    if manifest.backend_kind != current.kind || manifest.storage_location != current.location {
        return Err(LegacyManifestError::BackendMismatch {
            config: format!("{}:{}", current.kind, current.location),
            manifest: format!("{}:{}", manifest.backend_kind, manifest.storage_location),
        });
    }
    // AC-4: an explicit operator identity and statement digest.
    if manifest.operator.identity.trim().is_empty()
        || manifest.operator.statement_digest.trim().is_empty()
    {
        return Err(LegacyManifestError::OperatorMissing);
    }
    // AC-3/AC-5: every historical location must be declared and known.
    if manifest.historical_locations.is_empty()
        || manifest
            .historical_locations
            .iter()
            .any(|location| location.trim().is_empty() || location == "unknown")
    {
        return Err(LegacyManifestError::HistoricalLocationUnknown);
    }
    // AC-6: duplicates are a conflicting source, not a union.
    let mut claimed: Vec<(&str, &str)> = manifest
        .scopes
        .iter()
        .map(|scope| (scope.deployment_id.as_str(), scope.tenant_id.as_str()))
        .collect();
    let before = claimed.len();
    claimed.sort_unstable();
    claimed.dedup();
    if claimed.len() != before {
        return Err(LegacyManifestError::DuplicateScope);
    }
    // AC-2/AC-6: legacy DB metadata is another verified source. Every scope
    // that still has legacy rows must be inside the authorized manifest.
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT DISTINCT deployment_id, tenant_id FROM agent_capture_session",
        ))
        .await?;
    for row in rows {
        let deployment: String = row
            .try_get("", "deployment_id")
            .map_err(|error| LegacyManifestError::SourceUnreadable(error.to_string()))?;
        let tenant: String = row
            .try_get("", "tenant_id")
            .map_err(|error| LegacyManifestError::SourceUnreadable(error.to_string()))?;
        if !claimed.iter().any(|(claimed_deployment, claimed_tenant)| {
            *claimed_deployment == deployment && *claimed_tenant == tenant
        }) {
            return Err(LegacyManifestError::UnauthorizedDatabaseScope);
        }
    }
    // AC-7: persist the frozen digest before any destructive step.
    let digest = manifest_digest(&manifest)?;
    let id = uuid::Uuid::new_v4().to_string();
    trace_legacy_purge::ActiveModel {
        id: sea_orm::Set(id.clone()),
        backend_fingerprint: sea_orm::Set(format!(
            "{}:{}",
            manifest.backend_kind, manifest.storage_location
        )),
        storage_location: sea_orm::Set(manifest.historical_locations.join(",")),
        deployment_id: sea_orm::Set(manifest.deployment_id.clone()),
        manifest_digest: sea_orm::Set(digest.clone()),
        state: sea_orm::Set("frozen".to_owned()),
        cursor: sea_orm::Set(None),
        retry_count: sea_orm::Set(0),
        created_at: sea_orm::Set(chrono::Utc::now().fixed_offset()),
    }
    .insert(db)
    .await
    .map_err(|error| LegacyManifestError::Journal(error.to_string()))?;
    Ok(FrozenLegacyManifest {
        journal_id: id,
        digest,
        deployment_id: manifest.deployment_id,
    })
}

#[cfg(test)]
mod tests {
    use sea_orm::ConnectionTrait;
    use serde_json::json;

    use super::{
        CurrentBackend, LegacyManifestError, current_backend_from_config, verify_and_freeze,
    };
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    fn current() -> CurrentBackend {
        CurrentBackend {
            kind: "local".to_owned(),
            location: "/srv/mega2/objects".to_owned(),
        }
    }

    fn manifest(extra: serde_json::Value) -> String {
        let mut value = json!({
            "deployment_id": "deploy-fixture",
            "backend_kind": "local",
            "storage_location": "/srv/mega2/objects",
            "historical_locations": ["local:/srv/mega2/objects", "s3:bucket-legacy/agent"],
            "scopes": [{"deployment_id": "deploy-fixture", "tenant_id": "tenant-a"}],
            "operator": {"identity": "operator@example.invalid", "statement_digest": "sha256:statement"}
        });
        if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            for (key, entry) in extra {
                value.insert(key.clone(), entry.clone());
            }
        }
        value.to_string()
    }

    #[tokio::test]
    async fn legacy_inventory_provenance_freezes_digest() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let frozen = verify_and_freeze(&db, &current(), &manifest(json!({})))
            .await
            .expect("freeze");
        assert!(frozen.digest.starts_with("sha256:"), "{}", frozen.digest);
        let rows = db
            .query_all_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Postgres,
                "SELECT manifest_digest, storage_location FROM trace_legacy_purge",
            ))
            .await
            .expect("journal rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]
                .try_get::<String>("", "manifest_digest")
                .expect("digest"),
            frozen.digest
        );
        // The historical locations are preserved for later provider
        // inventory steps.
        let locations = rows[0]
            .try_get::<String>("", "storage_location")
            .expect("locations");
        assert!(locations.contains("s3:bucket-legacy/agent"), "{locations}");
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_inventory_provenance_blocks_unknown_location() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let error = verify_and_freeze(
            &db,
            &current(),
            &manifest(json!({"historical_locations": ["local:/srv/mega2/objects", "unknown"]})),
        )
        .await
        .expect_err("unknown historical location must block");
        assert!(matches!(
            error,
            LegacyManifestError::HistoricalLocationUnknown
        ));
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_inventory_provenance_blocks_conflicting_sources() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        db.execute_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Postgres,
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind, completeness)
             VALUES ('deploy-fixture', 'tenant-other', 'r', 'p', 'unclaimed', 'external_capture', 'complete')",
        ))
        .await
        .expect("legacy row");
        let error = verify_and_freeze(&db, &current(), &manifest(json!({})))
            .await
            .expect_err("an unclaimed DB scope must block");
        assert!(matches!(
            error,
            LegacyManifestError::UnauthorizedDatabaseScope
        ));
        db.close().await.ok();
    }

    #[test]
    fn legacy_inventory_provenance_maps_config_backend() {
        let config: crate::config::ObjectStorageConfig = toml::from_str(
            "storage_type = \"local\"\n[local]\nroot_dir = \"/srv/mega2/objects\"\n",
        )
        .expect("config");
        assert_eq!(current_backend_from_config(&config), current());
    }
}
