//! Frozen legacy-Capture object inventory (MTR-24).
//!
//! Only the prefixes named by the frozen maintenance manifest are enumerated:
//! the three legacy visibilities (`agent/raw/`, `agent/redacted/`,
//! `agent/staging/`) plus explicitly trusted historical prefixes. Objects are
//! written to the durable purge journal, so a later delete step works from a
//! fixed list instead of a changing provider listing. A backend whose native
//! version/generation capability is required but unavailable blocks the
//! inventory; Local has no versions, so its items record `None`.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, Statement,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    callisto::{trace_legacy_purge, trace_legacy_purge_item, trace_legacy_writer_fence},
    jupiter::service::trace_legacy_scope_guard::ScopeAuthorizationManifest,
    orbit::ObjectStoreAdapter,
};

/// The three legacy object visibilities, as literal backend prefixes.
pub const CANONICAL_LEGACY_PREFIXES: [&str; 3] =
    ["agent/raw/", "agent/redacted/", "agent/staging/"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LegacyInventoryOutcome {
    pub journal_id: String,
    pub prefixes: Vec<String>,
    pub items: usize,
    /// `none` for backends without native versions (Local), `native` when the
    /// backend reported versions for every object.
    pub capability: &'static str,
}

#[derive(Debug, Error)]
pub enum LegacyInventoryError {
    #[error("the maintenance manifest is not valid JSON: {0}")]
    InvalidManifest(String),
    #[error("the purge journal does not exist")]
    JournalMissing,
    #[error("the purge journal is not in the frozen state")]
    JournalNotFrozen,
    #[error("no active writer fence belongs to this journal")]
    FenceMissing,
    #[error("the backend {backend} requires a native version inventory that is unavailable")]
    VersionCapabilityUnavailable { backend: String },
    #[error("the trusted prefix {prefix} is outside the legacy agent namespace")]
    UntrustedPrefix { prefix: String },
    #[error("the object store cannot be listed: {0}")]
    Store(String),
    #[error("the inventory journal cannot be written: {0}")]
    Database(String),
}

impl From<sea_orm::DbErr> for LegacyInventoryError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<crate::orbit_api::error::IoOrbitError> for LegacyInventoryError {
    fn from(error: crate::orbit_api::error::IoOrbitError) -> Self {
        Self::Store(error.to_string())
    }
}

/// The manifest may add trusted historical prefixes; the canonical three are
/// always enumerated.
#[derive(Clone, Debug, Deserialize)]
struct InventoryManifest {
    #[serde(flatten)]
    scope: ScopeAuthorizationManifest,
    #[serde(default)]
    trusted_prefixes: Vec<String>,
}

fn prefixes_for(manifest: &InventoryManifest) -> Result<Vec<String>, LegacyInventoryError> {
    let mut prefixes: Vec<String> = CANONICAL_LEGACY_PREFIXES
        .iter()
        .map(|prefix| (*prefix).to_owned())
        .collect();
    for prefix in &manifest.trusted_prefixes {
        let trimmed = prefix.trim();
        if !trimmed.starts_with("agent/")
            || trimmed.is_empty()
            || trimmed.contains("..")
            || trimmed.contains('\\')
            || trimmed.contains('\0')
        {
            return Err(LegacyInventoryError::UntrustedPrefix {
                prefix: prefix.clone(),
            });
        }
        prefixes.push(trimmed.to_owned());
    }
    prefixes.sort();
    prefixes.dedup();
    Ok(prefixes)
}

/// Enumerate the frozen prefixes and persist one journal item per object.
/// Nothing is deleted here.
pub async fn freeze_legacy_inventory(
    db: &DatabaseConnection,
    adapter: &ObjectStoreAdapter,
    journal_id: &str,
    raw_manifest: &str,
) -> Result<LegacyInventoryOutcome, LegacyInventoryError> {
    let manifest: InventoryManifest = serde_json::from_str(raw_manifest)
        .map_err(|error| LegacyInventoryError::InvalidManifest(error.to_string()))?;
    let journal = trace_legacy_purge::Entity::find_by_id(journal_id)
        .one(db)
        .await?
        .ok_or(LegacyInventoryError::JournalMissing)?;
    if journal.state != "frozen" {
        return Err(LegacyInventoryError::JournalNotFrozen);
    }
    // AC-8: the inventory belongs to the journal whose writer fence is active.
    let fence = trace_legacy_writer_fence::Entity::find()
        .one(db)
        .await?
        .filter(|fence| fence.purge_id == journal_id && fence.active)
        .ok_or(LegacyInventoryError::FenceMissing)?;
    let _ = fence;
    // AC-7: a cloud backend needs the provider-native version inventory; this
    // maintenance connection cannot prove it, so it must not pass.
    let backend = journal
        .backend_fingerprint
        .split_once(':')
        .map(|(kind, _)| kind)
        .unwrap_or(journal.backend_fingerprint.as_str());
    if backend != "local" {
        return Err(LegacyInventoryError::VersionCapabilityUnavailable {
            backend: backend.to_owned(),
        });
    }
    let prefixes = prefixes_for(&manifest)?;
    let scope = journal.deployment_id.clone();
    let mut items = 0usize;
    for prefix in &prefixes {
        for (path, _size) in adapter.list_prefix(prefix).await? {
            // Defensive: the backend path must still live under the frozen
            // prefix; the listing is authoritative but the check documents it.
            if !path.starts_with(prefix.as_str()) {
                continue;
            }
            let exists = trace_legacy_purge_item::Entity::find()
                .filter(trace_legacy_purge_item::Column::PurgeId.eq(journal_id))
                .filter(trace_legacy_purge_item::Column::Scope.eq(scope.clone()))
                .filter(trace_legacy_purge_item::Column::ObjectKey.eq(path.clone()))
                .one(db)
                .await?;
            if exists.is_some() {
                continue;
            }
            trace_legacy_purge_item::ActiveModel {
                id: sea_orm::Set(uuid::Uuid::new_v4().to_string()),
                purge_id: sea_orm::Set(journal_id.to_owned()),
                scope: sea_orm::Set(scope.clone()),
                object_key: sea_orm::Set(path),
                provider_version: sea_orm::Set(None),
                provider_generation: sea_orm::Set(None),
                deleted_at: sea_orm::Set(None),
            }
            .insert(db)
            .await?;
            items += 1;
        }
    }
    // AC-1: the maintenance connection never applies migrations; this query is
    // the only schema interaction and it is read-only.
    let _ = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT 1 AS present",
        ))
        .await?;
    Ok(LegacyInventoryOutcome {
        journal_id: journal_id.to_owned(),
        prefixes,
        items,
        capability: "none",
    })
}
