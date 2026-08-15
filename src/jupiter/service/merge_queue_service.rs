use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{
    callisto::sea_orm_active_enums::{MergeStatusEnum, QueueFailureTypeEnum, QueueStatusEnum},
    common::errors::MegaError,
    jupiter::{
        model::merge_queue_dto::QueueStats,
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            cl_storage::ClStorage,
            merge_queue_storage::MergeQueueStorage,
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

    pub fn mock() -> Self {
        let base_storage = BaseStorage::mock();
        Self::new(base_storage)
    }
}
