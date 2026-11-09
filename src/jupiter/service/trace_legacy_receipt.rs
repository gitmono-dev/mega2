//! Verified legacy-Capture purge completion receipt (MTR-44).
//!
//! The guarded DROP consumes a [`VerifiedLegacyPurgeReceipt`] that only this
//! module can construct. The receipt binds the current fence, the frozen
//! inventory, every declared backend fingerprint, the whole-database scope set
//! and the post-deletion rescan. Any unverified historical location, a
//! historical version residue, an undeleted inventory item or an unclaimed
//! empty install refuses verification. The module uses explicit SQL and the
//! maintenance manifest only; it never references the old Capture entities.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, Statement};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    callisto::trace_legacy_purge,
    jupiter::service::trace_legacy_scope_guard::{
        AggregateScopeReceipt, ScopeAuthorizationManifest, preflight_legacy_scopes,
    },
    orbit::ObjectStoreAdapter,
};

/// A completion receipt the guarded DROP may consume. No other module can
/// construct it: the fields are private and only [`verify_purge_complete`]
/// creates the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedLegacyPurgeReceipt {
    digest: String,
    journal_id: String,
    fence_id: String,
    aggregate_scope_digest: String,
    inventory_digest: String,
    backend_fingerprint: String,
    scope_count: usize,
    empty_install: bool,
}

impl VerifiedLegacyPurgeReceipt {
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn journal_id(&self) -> &str {
        &self.journal_id
    }
    pub fn fence_id(&self) -> &str {
        &self.fence_id
    }
    pub fn aggregate_scope_digest(&self) -> &str {
        &self.aggregate_scope_digest
    }
    pub fn inventory_digest(&self) -> &str {
        &self.inventory_digest
    }
    pub fn backend_fingerprint(&self) -> &str {
        &self.backend_fingerprint
    }
    pub fn scope_count(&self) -> usize {
        self.scope_count
    }
    pub fn empty_install(&self) -> bool {
        self.empty_install
    }
}

#[derive(Debug, Error)]
pub enum PurgeReceiptError {
    #[error("the maintenance manifest is not valid JSON: {0}")]
    InvalidManifest(String),
    #[error("the purge journal does not exist")]
    JournalMissing,
    #[error("the purge journal is not in the frozen state")]
    JournalNotFrozen,
    #[error("the journal's active writer fence is missing")]
    FenceMissing,
    #[error("the journal backend {journal} does not match the manifest {manifest}")]
    BackendMismatch { journal: String, manifest: String },
    #[error("a historical deployment location is unknown or empty")]
    HistoricalLocationUnknown,
    #[error("the frozen inventory still has {remaining} undeleted object(s)")]
    InventoryIncomplete { remaining: usize },
    #[error("the post-deletion rescan still finds objects under {prefix}")]
    RescanNotEmpty { prefix: String },
    #[error("the backend {backend} cannot prove its historical version inventory")]
    VersionCapabilityUnproven { backend: String },
    #[error("an empty installation needs an explicit empty-inventory claim")]
    EmptyInstallUnclaimed,
    #[error("the scope preflight failed: {0}")]
    Preflight(String),
    #[error("the object store cannot be read: {0}")]
    Store(String),
    #[error("the receipt cannot be verified: {0}")]
    Database(String),
}

impl From<sea_orm::DbErr> for PurgeReceiptError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<crate::orbit_api::error::IoOrbitError> for PurgeReceiptError {
    fn from(error: crate::orbit_api::error::IoOrbitError) -> Self {
        Self::Store(error.to_string())
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ReceiptManifest {
    #[serde(flatten)]
    scope: ScopeAuthorizationManifest,
    backend_kind: String,
    storage_location: String,
    #[serde(default)]
    trusted_prefixes: Vec<String>,
    /// Historical deployment locations; every one must be declared and known.
    #[serde(default)]
    historical_locations: Vec<String>,
    /// Set by the operator only after the empty installation was checked.
    #[serde(default)]
    empty_inventory_verified: bool,
}

fn sha256_hex(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Verify that one journal is ready for the guarded DROP.
pub async fn verify_purge_complete(
    db: &DatabaseConnection,
    adapter: &ObjectStoreAdapter,
    journal_id: &str,
    raw_manifest: &str,
) -> Result<VerifiedLegacyPurgeReceipt, PurgeReceiptError> {
    let manifest: ReceiptManifest = serde_json::from_str(raw_manifest)
        .map_err(|error| PurgeReceiptError::InvalidManifest(error.to_string()))?;
    // AC-5: every historical location must be declared and known.
    if manifest.historical_locations.is_empty()
        || manifest
            .historical_locations
            .iter()
            .any(|location| location.trim().is_empty() || location.trim() == "unknown")
    {
        return Err(PurgeReceiptError::HistoricalLocationUnknown);
    }
    let journal = trace_legacy_purge::Entity::find_by_id(journal_id)
        .one(db)
        .await?
        .ok_or(PurgeReceiptError::JournalMissing)?;
    if journal.state != "frozen" {
        return Err(PurgeReceiptError::JournalNotFrozen);
    }
    // AC-1: the receipt matches the current active fence.
    let fence_id: String = db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT id FROM trace_legacy_writer_fence
             WHERE purge_id = $1 AND active ORDER BY activated_at DESC LIMIT 1",
            [journal_id.into()],
        ))
        .await?
        .map(|row| row.try_get::<String>("", "id").unwrap_or_default())
        .filter(|id| !id.is_empty())
        .ok_or(PurgeReceiptError::FenceMissing)?;
    // AC-3: the journal backend fingerprint matches the frozen manifest.
    let manifest_fingerprint = format!("{}:{}", manifest.backend_kind, manifest.storage_location);
    if journal.backend_fingerprint != manifest_fingerprint {
        return Err(PurgeReceiptError::BackendMismatch {
            journal: journal.backend_fingerprint.clone(),
            manifest: manifest_fingerprint,
        });
    }
    // AC-4: the receipt covers the current whole-database scope set.
    let aggregate: AggregateScopeReceipt = preflight_legacy_scopes(db, raw_manifest)
        .await
        .map_err(|error| PurgeReceiptError::Preflight(error.to_string()))?;
    // AC-2: the frozen inventory is fully deleted and its digest is stable.
    let items = db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT object_key, deleted_at FROM trace_legacy_purge_item
             WHERE purge_id = $1 ORDER BY object_key",
            [journal_id.into()],
        ))
        .await?;
    let mut undeleted = 0usize;
    let mut keys: Vec<String> = Vec::with_capacity(items.len());
    for row in &items {
        let key: String = row.try_get("", "object_key")?;
        let deleted: Option<chrono::DateTime<chrono::FixedOffset>> =
            row.try_get("", "deleted_at").ok();
        if deleted.is_none() {
            undeleted += 1;
        }
        keys.push(key);
    }
    if undeleted > 0 {
        return Err(PurgeReceiptError::InventoryIncomplete {
            remaining: undeleted,
        });
    }
    // AC-6: a cloud backend's historical versions cannot be proven by this
    // maintenance connection, so it must never become Verified. Local has no
    // versions and the listing below is the whole truth there.
    let backend = journal
        .backend_fingerprint
        .split_once(':')
        .map(|(kind, _)| kind)
        .unwrap_or(journal.backend_fingerprint.as_str());
    if backend != "local" {
        return Err(PurgeReceiptError::VersionCapabilityUnproven {
            backend: backend.to_owned(),
        });
    }
    // The rescan must not find a residue under a frozen prefix.
    let mut prefixes: Vec<String> = vec![
        "agent/raw/".to_owned(),
        "agent/redacted/".to_owned(),
        "agent/staging/".to_owned(),
    ];
    for prefix in &manifest.trusted_prefixes {
        prefixes.push(prefix.trim().to_owned());
    }
    prefixes.sort();
    prefixes.dedup();
    for prefix in &prefixes {
        if !adapter.list_prefix(prefix).await?.is_empty() {
            return Err(PurgeReceiptError::RescanNotEmpty {
                prefix: prefix.clone(),
            });
        }
    }
    // AC-7: an empty installation is only accepted with an explicit claim.
    let empty_install = aggregate.scope_count == 0 && items.is_empty();
    if empty_install && !manifest.empty_inventory_verified {
        return Err(PurgeReceiptError::EmptyInstallUnclaimed);
    }
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let inventory_digest = sha256_hex(&key_refs);
    let digest = sha256_hex(&[
        journal_id,
        &fence_id,
        &aggregate.digest,
        &inventory_digest,
        &journal.backend_fingerprint,
    ]);
    Ok(VerifiedLegacyPurgeReceipt {
        digest,
        journal_id: journal_id.to_owned(),
        fence_id,
        aggregate_scope_digest: aggregate.digest,
        inventory_digest,
        backend_fingerprint: journal.backend_fingerprint,
        scope_count: aggregate.scope_count,
        empty_install,
    })
}

#[cfg(test)]
mod tests {
    use sea_orm::{ConnectionTrait, Statement};

    use super::{PurgeReceiptError, verify_purge_complete};
    use crate::{
        jupiter::{migration::apply_migrations, tests::test_db_connection},
        orbit::factory::ObjectStorageFactory,
        orbit_api::factory::ObjectStorageConfig,
    };

    const JOURNAL: &str = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    const FENCE: &str = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

    async fn fixture() -> (
        tempfile::TempDir,
        sea_orm::DatabaseConnection,
        crate::orbit::ObjectStoreAdapter,
        String,
    ) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let root = temp_dir.path().join("objects");
        std::fs::create_dir_all(&root).expect("root");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let cfg: ObjectStorageConfig = toml::from_str(&format!(
            "storage_type = \"local\"\n[local]\nroot_dir = \"{}\"\n",
            root.display()
        ))
        .expect("storage config");
        let adapter = ObjectStorageFactory::build_adapter(&cfg)
            .await
            .expect("adapter");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge
             (id, backend_fingerprint, storage_location, deployment_id, manifest_digest, state)
             VALUES ('{JOURNAL}', 'local:{}', '{}', 'deploy-r', 'sha256:manifest', 'frozen')",
            root.display(),
            root.display()
        ))
        .await
        .expect("journal");
        db.execute_unprepared(
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind, completeness)
             VALUES ('deploy-r', 'tenant-r', 'repo', 'producer', 'receipt-session', 'external_capture', 'complete')",
        )
        .await
        .expect("legacy session");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_writer_fence (id, purge_id, deployment_id, active)
             VALUES ('{FENCE}', '{JOURNAL}', 'deploy-r', TRUE)"
        ))
        .await
        .expect("fence");
        let manifest = format!(
            r#"{{
                "bucket_scope": "mega2-legacy/",
                "deployment_id": "deploy-r",
                "backend_kind": "local",
                "storage_location": "{}",
                "historical_locations": ["local:{}"],
                "owners": [{{"owner": "owner-a", "deployment_id": "deploy-r", "tenant_id": "tenant-r", "receipt_id": "receipt-a", "complete": true}}],
                "orphan_prefixes": []
            }}"#,
            root.display(),
            root.display()
        );
        (temp_dir, db, adapter, manifest)
    }

    fn root_for_manifest(manifest: &str) -> String {
        serde_json::from_str::<serde_json::Value>(manifest)
            .expect("manifest json")
            .get("storage_location")
            .and_then(|value| value.as_str())
            .expect("storage location")
            .to_owned()
    }

    async fn add_deleted_item(db: &sea_orm::DatabaseConnection) {
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge_item
             (id, purge_id, scope, object_key, deleted_at)
             VALUES ('eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee', '{JOURNAL}', 'deploy-r', 'agent/raw/aa', now())"
        ))
        .await
        .expect("item");
    }

    #[tokio::test]
    async fn purge_receipt_verifies_a_complete_journal() {
        let (_temp, db, adapter, manifest) = fixture().await;
        add_deleted_item(&db).await;
        let receipt = verify_purge_complete(&db, &adapter, JOURNAL, &manifest)
            .await
            .expect("receipt");
        assert_eq!(receipt.digest().len(), 64);
        assert_eq!(receipt.journal_id(), JOURNAL);
        assert_eq!(receipt.fence_id(), FENCE);
        assert_eq!(receipt.scope_count(), 1);
        assert!(!receipt.empty_install());
        assert_eq!(
            receipt.backend_fingerprint().split(':').next(),
            Some("local")
        );
        // The digest binds the fence and the inventory.
        assert_eq!(receipt.inventory_digest().len(), 64);
        db.close().await.ok();
    }

    #[tokio::test]
    async fn purge_receipt_refuses_incomplete_or_mismatched_state() {
        let (_temp, db, adapter, manifest) = fixture().await;
        // AC-1: no active fence.
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_writer_fence SET active = FALSE, released_at = now() WHERE id = '{FENCE}'"
        ))
        .await
        .expect("release");
        assert!(matches!(
            verify_purge_complete(&db, &adapter, JOURNAL, &manifest)
                .await
                .expect_err("no fence"),
            PurgeReceiptError::FenceMissing
        ));
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_writer_fence SET active = TRUE, released_at = NULL WHERE id = '{FENCE}'"
        ))
        .await
        .expect("reactivate");

        // AC-2: an undeleted inventory object.
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge_item (id, purge_id, scope, object_key)
             VALUES ('ffffffff-ffff-4fff-8fff-ffffffffffff', '{JOURNAL}', 'deploy-r', 'agent/raw/bb')"
        ))
        .await
        .expect("item");
        assert!(matches!(
            verify_purge_complete(&db, &adapter, JOURNAL, &manifest)
                .await
                .expect_err("undeleted"),
            PurgeReceiptError::InventoryIncomplete { remaining: 1 }
        ));
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_purge_item SET deleted_at = now()
             WHERE purge_id = '{JOURNAL}' AND object_key = 'agent/raw/bb'"
        ))
        .await
        .expect("mark second item");

        // AC-3: the journal backend disagrees with the manifest.
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_purge SET backend_fingerprint = 's3:bucket' WHERE id = '{JOURNAL}'"
        ))
        .await
        .expect("backend");
        assert!(matches!(
            verify_purge_complete(&db, &adapter, JOURNAL, &manifest)
                .await
                .expect_err("backend"),
            PurgeReceiptError::BackendMismatch { .. }
        ));

        // Restore the journal backend from the manifest before the next check.
        let manifest_location: String = serde_json::from_str::<serde_json::Value>(&manifest)
            .expect("manifest json")
            .get("storage_location")
            .and_then(|value| value.as_str())
            .expect("storage location")
            .to_owned();
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_purge SET backend_fingerprint = 'local:{}', storage_location = '{}' WHERE id = '{JOURNAL}'",
            manifest_location, manifest_location
        ))
        .await
        .expect("backend restore");

        // AC-6: a cloud backend cannot be Verified by this connection.
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_purge SET backend_fingerprint = 's3:bucket' WHERE id = '{JOURNAL}'"
        ))
        .await
        .expect("cloud backend");
        let cloud_manifest = manifest
            .replace("\"backend_kind\": \"local\"", "\"backend_kind\": \"s3\"")
            .replace(
                &format!("\"storage_location\": \"{}\"", root_for_manifest(&manifest)),
                "\"storage_location\": \"bucket\"",
            );
        assert!(matches!(
            verify_purge_complete(&db, &adapter, JOURNAL, &cloud_manifest)
                .await
                .expect_err("cloud"),
            PurgeReceiptError::VersionCapabilityUnproven { .. }
        ));
        db.execute_unprepared(&format!(
            "UPDATE trace_legacy_purge SET backend_fingerprint = 'local:{}' WHERE id = '{JOURNAL}'",
            root_for_manifest(&manifest)
        ))
        .await
        .expect("backend restore after cloud");

        // AC-5: an unknown historical location.
        let unknown = manifest.replace(
            "\"historical_locations\": [",
            "\"historical_locations\": [\"unknown\", ",
        );
        assert!(matches!(
            verify_purge_complete(&db, &adapter, JOURNAL, &unknown)
                .await
                .expect_err("historical"),
            PurgeReceiptError::HistoricalLocationUnknown
        ));

        // AC-6: a rescan residue under a frozen prefix.
        let root = db
            .query_one_raw(Statement::from_string(
                sea_orm::DbBackend::Postgres,
                format!("SELECT storage_location FROM trace_legacy_purge WHERE id = '{JOURNAL}'"),
            ))
            .await
            .expect("root lookup")
            .expect("root row")
            .try_get::<String>("", "storage_location")
            .expect("root");
        std::fs::create_dir_all(std::path::Path::new(&root).join("agent/raw")).expect("dirs");
        std::fs::write(std::path::Path::new(&root).join("agent/raw/residue"), b"x")
            .expect("residue");
        assert!(matches!(
            verify_purge_complete(&db, &adapter, JOURNAL, &manifest)
                .await
                .expect_err("residue"),
            PurgeReceiptError::RescanNotEmpty { .. }
        ));
        std::fs::remove_file(std::path::Path::new(&root).join("agent/raw/residue"))
            .expect("cleanup");
        db.close().await.ok();
    }

    #[tokio::test]
    async fn purge_receipt_requires_an_empty_install_claim() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let root = temp_dir.path().join("objects");
        std::fs::create_dir_all(&root).expect("root");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let cfg: ObjectStorageConfig = toml::from_str(&format!(
            "storage_type = \"local\"\n[local]\nroot_dir = \"{}\"\n",
            root.display()
        ))
        .expect("storage config");
        let adapter = ObjectStorageFactory::build_adapter(&cfg)
            .await
            .expect("adapter");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge
             (id, backend_fingerprint, storage_location, deployment_id, manifest_digest, state)
             VALUES ('{JOURNAL}', 'local:{}', '{}', 'deploy-r', 'sha256:manifest', 'frozen')",
            root.display(),
            root.display()
        ))
        .await
        .expect("journal");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_writer_fence (id, purge_id, deployment_id, active)
             VALUES ('{FENCE}', '{JOURNAL}', 'deploy-r', TRUE)"
        ))
        .await
        .expect("fence");
        let base = format!(
            r#"{{"bucket_scope":"mega2-legacy/","deployment_id":"deploy-r","backend_kind":"local","storage_location":"{}","historical_locations":["local:{}"],"owners":[],"orphan_prefixes":[]}}"#,
            root.display(),
            root.display()
        );
        assert!(matches!(
            verify_purge_complete(&db, &adapter, JOURNAL, &base)
                .await
                .expect_err("unclaimed empty install"),
            PurgeReceiptError::EmptyInstallUnclaimed
        ));
        let claimed = base.replace(
            "\"orphan_prefixes\":[]",
            "\"orphan_prefixes\":[],\"empty_inventory_verified\":true",
        );
        let receipt = verify_purge_complete(&db, &adapter, JOURNAL, &claimed)
            .await
            .expect("claimed empty install");
        assert!(receipt.empty_install());
        assert_eq!(receipt.scope_count(), 0);
        db.close().await.ok();
    }
}
