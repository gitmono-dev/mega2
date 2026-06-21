use async_trait::async_trait;

use super::{CHANNEL_IN_APP, NotificationChannel, OutboundMessage};
use crate::{
    common::errors::MegaError, jupiter::storage::notification_storage::NotificationStorage,
};

/// In-app (inbox) delivery channel: persists a notification row the user can
/// read inside the product (docs/notification.md phase 4).
///
/// Backed by the `user_inbox_notifications` table via [`NotificationStorage`].
/// Used as a secondary channel alongside email: the dispatcher fans a delivered
/// notification out to the inbox best-effort after the email send succeeds.
pub struct InAppChannel {
    stg: NotificationStorage,
}

impl InAppChannel {
    pub fn new(stg: NotificationStorage) -> Self {
        Self { stg }
    }
}

#[async_trait]
impl NotificationChannel for InAppChannel {
    fn name(&self) -> &'static str {
        CHANNEL_IN_APP
    }

    async fn deliver(&self, message: &OutboundMessage<'_>) -> Result<(), MegaError> {
        self.stg
            .create_inbox_notification(
                message.username,
                message.event_type_code,
                message.subject,
                message.body_html,
                message.body_text,
            )
            .await
            .map_err(MegaError::from)
    }
}
