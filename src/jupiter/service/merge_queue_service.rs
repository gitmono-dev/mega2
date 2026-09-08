use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{
    callisto::sea_orm_active_enums::{
        MergeStatusEnum, PushQueueKindEnum, QueueFailureTypeEnum, QueueStatusEnum,
    },
    common::{errors::MegaError, utils::ZERO_ID},
    jupiter::{
        model::merge_queue_dto::QueueStats,
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            cl_storage::ClStorage,
            merge_queue_storage::MergeQueueStorage,
            push_queue_storage::{EnqueueOutcome, EnqueueParams, PushQueueStorage},
        },
    },
};

/// Merge queue service for CL processing
#[derive(Clone)]
pub struct MergeQueueService {
    merge_queue_storage: MergeQueueStorage,
    cl_storage: ClStorage,
    processor_running: Arc<AtomicBool>,
}

/// Counts produced by [`MergeQueueService::absorb_into_push_queue`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AbsorbReport {
    pub queued: u64,
    pub failed_interrupted: u64,
}

impl MergeQueueService {
    pub fn new(base_storage: BaseStorage) -> Self {
        Self {
            merge_queue_storage: MergeQueueStorage::new(base_storage.clone()),
            cl_storage: ClStorage {
                base: base_storage.clone(),
            },
            processor_running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Adds a CL to the merge queue.
    ///
    /// Note: This method only adds to queue. The background processor
    /// should be started by the caller (MonoApiService) after this call.
    pub async fn add_to_queue(&self, cl_link: String) -> Result<i64, MegaError> {
        self.add_to_queue_with_requester(cl_link, None).await
    }

    /// Enqueue a CL, recording the subject that requested it (UN-20).
    /// `None` = anonymous.
    pub async fn add_to_queue_with_requester(
        &self,
        cl_link: String,
        requester: Option<String>,
    ) -> Result<i64, MegaError> {
        self.validate_cl_for_queue(&cl_link).await?;

        let position = self
            .merge_queue_storage
            .add_to_queue_with_requester(cl_link, requester)
            .await
            .map_err(MegaError::Other)?;

        Ok(position)
    }

    pub async fn remove_from_queue(&self, cl_link: &str) -> Result<bool, MegaError> {
        self.merge_queue_storage
            .remove_from_queue(cl_link)
            .await
            .map_err(MegaError::Other)
    }

    pub async fn get_queue_list(
        &self,
    ) -> Result<Vec<crate::callisto::merge_queue::Model>, MegaError> {
        self.merge_queue_storage
            .get_queue_list()
            .await
            .map_err(MegaError::Other)
    }

    pub async fn get_cl_queue_status(
        &self,
        cl_link: &str,
    ) -> Result<Option<crate::callisto::merge_queue::Model>, MegaError> {
        self.merge_queue_storage
            .get_cl_queue_status(cl_link)
            .await
            .map_err(MegaError::Other)
    }

    pub async fn get_display_position(&self, cl_link: &str) -> Result<Option<usize>, MegaError> {
        self.merge_queue_storage
            .get_display_position(cl_link)
            .await
            .map_err(MegaError::Other)
    }

    pub async fn get_display_position_by_position(
        &self,
        position: i64,
    ) -> Result<usize, MegaError> {
        self.merge_queue_storage
            .get_display_position_by_position(position)
            .await
            .map_err(MegaError::Other)
    }

    pub async fn get_queue_stats(&self) -> Result<QueueStats, MegaError> {
        self.merge_queue_storage
            .get_queue_stats()
            .await
            .map_err(MegaError::Other)
    }

    // ========== Methods for MonoApiService to use ==========

    /// Gets the next waiting item from the queue.
    ///
    /// Called by MonoApiService's background processor.
    pub async fn get_next_waiting_item(
        &self,
    ) -> Result<Option<crate::callisto::merge_queue::Model>, MegaError> {
        self.merge_queue_storage
            .get_next_waiting_item()
            .await
            .map_err(MegaError::Other)
    }

    /// Updates the status of a queue item.
    ///
    /// Returns true if update was successful, false if item was cancelled/not found.
    pub async fn update_item_status(
        &self,
        cl_link: &str,
        status: QueueStatusEnum,
    ) -> Result<bool, MegaError> {
        self.merge_queue_storage
            .update_item_status(cl_link, status)
            .await
            .map_err(MegaError::Other)
    }

    /// Updates item status to Failed with error details.
    pub async fn update_item_status_with_error(
        &self,
        cl_link: &str,
        failure_type: QueueFailureTypeEnum,
        message: String,
    ) -> Result<bool, MegaError> {
        self.merge_queue_storage
            .update_item_status_with_error(cl_link, failure_type, message)
            .await
            .map_err(MegaError::Other)
    }

    /// Moves a conflicting item to the tail of the queue for retry.
    pub async fn move_item_to_tail(&self, cl_link: &str) -> Result<bool, MegaError> {
        self.merge_queue_storage
            .move_item_to_tail(cl_link)
            .await
            .map_err(MegaError::Other)
    }

    // ========== Processor control methods ==========

    /// Tries to start the processor. Returns true if this call started it,
    /// false if it was already running.
    ///
    /// The actual processor loop should be implemented in MonoApiService (ceres layer).
    pub fn try_start_processor(&self) -> bool {
        self.processor_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// Stops the processor by setting the running flag to false.
    pub fn stop_processor(&self) {
        self.processor_running.store(false, Ordering::SeqCst);
    }

    /// Checks if the processor is currently running.
    pub fn is_processor_running(&self) -> bool {
        self.processor_running.load(Ordering::SeqCst)
    }

    // ========== Validation and helper methods ==========

    /// Validates CL exists and is not closed before adding to queue
    async fn validate_cl_for_queue(&self, cl_link: &str) -> Result<(), MegaError> {
        let cl = self.cl_storage.get_cl(cl_link).await?;

        match cl {
            Some(cl_model) => match cl_model.status {
                MergeStatusEnum::Open => Ok(()),
                MergeStatusEnum::Closed => {
                    Err(MegaError::Other("Cannot queue a closed CL".to_string()))
                }
                MergeStatusEnum::Merged => {
                    Err(MegaError::Other("Cannot queue a merged CL".to_string()))
                }
                MergeStatusEnum::Draft => {
                    Err(MegaError::Other("Cannot queue a draft CL".to_string()))
                }
            },
            None => Err(MegaError::Other("CL not found".to_string())),
        }
    }

    pub async fn cancel_all_pending(&self) -> Result<u64, MegaError> {
        let count = self
            .merge_queue_storage
            .cancel_all_pending()
            .await
            .map_err(MegaError::Other)?;
        Ok(count)
    }

    /// Retries a failed queue item by resetting its status to Waiting.
    ///
    /// Note: The caller (MonoApiService) should start the processor after this.
    pub async fn retry_queue_item(&self, cl_link: &str) -> Result<bool, MegaError> {
        self.merge_queue_storage
            .retry_failed_item(cl_link)
            .await
            .map_err(MegaError::Other)
    }

    /// Retry a failed item, recording the subject that requested the retry
    /// (UN-20). `None` = anonymous.
    pub async fn retry_queue_item_with_requester(
        &self,
        cl_link: &str,
        requester: Option<String>,
    ) -> Result<bool, MegaError> {
        self.merge_queue_storage
            .retry_failed_item_with_requester(cl_link, requester)
            .await
            .map_err(MegaError::Other)
    }

    /// Freeze a queued item because authorization could not be decided
    /// (UN-25). Reuses the existing terminal state — `Failed` +
    /// `SystemError` — so the existing retry entry point keeps working; the
    /// recorded requester is untouched.
    pub async fn freeze_item_for_authz(
        &self,
        cl_link: &str,
        message: &str,
    ) -> Result<bool, MegaError> {
        self.merge_queue_storage
            .update_item_status_with_error(
                cl_link,
                QueueFailureTypeEnum::SystemError,
                message.to_owned(),
            )
            .await
            .map_err(MegaError::Other)
    }

    /// The subject recorded for a queued CL (UN-20); consumed by the queue's
    /// execution decision (UN-17).
    pub async fn get_queue_requester(
        &self,
        cl_link: &str,
    ) -> Result<Option<Option<String>>, MegaError> {
        self.merge_queue_storage
            .get_requester(cl_link)
            .await
            .map_err(MegaError::Other)
    }

    /// Absorb leftover `merge_queue` rows into `push_queue` when switching
    /// `merge_writer=queue` (TP-07 rolling deploy).
    ///
    /// `Waiting`/`Testing` → `push_queue` `Queued`; `Merging` →
    /// `Failed(MergeFailure, interrupted)`. Each source row is deleted after
    /// the destination insert.
    pub async fn absorb_into_push_queue(
        &self,
        push_queue: &PushQueueStorage,
    ) -> Result<AbsorbReport, MegaError> {
        let items = self
            .merge_queue_storage
            .list_absorb_candidates()
            .await
            .map_err(MegaError::Other)?;
        let mut report = AbsorbReport::default();
        for item in items {
            let cl = self.cl_storage.get_cl(&item.cl_link).await?;
            let (path, old_id, new_id) = match &cl {
                Some(cl) => (cl.path.clone(), cl.from_hash.clone(), cl.to_hash.clone()),
                None => ("/".to_owned(), ZERO_ID.to_owned(), ZERO_ID.to_owned()),
            };
            match item.status {
                QueueStatusEnum::Waiting | QueueStatusEnum::Testing => {
                    let outcome = push_queue
                        .enqueue_atomic(EnqueueParams {
                            kind: PushQueueKindEnum::Merge,
                            operation_id: &item.cl_link,
                            path: &path,
                            old_id: &old_id,
                            new_id: &new_id,
                            requester: item.requester.as_deref(),
                            payload: serde_json::json!({
                                "cl_link": item.cl_link,
                                "authz_principal": item.requester.clone().unwrap_or_else(|| "system".into()),
                                "execution_actor": "system",
                                "apply_queue_execution_decision": true,
                                "requester": item.requester,
                            }),
                        })
                        .await?;
                    match outcome {
                        EnqueueOutcome::Inserted { .. }
                        | EnqueueOutcome::Adopted { .. }
                        | EnqueueOutcome::Replay { .. } => {}
                        EnqueueOutcome::Rejected { reason } => {
                            return Err(MegaError::Other(format!(
                                "absorb enqueue rejected for {}: {reason:?}",
                                item.cl_link
                            )));
                        }
                    }
                    report.queued += 1;
                }
                QueueStatusEnum::Merging => {
                    push_queue
                        .insert_absorbed_failed_merge(
                            &item.cl_link,
                            &path,
                            &old_id,
                            &new_id,
                            item.requester.as_deref(),
                            "interrupted",
                        )
                        .await?;
                    report.failed_interrupted += 1;
                }
                _ => {}
            }
            self.merge_queue_storage
                .delete_by_pk(item.id)
                .await
                .map_err(MegaError::Other)?;
        }
        Ok(report)
    }

    /// Queue-mode HTTP startup: disable the legacy processor, absorb leftover
    /// `merge_queue` rows, then refuse to start if any non-terminal rows remain.
    pub async fn prepare_for_queue_writer(
        &self,
        push_queue: &PushQueueStorage,
    ) -> Result<AbsorbReport, MegaError> {
        self.stop_processor();
        let report = self.absorb_into_push_queue(push_queue).await?;
        let left = self
            .merge_queue_storage
            .list_absorb_candidates()
            .await
            .map_err(MegaError::Other)?;
        if !left.is_empty() {
            return Err(MegaError::Other(format!(
                "merge_writer=queue requires non-terminal merge_queue rows to be drained; {} remain after absorb",
                left.len()
            )));
        }
        Ok(report)
    }

    pub fn mock() -> Self {
        let base_storage = BaseStorage::mock();
        Self::new(base_storage)
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::{ActiveModelTrait, Set};

    use super::*;
    use crate::{
        callisto::{merge_queue, sea_orm_active_enums::PushQueueStatusEnum},
        jupiter::{storage::base_storage::StorageConnector, tests::test_storage_queue_merge},
    };

    async fn seed_open_cl(storage: &crate::jupiter::storage::Storage, link: &str, path: &str) {
        storage
            .cl_storage()
            .new_cl_model(
                path,
                link,
                "absorb test",
                "main",
                &"a".repeat(40),
                &"b".repeat(40),
                "alice",
            )
            .await
            .unwrap();
    }

    async fn insert_mq(
        storage: &crate::jupiter::storage::Storage,
        link: &str,
        status: QueueStatusEnum,
    ) {
        let now = chrono::Utc::now().naive_utc();
        merge_queue::ActiveModel {
            id: Set(crate::common::utils::generate_id()),
            cl_link: Set(link.to_owned()),
            status: Set(status),
            position: Set(chrono::Utc::now().timestamp_millis()),
            retry_count: Set(0),
            last_retry_at: Set(None),
            failure_type: Set(None),
            error_message: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            requester: Set(Some("alice".into())),
        }
        .insert(storage.merge_queue_storage().get_connection())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn absorb_waiting_and_testing_become_queued() {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        use crate::callisto::push_queue;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage_queue_merge(temp.path()).await;
        seed_open_cl(&storage, "CLWAIT", "/p-wait").await;
        seed_open_cl(&storage, "CLTEST", "/p-test").await;
        insert_mq(&storage, "CLWAIT", QueueStatusEnum::Waiting).await;
        insert_mq(&storage, "CLTEST", QueueStatusEnum::Testing).await;

        let report = storage
            .merge_queue_service
            .prepare_for_queue_writer(storage.push_queue_service.storage())
            .await
            .unwrap();
        assert_eq!(report.queued, 2);
        assert_eq!(report.failed_interrupted, 0);
        assert!(!storage.merge_queue_service.is_processor_running());

        let left = storage
            .merge_queue_storage()
            .list_absorb_candidates()
            .await
            .unwrap();
        assert!(left.is_empty());

        for link in ["CLWAIT", "CLTEST"] {
            let rows = push_queue::Entity::find()
                .filter(push_queue::Column::OperationId.eq(link))
                .all(storage.push_queue_storage().get_connection())
                .await
                .unwrap();
            assert_eq!(rows.len(), 1, "{link}");
            assert_eq!(rows[0].status, PushQueueStatusEnum::Queued);
            assert_eq!(rows[0].kind, PushQueueKindEnum::Merge);
        }
    }

    #[tokio::test]
    async fn absorb_merging_becomes_failed_interrupted() {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        use crate::callisto::push_queue;

        let temp = tempfile::TempDir::new().unwrap();
        let storage = test_storage_queue_merge(temp.path()).await;
        seed_open_cl(&storage, "CLMERGE", "/p-merge").await;
        insert_mq(&storage, "CLMERGE", QueueStatusEnum::Merging).await;

        let report = storage
            .merge_queue_service
            .prepare_for_queue_writer(storage.push_queue_service.storage())
            .await
            .unwrap();
        assert_eq!(report.queued, 0);
        assert_eq!(report.failed_interrupted, 1);
        assert!(!storage.merge_queue_service.is_processor_running());
        assert!(
            storage
                .merge_queue_storage()
                .list_absorb_candidates()
                .await
                .unwrap()
                .is_empty()
        );
        let rows = push_queue::Entity::find()
            .filter(push_queue::Column::OperationId.eq("CLMERGE"))
            .all(storage.push_queue_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, PushQueueStatusEnum::Failed);
        assert_eq!(
            rows[0].failure_type,
            Some(crate::callisto::sea_orm_active_enums::PushQueueFailureEnum::MergeFailure)
        );
        assert_eq!(rows[0].error_message.as_deref(), Some("interrupted"));
    }
}
