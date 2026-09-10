use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::callisto::sea_orm_active_enums::{PushQueueFailureEnum, PushQueueStatusEnum};

/// CL queue status for API
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum QueueStatus {
    Waiting,
    Testing,
    Merging,
    Merged,
    Failed,
}

/// Failure type for API
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum FailureType {
    TestFailure,
    BuildFailure,
    Conflict,
    MergeFailure,
    SystemError,
    Timeout,
}

/// Error details for API
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueueError {
    pub failure_type: FailureType,
    pub message: String,
    pub occurred_at: String,
}

/// Queue item for API
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueueItem {
    pub cl_link: String,
    pub status: QueueStatus,
    pub position: i64,
    pub display_position: Option<usize>,
    pub created_at: String,
    pub updated_at: String,
    pub retry_count: i32,
    pub error: Option<QueueError>,
}

/// Queue statistics for API
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueueStats {
    pub total_items: usize,
    pub waiting_count: usize,
    pub testing_count: usize,
    pub merging_count: usize,
    pub failed_count: usize,
    pub merged_count: usize,
}

/// Add CL to queue request
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AddToQueueRequest {
    pub cl_link: String,
}

/// Add CL to queue response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AddToQueueResponse {
    pub success: bool,
    pub position: i64,
    pub display_position: Option<usize>,
    pub message: String,
}

/// Queue list response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct QueueListResponse {
    pub items: Vec<QueueItem>,
    pub total_count: usize,
}

/// Queue status check response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct QueueStatusResponse {
    pub in_queue: bool,
    pub item: Option<QueueItem>,
}

/// Queue statistics response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct QueueStatsResponse {
    pub stats: QueueStats,
}

impl QueueItem {
    /// Map a MonoWriteQueue row onto the legacy merge-queue list shape.
    pub fn from_push_queue(row: &crate::callisto::push_queue::Model) -> Self {
        let status = match row.status {
            PushQueueStatusEnum::Queued => QueueStatus::Waiting,
            PushQueueStatusEnum::Running => QueueStatus::Merging,
            PushQueueStatusEnum::Done => QueueStatus::Merged,
            PushQueueStatusEnum::Failed | PushQueueStatusEnum::Cancelled => QueueStatus::Failed,
        };
        let error = row.failure_type.as_ref().map(|ft| {
            let occurred_at_local = row
                .updated_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string();
            QueueError {
                failure_type: match ft {
                    PushQueueFailureEnum::Conflict => FailureType::Conflict,
                    PushQueueFailureEnum::MergeFailure => FailureType::MergeFailure,
                    PushQueueFailureEnum::SystemError => FailureType::SystemError,
                    _ => FailureType::SystemError,
                },
                message: row.error_message.clone().unwrap_or_default(),
                occurred_at: occurred_at_local,
            }
        });
        QueueItem {
            cl_link: row.operation_id.clone(),
            status,
            position: row.id,
            display_position: None,
            created_at: row
                .enqueued_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            updated_at: row
                .updated_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string(),
            retry_count: 0,
            error,
        }
    }
}

impl From<QueueStats> for QueueStatsResponse {
    fn from(stats: QueueStats) -> Self {
        QueueStatsResponse { stats }
    }
}
