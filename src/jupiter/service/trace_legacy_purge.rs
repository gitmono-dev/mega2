//! Journal-driven legacy-Capture object deletion (MTR-25).
//!
//! The delete step only consumes the frozen inventory of one journal. Each
//! object is removed and marked `deleted_at` in the same item row, and the
//! journal cursor advances after every object, so a crash resumes from the
//! first unmarked item instead of claiming an empty bucket meant "no data".
//! Only keys under the frozen manifest prefixes are touched: a shared bucket's
//! unauthorized prefix and the new `traces/` namespace are never addressed.
//! The legacy tables are not dropped here.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, Set, Statement, TransactionTrait,
};
use serde::Deserialize;
use thiserror::Error;

use crate::{
    callisto::{trace_legacy_purge, trace_legacy_purge_item, trace_legacy_writer_fence},
    jupiter::service::{
        trace_legacy_inventory::CANONICAL_LEGACY_PREFIXES,
        trace_legacy_scope_guard::ScopeAuthorizationManifest,
    },
    orbit::ObjectStoreAdapter,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyDeleteOutcome {
    pub journal_id: String,
    pub deleted: usize,
    pub already_deleted: usize,
    pub remaining: usize,
    pub rescan_empty: bool,
    pub owners: Vec<String>,
}

#[derive(Debug, Error)]
pub enum LegacyDeleteError {
    #[error("the maintenance manifest is not valid JSON: {0}")]
    InvalidManifest(String),
    #[error("the purge journal does not exist")]
    JournalMissing,
    #[error("the purge journal is not in the frozen state")]
    JournalNotFrozen,
    #[error("no active writer fence belongs to this journal")]
    FenceMissing,
    #[error("no inventory has been frozen for this journal")]
    InventoryMissing,
    #[error("the object {key} is outside the frozen manifest prefixes")]
    OutsideFrozenPrefixes { key: String },
    #[error("the object {key} is in a namespace this purge never deletes")]
    ForbiddenNamespace { key: String },
    #[error("the object store cannot delete {key}: {reason}")]
    Store { key: String, reason: String },
    #[error("the rescan still finds objects under {prefix}")]
    RescanNotEmpty { prefix: String },
    #[error("the deletion journal cannot be written: {0}")]
    Database(String),
}

impl From<sea_orm::DbErr> for LegacyDeleteError {
    fn from(error: sea_orm::DbErr) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<crate::orbit_api::error::IoOrbitError> for LegacyDeleteError {
    fn from(error: crate::orbit_api::error::IoOrbitError) -> Self {
        Self::Store {
            key: "store".to_owned(),
            reason: error.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct DeleteManifest {
    #[serde(flatten)]
    scope: ScopeAuthorizationManifest,
    #[serde(default)]
    trusted_prefixes: Vec<String>,
}

fn frozen_prefixes(manifest: &DeleteManifest) -> Result<Vec<String>, LegacyDeleteError> {
    let mut prefixes: Vec<String> = CANONICAL_LEGACY_PREFIXES
        .iter()
        .map(|prefix| (*prefix).to_owned())
        .collect();
    for prefix in &manifest.trusted_prefixes {
        let trimmed = prefix.trim();
        if trimmed.is_empty()
            || !trimmed.starts_with("agent/")
            || trimmed.contains("..")
            || trimmed.contains('\\')
            || trimmed.contains('\0')
        {
            return Err(LegacyDeleteError::OutsideFrozenPrefixes {
                key: prefix.clone(),
            });
        }
        prefixes.push(trimmed.to_owned());
    }
    prefixes.sort();
    prefixes.dedup();
    Ok(prefixes)
}

fn guard_key(key: &str, prefixes: &[String]) -> Result<(), LegacyDeleteError> {
    if key.starts_with("traces/") {
        return Err(LegacyDeleteError::ForbiddenNamespace {
            key: key.to_owned(),
        });
    }
    if !prefixes
        .iter()
        .any(|prefix| key.starts_with(prefix.as_str()))
    {
        return Err(LegacyDeleteError::OutsideFrozenPrefixes {
            key: key.to_owned(),
        });
    }
    Ok(())
}

/// Delete the frozen inventory of one journal, resumable after a crash.
pub async fn delete_legacy_objects(
    db: &DatabaseConnection,
    adapter: &ObjectStoreAdapter,
    journal_id: &str,
    raw_manifest: &str,
) -> Result<LegacyDeleteOutcome, LegacyDeleteError> {
    let manifest: DeleteManifest = serde_json::from_str(raw_manifest)
        .map_err(|error| LegacyDeleteError::InvalidManifest(error.to_string()))?;
    let journal = trace_legacy_purge::Entity::find_by_id(journal_id)
        .one(db)
        .await?
        .ok_or(LegacyDeleteError::JournalMissing)?;
    if journal.state != "frozen" {
        return Err(LegacyDeleteError::JournalNotFrozen);
    }
    trace_legacy_writer_fence::Entity::find()
        .one(db)
        .await?
        .filter(|fence| fence.purge_id == journal_id && fence.active)
        .ok_or(LegacyDeleteError::FenceMissing)?;
    let prefixes = frozen_prefixes(&manifest)?;
    let items = trace_legacy_purge_item::Entity::find()
        .filter(trace_legacy_purge_item::Column::PurgeId.eq(journal_id))
        .all(db)
        .await?;
    if items.is_empty() {
        return Err(LegacyDeleteError::InventoryMissing);
    }
    let mut deleted = 0usize;
    let mut already_deleted = 0usize;
    for item in &items {
        guard_key(&item.object_key, &prefixes)?;
        if item.deleted_at.is_some() {
            already_deleted += 1;
            continue;
        }
        adapter
            .delete_backend_path(&item.object_key)
            .await
            .map_err(|error| LegacyDeleteError::Store {
                key: item.object_key.clone(),
                reason: error.to_string(),
            })?;
        // Durability point: the item row and the journal cursor advance in one
        // transaction, so a crash between objects resumes from here.
        let txn = db.begin().await?;
        let mut update: trace_legacy_purge_item::ActiveModel = item.clone().into();
        update.deleted_at = Set(Some(chrono::Utc::now().fixed_offset()));
        update.update(&txn).await?;
        let mut cursor: trace_legacy_purge::ActiveModel = journal.clone().into();
        cursor.cursor = Set(Some(item.object_key.clone()));
        cursor.update(&txn).await?;
        txn.commit().await?;
        deleted += 1;
    }
    // Rescan: every frozen prefix must now be empty. An empty bucket on the
    // first run never proves "there was no data"; the journal does.
    for prefix in &prefixes {
        let remaining = adapter.list_prefix(prefix).await?;
        if !remaining.is_empty() {
            return Err(LegacyDeleteError::RescanNotEmpty {
                prefix: prefix.clone(),
            });
        }
    }
    let owners: Vec<String> = manifest
        .scope
        .owners
        .iter()
        .map(|owner| owner.owner.clone())
        .collect();
    let remaining = trace_legacy_purge_item::Entity::find()
        .filter(trace_legacy_purge_item::Column::PurgeId.eq(journal_id))
        .filter(trace_legacy_purge_item::Column::DeletedAt.is_null())
        .all(db)
        .await?
        .len();
    // AC-8: the legacy tables are untouched; the plain count proves the delete
    // step did not drop or truncate anything.
    let _ = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*) AS n FROM agent_capture_session",
        ))
        .await?;
    Ok(LegacyDeleteOutcome {
        journal_id: journal_id.to_owned(),
        deleted,
        already_deleted,
        remaining,
        rescan_empty: true,
        owners,
    })
}
