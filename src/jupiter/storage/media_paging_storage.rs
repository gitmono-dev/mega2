//! Durable Media paging state (plan-20260913 MF-08).
//!
//! Session seal serialization, chunk hash→length index, offset covering
//! queries, and finalize task leases with epoch fencing.

use std::ops::Deref;

use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, TransactionTrait,
};
use uuid::Uuid;

use crate::{
    callisto::{media_entry, media_session, media_task},
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

pub const STATE_PENDING: &str = "pending";
pub const STATE_SEALED: &str = "sealed";
pub const STATE_FINALIZED: &str = "finalized";
pub const TASK_PENDING: &str = "pending";
pub const TASK_RUNNING: &str = "running";
pub const TASK_COMPLETE: &str = "complete";
pub const TASK_FAILED: &str = "failed";

/// Default lease TTL (P-04b): 60 seconds.
pub const LEASE_TTL_SECS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkIndexRow {
    pub page_no: i32,
    pub ordinal: i32,
    pub offset: u64,
    pub length: u64,
    pub chunk_hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum MediaPagingError {
    #[error("media paging conflict: {0}")]
    Conflict(String),
    #[error("media paging not found")]
    NotFound,
    #[error("media paging stale lease")]
    StaleLease,
    #[error("media paging storage: {0}")]
    Storage(String),
}

impl From<DbErr> for MediaPagingError {
    fn from(value: DbErr) -> Self {
        MediaPagingError::Storage(value.to_string())
    }
}

impl From<MediaPagingError> for MegaError {
    fn from(value: MediaPagingError) -> Self {
        MegaError::Other(value.to_string())
    }
}

#[derive(Clone)]
pub struct MediaPagingStorage {
    pub base: BaseStorage,
}

impl Deref for MediaPagingStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl MediaPagingStorage {
    pub fn new(base: BaseStorage) -> Self {
        Self { base }
    }

    fn now() -> DateTime<FixedOffset> {
        Utc::now().fixed_offset()
    }

    /// Insert or return the existing pending session for `(scope, manifest_id)`.
    pub async fn upsert_pending_session(
        &self,
        scope_digest: &str,
        manifest_id: &str,
        algorithm: &str,
        oid: &str,
        size: u64,
        chunk_count: u64,
        page_count: i32,
        created_by: Option<&str>,
        expires_at: DateTime<FixedOffset>,
    ) -> Result<media_session::Model, MediaPagingError> {
        let conn = self.get_connection();
        if let Some(existing) = media_session::Entity::find()
            .filter(media_session::Column::ScopeDigest.eq(scope_digest))
            .filter(media_session::Column::ManifestId.eq(manifest_id))
            .one(conn)
            .await?
        {
            return Ok(existing);
        }
        let now = Self::now();
        let model = media_session::ActiveModel {
            id: ActiveValue::NotSet,
            scope_digest: Set(scope_digest.to_owned()),
            manifest_id: Set(manifest_id.to_owned()),
            algorithm: Set(algorithm.to_owned()),
            oid: Set(oid.to_owned()),
            size: Set(i64::try_from(size)
                .map_err(|_| MediaPagingError::Conflict("size exceeds signed i64".into()))?),
            chunk_count: Set(i64::try_from(chunk_count).map_err(|_| {
                MediaPagingError::Conflict("chunk_count exceeds signed i64".into())
            })?),
            page_count: Set(page_count),
            state: Set(STATE_PENDING.to_owned()),
            seal_generation: Set(0),
            created_by: Set(created_by.map(str::to_owned)),
            created_at: Set(now),
            updated_at: Set(now),
            expires_at: Set(expires_at),
        };
        Ok(model.insert(conn).await?)
    }

    /// Append a canonical page of entries. Same page content is idempotent;
    /// different content for the same page_no is Conflict. Must run under the
    /// session row lock held by [`Self::seal_session`].
    pub async fn put_page_entries(
        &self,
        scope_digest: &str,
        manifest_id: &str,
        page_no: i32,
        entries: &[ChunkIndexRow],
    ) -> Result<(), MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        Self::put_page_entries_in_txn(&txn, scope_digest, manifest_id, page_no, entries).await?;
        txn.commit().await?;
        Ok(())
    }

    async fn put_page_entries_in_txn<C: ConnectionTrait>(
        txn: &C,
        scope_digest: &str,
        manifest_id: &str,
        page_no: i32,
        entries: &[ChunkIndexRow],
    ) -> Result<(), MediaPagingError> {
        let existing = media_entry::Entity::find()
            .filter(media_entry::Column::ScopeDigest.eq(scope_digest))
            .filter(media_entry::Column::ManifestId.eq(manifest_id))
            .filter(media_entry::Column::PageNo.eq(page_no))
            .order_by_asc(media_entry::Column::Ordinal)
            .all(txn)
            .await?;
        if !existing.is_empty() {
            if existing.len() != entries.len() {
                return Err(MediaPagingError::Conflict(
                    "page already exists with different entry count".into(),
                ));
            }
            for (row, expected) in existing.iter().zip(entries.iter()) {
                if row.ordinal != expected.ordinal
                    || row.offset as u64 != expected.offset
                    || row.length as u64 != expected.length
                    || row.chunk_hash != expected.chunk_hash
                {
                    return Err(MediaPagingError::Conflict(
                        "page already exists with different content".into(),
                    ));
                }
            }
            return Ok(());
        }

        // Cross-page hash→length consistency.
        for e in entries {
            if let Some(prior) = media_entry::Entity::find()
                .filter(media_entry::Column::ScopeDigest.eq(scope_digest))
                .filter(media_entry::Column::ManifestId.eq(manifest_id))
                .filter(media_entry::Column::ChunkHash.eq(&e.chunk_hash))
                .one(txn)
                .await?
            {
                if prior.length as u64 != e.length {
                    return Err(MediaPagingError::Conflict(
                        "chunk hash length conflict across pages".into(),
                    ));
                }
            }
        }

        for e in entries {
            let am = media_entry::ActiveModel {
                id: ActiveValue::NotSet,
                scope_digest: Set(scope_digest.to_owned()),
                manifest_id: Set(manifest_id.to_owned()),
                page_no: Set(page_no),
                ordinal: Set(e.ordinal),
                offset: Set(i64::try_from(e.offset)
                    .map_err(|_| MediaPagingError::Conflict("offset exceeds signed i64".into()))?),
                length: Set(i64::try_from(e.length)
                    .map_err(|_| MediaPagingError::Conflict("length exceeds signed i64".into()))?),
                chunk_hash: Set(e.chunk_hash.clone()),
            };
            am.insert(txn).await?;
        }
        Ok(())
    }

    /// Seal a pending session after verifying contiguous pages `0..page_count`.
    /// Serializes concurrent seal attempts via row lock.
    pub async fn seal_session(
        &self,
        scope_digest: &str,
        manifest_id: &str,
    ) -> Result<media_session::Model, MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        let session = media_session::Entity::find()
            .filter(media_session::Column::ScopeDigest.eq(scope_digest))
            .filter(media_session::Column::ManifestId.eq(manifest_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(MediaPagingError::NotFound)?;

        if session.state == STATE_SEALED || session.state == STATE_FINALIZED {
            txn.commit().await?;
            return Ok(session);
        }
        if session.state != STATE_PENDING {
            return Err(MediaPagingError::Conflict(format!(
                "cannot seal session in state {}",
                session.state
            )));
        }

        // Contiguous pages 0..page_count-1 must exist; reject gaps / extras.
        let pages: Vec<i32> = media_entry::Entity::find()
            .filter(media_entry::Column::ScopeDigest.eq(scope_digest))
            .filter(media_entry::Column::ManifestId.eq(manifest_id))
            .select_only()
            .column(media_entry::Column::PageNo)
            .distinct()
            .into_tuple::<i32>()
            .all(&txn)
            .await?;
        let mut pages = pages;
        pages.sort_unstable();
        let expected: Vec<i32> = (0..session.page_count).collect();
        if pages != expected {
            return Err(MediaPagingError::Conflict(
                "non-canonical or incomplete page set".into(),
            ));
        }

        let now = Self::now();
        let new_generation = session.seal_generation.saturating_add(1);
        let mut am: media_session::ActiveModel = session.into();
        am.state = Set(STATE_SEALED.to_owned());
        am.seal_generation = Set(new_generation);
        am.updated_at = Set(now);
        let updated = am.update(&txn).await?;
        txn.commit().await?;
        Ok(updated)
    }

    /// Offset covering query with bounded page size (≤4096).
    pub async fn entries_covering_range(
        &self,
        scope_digest: &str,
        manifest_id: &str,
        offset: u64,
        length: u64,
        limit: u64,
    ) -> Result<Vec<media_entry::Model>, MediaPagingError> {
        let limit = limit.min(4096) as u64;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| MediaPagingError::Conflict("range overflow".into()))?;
        let start_i = i64::try_from(offset)
            .map_err(|_| MediaPagingError::Conflict("offset exceeds signed i64".into()))?;
        let end_i = i64::try_from(end)
            .map_err(|_| MediaPagingError::Conflict("range end exceeds signed i64".into()))?;

        // Chunk covers [offset, offset+length): overlap when chunk.offset < end
        // and chunk.offset+chunk.length > start.
        let rows = media_entry::Entity::find()
            .filter(media_entry::Column::ScopeDigest.eq(scope_digest))
            .filter(media_entry::Column::ManifestId.eq(manifest_id))
            .filter(media_entry::Column::Offset.lt(end_i))
            .order_by_asc(media_entry::Column::Offset)
            .limit(limit)
            .all(self.get_connection())
            .await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.offset.saturating_add(r.length) > start_i)
            .collect())
    }

    /// Drop and rebuild the derived index from caller-supplied sealed pages.
    pub async fn rebuild_index_from_pages(
        &self,
        scope_digest: &str,
        manifest_id: &str,
        pages: &[(i32, Vec<ChunkIndexRow>)],
    ) -> Result<(), MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        media_entry::Entity::delete_many()
            .filter(media_entry::Column::ScopeDigest.eq(scope_digest))
            .filter(media_entry::Column::ManifestId.eq(manifest_id))
            .exec(&txn)
            .await?;
        for (page_no, entries) in pages {
            Self::put_page_entries_in_txn(&txn, scope_digest, manifest_id, *page_no, entries)
                .await?;
        }
        txn.commit().await?;
        Ok(())
    }

    /// Create or reuse the finalize task for a sealed session.
    pub async fn ensure_task(
        &self,
        scope_digest: &str,
        manifest_id: &str,
    ) -> Result<media_task::Model, MediaPagingError> {
        let conn = self.get_connection();
        if let Some(existing) = media_task::Entity::find()
            .filter(media_task::Column::ScopeDigest.eq(scope_digest))
            .filter(media_task::Column::ManifestId.eq(manifest_id))
            .one(conn)
            .await?
        {
            return Ok(existing);
        }
        let now = Self::now();
        let model = media_task::ActiveModel {
            id: ActiveValue::NotSet,
            task_id: Set(Uuid::new_v4().to_string()),
            scope_digest: Set(scope_digest.to_owned()),
            manifest_id: Set(manifest_id.to_owned()),
            lease_owner: Set(None),
            lease_epoch: Set(0),
            expires_at: Set(None),
            state: Set(TASK_PENDING.to_owned()),
            bytes_verified: Set(0),
            pages_verified: Set(0),
            retryable: Set(true),
            error_code: Set(None),
            stage: Set("queued".into()),
            created_at: Set(now),
            updated_at: Set(now),
        };
        Ok(model.insert(conn).await?)
    }

    /// Claim or renew a lease with epoch fencing. Returns the new epoch.
    pub async fn claim_or_renew_lease(
        &self,
        task_id: &str,
        owner: &str,
        expected_epoch: Option<i64>,
    ) -> Result<media_task::Model, MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        let task = media_task::Entity::find()
            .filter(media_task::Column::TaskId.eq(task_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(MediaPagingError::NotFound)?;

        let now = Self::now();
        let expired = task.expires_at.map(|e| e <= now).unwrap_or(true);

        if let Some(epoch) = expected_epoch {
            if task.lease_epoch != epoch {
                return Err(MediaPagingError::StaleLease);
            }
        } else if task.lease_owner.is_some() && !expired && task.state == TASK_RUNNING {
            if task.lease_owner.as_deref() != Some(owner) {
                return Err(MediaPagingError::Conflict(
                    "lease held by another owner".into(),
                ));
            }
        }

        let new_epoch = if expected_epoch.is_some() {
            task.lease_epoch
        } else if expired || task.lease_owner.is_none() {
            task.lease_epoch.saturating_add(1)
        } else {
            task.lease_epoch
        };

        let expires = now + chrono::Duration::seconds(LEASE_TTL_SECS);
        let mut am: media_task::ActiveModel = task.into();
        am.lease_owner = Set(Some(owner.to_owned()));
        am.lease_epoch = Set(new_epoch);
        am.expires_at = Set(Some(expires));
        am.state = Set(TASK_RUNNING.to_owned());
        am.updated_at = Set(now);
        let updated = am.update(&txn).await?;
        txn.commit().await?;
        Ok(updated)
    }

    /// Progress update requires matching lease epoch.
    pub async fn update_task_progress(
        &self,
        task_id: &str,
        epoch: i64,
        bytes_verified: u64,
        pages_verified: i32,
        stage: &str,
    ) -> Result<media_task::Model, MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        let task = media_task::Entity::find()
            .filter(media_task::Column::TaskId.eq(task_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(MediaPagingError::NotFound)?;
        if task.lease_epoch != epoch {
            return Err(MediaPagingError::StaleLease);
        }
        let now = Self::now();
        let mut am: media_task::ActiveModel = task.into();
        am.bytes_verified = Set(i64::try_from(bytes_verified)
            .map_err(|_| MediaPagingError::Conflict("bytes_verified exceeds signed i64".into()))?);
        am.pages_verified = Set(pages_verified);
        am.stage = Set(stage.to_owned());
        am.updated_at = Set(now);
        am.expires_at = Set(Some(now + chrono::Duration::seconds(LEASE_TTL_SECS)));
        let updated = am.update(&txn).await?;
        txn.commit().await?;
        Ok(updated)
    }

    /// Mark complete only with the current epoch.
    pub async fn complete_task(
        &self,
        task_id: &str,
        epoch: i64,
    ) -> Result<media_task::Model, MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        let task = media_task::Entity::find()
            .filter(media_task::Column::TaskId.eq(task_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(MediaPagingError::NotFound)?;
        if task.lease_epoch != epoch {
            return Err(MediaPagingError::StaleLease);
        }
        let now = Self::now();
        let mut am: media_task::ActiveModel = task.into();
        am.state = Set(TASK_COMPLETE.to_owned());
        am.stage = Set("done".into());
        am.updated_at = Set(now);
        am.expires_at = Set(None);
        let updated = am.update(&txn).await?;

        // Mark session finalized in the same transaction.
        if let Some(session) = media_session::Entity::find()
            .filter(media_session::Column::ScopeDigest.eq(&updated.scope_digest))
            .filter(media_session::Column::ManifestId.eq(&updated.manifest_id))
            .lock_exclusive()
            .one(&txn)
            .await?
        {
            let mut sam: media_session::ActiveModel = session.into();
            sam.state = Set(STATE_FINALIZED.to_owned());
            sam.updated_at = Set(now);
            sam.update(&txn).await?;
        }

        txn.commit().await?;
        Ok(updated)
    }

    /// Fetch a session by natural key.
    pub async fn get_session(
        &self,
        scope_digest: &str,
        manifest_id: &str,
    ) -> Result<media_session::Model, MediaPagingError> {
        media_session::Entity::find()
            .filter(media_session::Column::ScopeDigest.eq(scope_digest))
            .filter(media_session::Column::ManifestId.eq(manifest_id))
            .one(self.get_connection())
            .await?
            .ok_or(MediaPagingError::NotFound)
    }

    /// Ordered page entries for a session (page_no, ordinal ascending).
    pub async fn list_entries(
        &self,
        scope_digest: &str,
        manifest_id: &str,
    ) -> Result<Vec<media_entry::Model>, MediaPagingError> {
        Ok(media_entry::Entity::find()
            .filter(media_entry::Column::ScopeDigest.eq(scope_digest))
            .filter(media_entry::Column::ManifestId.eq(manifest_id))
            .order_by_asc(media_entry::Column::PageNo)
            .order_by_asc(media_entry::Column::Ordinal)
            .all(self.get_connection())
            .await?)
    }

    /// Entries for a single page (ordinal ascending). Used by finalize page walks.
    pub async fn list_entries_for_page(
        &self,
        scope_digest: &str,
        manifest_id: &str,
        page_no: i32,
    ) -> Result<Vec<media_entry::Model>, MediaPagingError> {
        Ok(media_entry::Entity::find()
            .filter(media_entry::Column::ScopeDigest.eq(scope_digest))
            .filter(media_entry::Column::ManifestId.eq(manifest_id))
            .filter(media_entry::Column::PageNo.eq(page_no))
            .order_by_asc(media_entry::Column::Ordinal)
            .all(self.get_connection())
            .await?)
    }

    /// Fetch a finalize task by public `task_id`.
    pub async fn get_task(&self, task_id: &str) -> Result<media_task::Model, MediaPagingError> {
        media_task::Entity::find()
            .filter(media_task::Column::TaskId.eq(task_id))
            .one(self.get_connection())
            .await?
            .ok_or(MediaPagingError::NotFound)
    }

    /// Count tasks in `pending` or `running` (queue + in-flight, P-04b).
    pub async fn count_active_tasks(&self) -> Result<u64, MediaPagingError> {
        use sea_orm::PaginatorTrait;
        Ok(media_task::Entity::find()
            .filter(
                media_task::Column::State.is_in([TASK_PENDING.to_owned(), TASK_RUNNING.to_owned()]),
            )
            .count(self.get_connection())
            .await?)
    }

    /// Mark failed with matching epoch; clears lease so another worker may reclaim.
    pub async fn fail_task(
        &self,
        task_id: &str,
        epoch: i64,
        error_code: &str,
        retryable: bool,
    ) -> Result<media_task::Model, MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        let task = media_task::Entity::find()
            .filter(media_task::Column::TaskId.eq(task_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(MediaPagingError::NotFound)?;
        if task.lease_epoch != epoch {
            return Err(MediaPagingError::StaleLease);
        }
        let now = Self::now();
        let mut am: media_task::ActiveModel = task.into();
        am.state = Set(TASK_FAILED.to_owned());
        am.stage = Set("failed".into());
        am.error_code = Set(Some(error_code.to_owned()));
        am.retryable = Set(retryable);
        am.lease_owner = Set(None);
        am.expires_at = Set(None);
        am.updated_at = Set(now);
        let updated = am.update(&txn).await?;
        txn.commit().await?;
        Ok(updated)
    }

    /// Cancel a task (any epoch): failed + `cancelled`, lease released.
    pub async fn cancel_task(&self, task_id: &str) -> Result<media_task::Model, MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        let task = media_task::Entity::find()
            .filter(media_task::Column::TaskId.eq(task_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(MediaPagingError::NotFound)?;
        if task.state == TASK_COMPLETE {
            return Err(MediaPagingError::Conflict(
                "cannot cancel a completed task".into(),
            ));
        }
        let now = Self::now();
        let mut am: media_task::ActiveModel = task.into();
        am.state = Set(TASK_FAILED.to_owned());
        am.stage = Set("cancelled".into());
        am.error_code = Set(Some("cancelled".into()));
        am.retryable = Set(false);
        am.lease_owner = Set(None);
        am.expires_at = Set(None);
        am.updated_at = Set(now);
        let updated = am.update(&txn).await?;
        txn.commit().await?;
        Ok(updated)
    }

    /// Reset a retryable failed task to `pending` for re-queue (same task_id).
    pub async fn requeue_failed_task(
        &self,
        task_id: &str,
    ) -> Result<media_task::Model, MediaPagingError> {
        let db = self.get_connection();
        let txn = db.begin().await?;
        let task = media_task::Entity::find()
            .filter(media_task::Column::TaskId.eq(task_id))
            .lock_exclusive()
            .one(&txn)
            .await?
            .ok_or(MediaPagingError::NotFound)?;
        if task.state != TASK_FAILED || !task.retryable {
            return Err(MediaPagingError::Conflict("task is not retryable".into()));
        }
        let now = Self::now();
        let mut am: media_task::ActiveModel = task.into();
        am.state = Set(TASK_PENDING.to_owned());
        am.stage = Set("queued".into());
        am.error_code = Set(None);
        am.lease_owner = Set(None);
        am.expires_at = Set(None);
        am.bytes_verified = Set(0);
        am.pages_verified = Set(0);
        am.updated_at = Set(now);
        let updated = am.update(&txn).await?;
        txn.commit().await?;
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn storage() -> (TempDir, MediaPagingStorage) {
        let dir = TempDir::new().unwrap();
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let base = BaseStorage::new(std::sync::Arc::new(db));
        (dir, MediaPagingStorage::new(base))
    }

    fn page(page_no: i32, start_offset: u64, hashes: &[&str]) -> (i32, Vec<ChunkIndexRow>) {
        let mut rows = Vec::new();
        let mut off = start_offset;
        for (i, h) in hashes.iter().enumerate() {
            let len = 32768u64;
            rows.push(ChunkIndexRow {
                page_no,
                ordinal: i as i32,
                offset: off,
                length: len,
                chunk_hash: (*h).to_owned(),
            });
            off += len;
        }
        (page_no, rows)
    }

    #[tokio::test]
    async fn media_paging_page_idempotent_and_conflict() {
        let (_dir, store) = storage().await;
        let scope = "a".repeat(64);
        let mid = "b".repeat(64);
        let exp = MediaPagingStorage::now() + chrono::Duration::hours(1);
        store
            .upsert_pending_session(
                &scope,
                &mid,
                "fastcdc-v2020-32k",
                &("c".repeat(64)),
                65536,
                2,
                1,
                None,
                exp,
            )
            .await
            .unwrap();
        let (_pn, entries) = page(0, 0, &["h1", "h2"]);
        store
            .put_page_entries(&scope, &mid, 0, &entries)
            .await
            .unwrap();
        store
            .put_page_entries(&scope, &mid, 0, &entries)
            .await
            .unwrap();
        let mut bad = entries.clone();
        bad[0].chunk_hash = "other".into();
        let err = store
            .put_page_entries(&scope, &mid, 0, &bad)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaPagingError::Conflict(_)));
    }

    #[tokio::test]
    async fn media_paging_cross_page_hash_length_conflict() {
        let (_dir, store) = storage().await;
        let scope = "d".repeat(64);
        let mid = "e".repeat(64);
        let exp = MediaPagingStorage::now() + chrono::Duration::hours(1);
        store
            .upsert_pending_session(
                &scope,
                &mid,
                "fastcdc-v2020-32k",
                &("f".repeat(64)),
                98304,
                3,
                2,
                None,
                exp,
            )
            .await
            .unwrap();
        let (_p0, e0) = page(0, 0, &["same"]);
        store.put_page_entries(&scope, &mid, 0, &e0).await.unwrap();
        let mut e1 = vec![ChunkIndexRow {
            page_no: 1,
            ordinal: 0,
            offset: 32768,
            length: 1, // conflict with prior length 32768
            chunk_hash: "same".into(),
        }];
        let err = store
            .put_page_entries(&scope, &mid, 1, &e1)
            .await
            .unwrap_err();
        assert!(matches!(err, MediaPagingError::Conflict(_)));
        e1[0].length = 32768;
        store.put_page_entries(&scope, &mid, 1, &e1).await.unwrap();
    }

    #[tokio::test]
    async fn media_paging_seal_serializes_and_rejects_gap() {
        let (_dir, store) = storage().await;
        let scope = "g".repeat(64);
        let mid = "h".repeat(64);
        let exp = MediaPagingStorage::now() + chrono::Duration::hours(1);
        store
            .upsert_pending_session(
                &scope,
                &mid,
                "fastcdc-v2020-32k",
                &("i".repeat(64)),
                65536,
                2,
                2,
                None,
                exp,
            )
            .await
            .unwrap();
        let (_p0, e0) = page(0, 0, &["a"]);
        store.put_page_entries(&scope, &mid, 0, &e0).await.unwrap();
        // Missing page 1
        let err = store.seal_session(&scope, &mid).await.unwrap_err();
        assert!(matches!(err, MediaPagingError::Conflict(_)));
        let (_p1, e1) = page(1, 32768, &["b"]);
        store.put_page_entries(&scope, &mid, 1, &e1).await.unwrap();
        let sealed = store.seal_session(&scope, &mid).await.unwrap();
        assert_eq!(sealed.state, STATE_SEALED);
        assert_eq!(sealed.seal_generation, 1);
        let again = store.seal_session(&scope, &mid).await.unwrap();
        assert_eq!(again.seal_generation, 1);
    }

    #[tokio::test]
    async fn media_paging_offset_cover_bounded() {
        let (_dir, store) = storage().await;
        let scope = "j".repeat(64);
        let mid = "k".repeat(64);
        let exp = MediaPagingStorage::now() + chrono::Duration::hours(1);
        store
            .upsert_pending_session(
                &scope,
                &mid,
                "fastcdc-v2020-32k",
                &("l".repeat(64)),
                131072,
                4,
                1,
                None,
                exp,
            )
            .await
            .unwrap();
        let (_p0, e0) = page(0, 0, &["c0", "c1", "c2", "c3"]);
        store.put_page_entries(&scope, &mid, 0, &e0).await.unwrap();
        let cover = store
            .entries_covering_range(&scope, &mid, 40000, 20000, 4096)
            .await
            .unwrap();
        assert!(!cover.is_empty());
        assert!(cover.iter().all(|r| {
            let start = r.offset as u64;
            let end = start + r.length as u64;
            start < 60000 && end > 40000
        }));
    }

    #[tokio::test]
    async fn media_paging_lease_epoch_fencing_and_rebuild() {
        let (_dir, store) = storage().await;
        let scope = "m".repeat(64);
        let mid = "n".repeat(64);
        let exp = MediaPagingStorage::now() + chrono::Duration::hours(1);
        store
            .upsert_pending_session(
                &scope,
                &mid,
                "fastcdc-v2020-32k",
                &("o".repeat(64)),
                32768,
                1,
                1,
                None,
                exp,
            )
            .await
            .unwrap();
        let (_p0, e0) = page(0, 0, &["only"]);
        store.put_page_entries(&scope, &mid, 0, &e0).await.unwrap();
        store.seal_session(&scope, &mid).await.unwrap();

        let task = store.ensure_task(&scope, &mid).await.unwrap();
        let claimed = store
            .claim_or_renew_lease(&task.task_id, "worker-a", None)
            .await
            .unwrap();
        assert_eq!(claimed.lease_epoch, 1);
        let stale = store
            .update_task_progress(&task.task_id, 0, 10, 0, "verify")
            .await
            .unwrap_err();
        assert!(matches!(stale, MediaPagingError::StaleLease));
        store
            .update_task_progress(&task.task_id, claimed.lease_epoch, 100, 1, "verify")
            .await
            .unwrap();
        store
            .complete_task(&task.task_id, claimed.lease_epoch)
            .await
            .unwrap();

        // Rebuild index after simulated corruption.
        store
            .rebuild_index_from_pages(&scope, &mid, &[(0, e0.clone())])
            .await
            .unwrap();
        let cover = store
            .entries_covering_range(&scope, &mid, 0, 32768, 16)
            .await
            .unwrap();
        assert_eq!(cover.len(), 1);
    }
}
