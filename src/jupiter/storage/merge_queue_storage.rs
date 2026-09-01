use std::ops::Deref;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};

use crate::{
    callisto::{
        merge_queue::{ActiveModel, Column, Entity, Model},
        sea_orm_active_enums::{QueueFailureTypeEnum, QueueStatusEnum},
    },
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

/// Maximum number of retry attempts for failed items
const MAX_RETRY_ATTEMPTS: i32 = 3;

/// Merge queue storage layer
#[derive(Clone)]
pub struct MergeQueueStorage {
    base: BaseStorage,
}

impl Deref for MergeQueueStorage {
    type Target = BaseStorage;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl MergeQueueStorage {
    pub fn new(base: BaseStorage) -> Self {
        Self { base }
    }

    /// Adds CL to queue with timestamp position
    pub async fn add_to_queue(&self, cl_link: String) -> Result<i64, String> {
        self.add_to_queue_with_requester(cl_link, None).await
    }

    /// Enqueue a CL and record the subject that requested it (UN-20).
    ///
    /// The requester is part of the **same insert** as the queue row: a queued
    /// merge is executed later by a background worker, so a row that exists
    /// without its requester would be a merge with no subject of its own.
    /// `None` means the request was anonymous and the column stays NULL.
    pub async fn add_to_queue_with_requester(
        &self,
        cl_link: String,
        requester: Option<String>,
    ) -> Result<i64, String> {
        let db = self.get_connection();

        // Check if CL is already in queue (any status)
        let existing = Entity::find()
            .filter(Column::ClLink.eq(&cl_link))
            .one(db)
            .await
            .map_err(|e| format!("Failed to check existing CL: {}", e))?;

        if let Some(item) = existing {
            return match item.status {
                QueueStatusEnum::Waiting | QueueStatusEnum::Testing | QueueStatusEnum::Merging => {
                    Err(format!(
                        "CL is already in the queue with status {:?}",
                        item.status
                    ))
                }
                QueueStatusEnum::Merged => {
                    Err("CL has already been merged, cannot add to queue again".to_string())
                }
                QueueStatusEnum::Failed => Err(
                    "CL previously failed, please use retry endpoint instead of adding again"
                        .to_string(),
                ),
            };
        }

        // Use timestamp as position value
        let now = chrono::Utc::now();
        let position = now.timestamp_millis();

        // Create new queue item
        let new_item = ActiveModel {
            id: Set(crate::common::utils::generate_id().map_err(|error| error.to_string())?),
            cl_link: Set(cl_link),
            status: Set(QueueStatusEnum::Waiting),
            position: Set(position),
            retry_count: Set(0),
            last_retry_at: Set(None),
            failure_type: Set(None),
            error_message: Set(None),
            created_at: Set(now.naive_utc()),
            updated_at: Set(now.naive_utc()),
            requester: Set(requester),
        };

        new_item
            .insert(db)
            .await
            .map_err(|e| format!("Failed to insert queue item: {}", e))?;

        Ok(position)
    }

    /// Removes or cancels a CL from the queue.
    /// - Waiting/Failed/Merged/Cancelled: directly deleted
    /// - Testing/Merging: marked as Failed (cancelled), background task will stop
    pub async fn remove_from_queue(&self, cl_link: &str) -> Result<bool, String> {
        let db = self.get_connection();

        let existing = Entity::find()
            .filter(Column::ClLink.eq(cl_link))
            .one(db)
            .await
            .map_err(|e| format!("Failed to check existing CL: {}", e))?;

        if let Some(item) = existing {
            match item.status {
                // Mark Testing/Merging items as Failed instead of deleting
                QueueStatusEnum::Testing | QueueStatusEnum::Merging => {
                    let mut active_model: ActiveModel = item.into();
                    active_model.status = Set(QueueStatusEnum::Failed);
                    active_model.failure_type = Set(Some(QueueFailureTypeEnum::SystemError));
                    active_model.error_message = Set(Some("Cancelled by user".to_string()));
                    active_model.updated_at = Set(chrono::Utc::now().naive_utc());

                    active_model
                        .update(db)
                        .await
                        .map_err(|e| format!("Failed to cancel queue item: {}", e))?;

                    Ok(true)
                }
                // Delete other statuses directly
                _ => {
                    let delete_result = Entity::delete_by_id(item.id)
                        .exec(db)
                        .await
                        .map_err(|e| format!("Failed to remove queue item: {}", e))?;

                    Ok(delete_result.rows_affected > 0)
                }
            }
        } else {
            Ok(false)
        }
    }

    pub async fn get_queue_list(&self) -> Result<Vec<Model>, String> {
        let db = self.get_connection();

        let items = Entity::find()
            .filter(Column::Status.is_in([
                QueueStatusEnum::Waiting,
                QueueStatusEnum::Testing,
                QueueStatusEnum::Merging,
                QueueStatusEnum::Failed,
            ]))
            .order_by_asc(Column::Position)
            .all(db)
            .await
            .map_err(|e| format!("Failed to fetch queue items: {}", e))?;

        Ok(items)
    }

    pub async fn get_cl_queue_status(&self, cl_link: &str) -> Result<Option<Model>, String> {
        Entity::find()
            .filter(Column::ClLink.eq(cl_link))
            .one(self.get_connection())
            .await
            .map_err(|e| format!("Failed to find item by link: {}", e))
    }

    pub async fn get_next_waiting_item(&self) -> Result<Option<Model>, String> {
        Entity::find()
            .filter(Column::Status.eq(QueueStatusEnum::Waiting))
            .order_by_asc(Column::Position)
            .one(self.get_connection())
            .await
            .map_err(|e| format!("Failed to find waiting items: {}", e))
    }

    /// Updates item status for normal workflow transitions.
    /// Returns false if item is already Failed (cancelled) - use retry_failed_item to re-queue.
    pub async fn update_item_status(
        &self,
        cl_link: &str,
        new_status: QueueStatusEnum,
    ) -> Result<bool, String> {
        let db = self.get_connection();

        let item_model = self.find_item_by_cl_link(cl_link).await?;

        if let Some(item_model) = item_model {
            // Skip if already cancelled/failed
            if matches!(item_model.status, QueueStatusEnum::Failed) {
                return Ok(false);
            }

            let mut active_model: ActiveModel = item_model.into();

            active_model.status = Set(new_status);
            active_model.updated_at = Set(chrono::Utc::now().naive_utc());
            active_model.error_message = Set(None);
            active_model.failure_type = Set(None);

            active_model
                .update(db)
                .await
                .map_err(|e| format!("Failed to update item: {}", e))?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn find_item_by_cl_link(&self, cl_link: &str) -> Result<Option<Model>, String> {
        Entity::find()
            .filter(Column::ClLink.eq(cl_link))
            .one(self.get_connection())
            .await
            .map_err(|e| format!("Failed to find item by cl link: {}", e))
    }

    /// Updates item status to Failed with error details.
    /// Will not overwrite if item is already in Failed state (preserves original error).
    pub async fn update_item_status_with_error(
        &self,
        cl_link: &str,
        failure_type: QueueFailureTypeEnum,
        error: String,
    ) -> Result<bool, String> {
        let db = self.get_connection();

        let item_model = self.find_item_by_cl_link(cl_link).await?;

        if let Some(item_model) = item_model {
            // Preserve original failure reason if already failed
            if matches!(item_model.status, QueueStatusEnum::Failed) {
                tracing::debug!(
                    "Item {} already in failed state, preserving original error",
                    cl_link
                );
                return Ok(false);
            }

            let mut active_model: ActiveModel = item_model.into();

            active_model.status = Set(QueueStatusEnum::Failed);
            active_model.failure_type = Set(Some(failure_type));
            active_model.error_message = Set(Some(error));
            active_model.updated_at = Set(chrono::Utc::now().naive_utc());

            active_model
                .update(db)
                .await
                .map_err(|e| format!("Failed to update item: {}", e))?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Gets queue statistics (optimized with single query)
    pub async fn get_queue_stats(
        &self,
    ) -> Result<crate::jupiter::model::merge_queue_dto::QueueStats, String> {
        let db = self.get_connection();

        let results: Vec<(QueueStatusEnum, i64)> = Entity::find()
            .select_only()
            .column(Column::Status)
            .column_as(Column::Status.count(), "count")
            .group_by(Column::Status)
            .into_tuple()
            .all(db)
            .await
            .map_err(|e| format!("Failed to fetch stats: {}", e))?;

        let mut stats = crate::jupiter::model::merge_queue_dto::QueueStats::default();
        let mut total_items = 0;

        for (status, count) in results {
            let count_usize = count as usize;
            match status {
                QueueStatusEnum::Waiting => stats.waiting_count = count_usize,
                QueueStatusEnum::Testing => stats.testing_count = count_usize,
                QueueStatusEnum::Merging => stats.merging_count = count_usize,
                QueueStatusEnum::Failed => stats.failed_count = count_usize,
                QueueStatusEnum::Merged => stats.merged_count = count_usize,
            }
            total_items += count_usize;
        }
        stats.total_items = total_items;

        Ok(stats)
    }

    async fn calc_display_position(&self, position: i64) -> Result<usize, String> {
        let db = self.get_connection();

        let count = Entity::find()
            .filter(Column::Status.is_in([
                QueueStatusEnum::Waiting,
                QueueStatusEnum::Testing,
                QueueStatusEnum::Merging,
            ]))
            .filter(Column::Position.lte(position))
            .count(db)
            .await
            .map_err(|e| format!("Failed to count queue position: {}", e))?;

        Ok(count as usize)
    }

    pub async fn get_display_position(&self, cl_link: &str) -> Result<Option<usize>, String> {
        let db = self.get_connection();

        let item = Entity::find()
            .filter(Column::ClLink.eq(cl_link))
            .filter(Column::Status.is_in([
                QueueStatusEnum::Waiting,
                QueueStatusEnum::Testing,
                QueueStatusEnum::Merging,
            ]))
            .one(db)
            .await
            .map_err(|e| format!("Failed to find item for display position: {}", e))?;

        let Some(item) = item else {
            return Ok(None);
        };

        let display_position = self.calc_display_position(item.position).await?;

        Ok(Some(display_position))
    }

    pub async fn get_display_position_by_position(&self, position: i64) -> Result<usize, String> {
        self.calc_display_position(position).await
    }

    pub async fn cancel_all_pending(&self) -> Result<u64, String> {
        let db = self.get_connection();

        let now = chrono::Utc::now().naive_utc();

        // Cancel all active items: Waiting, Testing, and Merging
        let update_result = Entity::update_many()
            .set(ActiveModel {
                status: Set(QueueStatusEnum::Failed),
                failure_type: Set(Some(QueueFailureTypeEnum::SystemError)),
                error_message: Set(Some("Operation cancelled by user".to_string())),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(Column::Status.is_in([
                QueueStatusEnum::Waiting,
                QueueStatusEnum::Testing,
                QueueStatusEnum::Merging,
            ]))
            .exec(db)
            .await
            .map_err(|e| format!("Failed to batch cancel items: {}", e))?;

        let affected = update_result.rows_affected;

        if affected == 0 {
            tracing::info!("No pending items to cancel");
        } else {
            tracing::info!("Successfully cancelled {} pending items", affected);
        }

        Ok(affected)
    }

    pub fn mock() -> Self {
        let base_storage = BaseStorage::mock();
        Self::new(base_storage)
    }

    pub async fn retry_failed_item(&self, cl_link: &str) -> Result<bool, String> {
        // `None` here means "leave the recorded requester as it is": a retry
        // that does not know its subject must not erase the one already stored.
        self.retry_failed_item_inner(cl_link, None).await
    }

    /// Retry a failed item and record the subject that requested the retry
    /// (UN-20), in the same update as the queue row. `requester` is `None` for
    /// an anonymous retry, which stores NULL.
    pub async fn retry_failed_item_with_requester(
        &self,
        cl_link: &str,
        requester: Option<String>,
    ) -> Result<bool, String> {
        self.retry_failed_item_inner(cl_link, Some(requester)).await
    }

    /// `requester_update`: outer `None` leaves the column untouched, `Some(v)`
    /// writes `v` (including `None` for anonymous).
    async fn retry_failed_item_inner(
        &self,
        cl_link: &str,
        requester_update: Option<Option<String>>,
    ) -> Result<bool, String> {
        let db = self.get_connection();

        let item = Entity::find()
            .filter(Column::ClLink.eq(cl_link))
            .one(db)
            .await
            .map_err(|e| format!("Failed to find item: {}", e))?;

        if let Some(item) = item {
            if !matches!(item.status, QueueStatusEnum::Failed) {
                return Err("Item is not in failed state".to_string());
            }
            if item.retry_count >= MAX_RETRY_ATTEMPTS {
                return Err("Item has exceeded maximum retry attempts".to_string());
            }

            let mut active_model: ActiveModel = item.into();
            active_model.status = Set(QueueStatusEnum::Waiting);
            active_model.retry_count = Set(active_model.retry_count.unwrap() + 1);
            active_model.last_retry_at = Set(Some(chrono::Utc::now().naive_utc()));
            active_model.position = Set(chrono::Utc::now().timestamp_millis());
            active_model.failure_type = Set(None);
            active_model.error_message = Set(None);
            if let Some(requester) = requester_update {
                active_model.requester = Set(requester);
            }

            active_model
                .update(db)
                .await
                .map_err(|e| format!("Failed to update item for retry: {}", e))?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// The subject recorded for a queued CL (UN-20).
    ///
    /// The outer `Option` distinguishes "no such queue row" from a row whose
    /// requester is unknown; the inner one is the anonymous / legacy-NULL case
    /// (rows written before the column existed read back as `None`).
    pub async fn get_requester(&self, cl_link: &str) -> Result<Option<Option<String>>, String> {
        let db = self.get_connection();
        let item = Entity::find()
            .filter(Column::ClLink.eq(cl_link))
            .one(db)
            .await
            .map_err(|e| format!("Failed to find item: {}", e))?;
        Ok(item.map(|item| item.requester))
    }

    pub async fn move_item_to_tail(&self, cl_link: &str) -> Result<bool, String> {
        let db = self.get_connection();

        let item = Entity::find()
            .filter(Column::ClLink.eq(cl_link))
            .one(db)
            .await
            .map_err(|e| format!("Failed to find item: {}", e))?;

        if let Some(item) = item {
            // Skip items already in Failed state
            if matches!(item.status, QueueStatusEnum::Failed) {
                tracing::info!(
                    "Skipping move_item_to_tail for {} - item is already in failed state",
                    cl_link
                );
                return Ok(false);
            }

            let mut active_model: ActiveModel = item.into();
            active_model.status = Set(QueueStatusEnum::Waiting);
            active_model.position = Set(chrono::Utc::now().timestamp_millis());
            active_model.updated_at = Set(chrono::Utc::now().naive_utc());

            active_model
                .update(db)
                .await
                .map_err(|e| format!("Failed to update item: {}", e))?;

            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::ConnectionTrait;
    use tempfile::TempDir;

    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn storage() -> (TempDir, MergeQueueStorage) {
        let temp_dir = TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp_dir.path()).await;
        apply_migrations(&conn, false).await.expect("migrations");
        (
            temp_dir,
            MergeQueueStorage::new(BaseStorage::new(std::sync::Arc::new(conn))),
        )
    }

    /// UN-20: the requester lands in the same insert as the queue row, so a
    /// queued merge never exists without the subject that asked for it.
    #[tokio::test]
    async fn un20_add_to_queue_records_the_requester_in_the_same_insert() {
        let (_temp, storage) = storage().await;

        storage
            .add_to_queue_with_requester("UN20WITH".to_string(), Some("alice".to_string()))
            .await
            .expect("enqueue with requester");

        assert_eq!(
            storage.get_requester("UN20WITH").await.expect("read back"),
            Some(Some("alice".to_string())),
            "the requester is readable straight after the insert"
        );
    }

    #[tokio::test]
    async fn un20_an_anonymous_enqueue_stores_null() {
        let (_temp, storage) = storage().await;

        storage
            .add_to_queue_with_requester("UN20ANON".to_string(), None)
            .await
            .expect("anonymous enqueue");

        assert_eq!(
            storage.get_requester("UN20ANON").await.expect("read back"),
            Some(None),
            "an anonymous request is recorded as NULL, not as a made-up subject"
        );
    }

    /// The pre-UN-20 entry point keeps its signature *and* its behavior.
    #[tokio::test]
    async fn un20_the_legacy_add_entry_point_is_unchanged() {
        let (_temp, storage) = storage().await;

        storage
            .add_to_queue("UN20LEGACY".to_string())
            .await
            .expect("legacy enqueue");

        assert_eq!(
            storage
                .get_requester("UN20LEGACY")
                .await
                .expect("read back"),
            Some(None)
        );
    }

    #[tokio::test]
    async fn un20_get_requester_distinguishes_a_missing_row_from_an_unknown_subject() {
        let (_temp, storage) = storage().await;
        storage
            .add_to_queue_with_requester("UN20PRESENT".to_string(), None)
            .await
            .expect("enqueue");

        assert_eq!(
            storage.get_requester("UN20ABSENT").await.expect("read"),
            None,
            "no queue row at all"
        );
        assert_eq!(
            storage.get_requester("UN20PRESENT").await.expect("read"),
            Some(None),
            "a row whose subject is unknown"
        );
    }

    /// A row written before the column existed reads back as `None` — the
    /// legacy semantics UN-17's execution decision is built on.
    #[tokio::test]
    async fn un20_legacy_null_rows_read_back_as_none() {
        let (temp, storage) = storage().await;
        let conn = test_db_connection(temp.path()).await;
        let _ = conn;

        storage
            .get_connection()
            .execute_unprepared(
                "INSERT INTO merge_queue \
                 (id, cl_link, status, position, retry_count, created_at, updated_at) \
                 VALUES (970001, 'UN20NULL', 'failed', 1, 0, now(), now())",
            )
            .await
            .expect("insert a pre-UN-18 shaped row");

        assert_eq!(
            storage.get_requester("UN20NULL").await.expect("read back"),
            Some(None)
        );
    }

    #[tokio::test]
    async fn un20_retry_records_the_requester_and_the_legacy_entry_point_preserves_it() {
        let (_temp, storage) = storage().await;
        storage
            .add_to_queue_with_requester("UN20RETRY".to_string(), Some("alice".to_string()))
            .await
            .expect("enqueue");
        storage
            .get_connection()
            .execute_unprepared(
                "UPDATE merge_queue SET status = 'failed' WHERE cl_link = 'UN20RETRY'",
            )
            .await
            .expect("mark failed");

        // A retry that knows its subject overwrites the recorded one.
        assert!(
            storage
                .retry_failed_item_with_requester("UN20RETRY", Some("bob".to_string()))
                .await
                .expect("retry with requester")
        );
        assert_eq!(
            storage.get_requester("UN20RETRY").await.expect("read back"),
            Some(Some("bob".to_string()))
        );

        // The legacy entry point must not erase it.
        storage
            .get_connection()
            .execute_unprepared(
                "UPDATE merge_queue SET status = 'failed' WHERE cl_link = 'UN20RETRY'",
            )
            .await
            .expect("mark failed again");
        assert!(
            storage
                .retry_failed_item("UN20RETRY")
                .await
                .expect("legacy retry")
        );
        assert_eq!(
            storage.get_requester("UN20RETRY").await.expect("read back"),
            Some(Some("bob".to_string())),
            "a retry with no known subject must not erase the recorded one"
        );

        // An explicitly anonymous retry does clear it.
        storage
            .get_connection()
            .execute_unprepared(
                "UPDATE merge_queue SET status = 'failed' WHERE cl_link = 'UN20RETRY'",
            )
            .await
            .expect("mark failed once more");
        assert!(
            storage
                .retry_failed_item_with_requester("UN20RETRY", None)
                .await
                .expect("anonymous retry")
        );
        assert_eq!(
            storage.get_requester("UN20RETRY").await.expect("read back"),
            Some(None)
        );
    }
}
