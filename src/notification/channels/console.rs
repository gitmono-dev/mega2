use async_trait::async_trait;

use super::{CHANNEL_CONSOLE, NotificationChannel, OutboundMessage};
use crate::common::errors::MegaError;

/// Console (dry-run) delivery channel: logs a redacted summary of each
/// notification without actually sending it. Used for dev/CI/local
/// environments where real delivery is not needed, and as a proof that
/// the `NotificationChannel` abstraction is not email-specific.
pub struct ConsoleChannel;

impl ConsoleChannel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ConsoleChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NotificationChannel for ConsoleChannel {
    fn name(&self) -> &'static str {
        CHANNEL_CONSOLE
    }

    async fn deliver(&self, message: &OutboundMessage<'_>) -> Result<(), MegaError> {
        tracing::info!(
            channel = "console",
            recipient = %crate::notification::redact::redact_email(message.to),
            username = %message.username,
            event_type = %message.event_type_code,
            subject_len = message.subject.len(),
            body_html_len = message.body_html.len(),
            body_text_len = message.body_text.map(|s| s.len()),
            "console channel dry-run delivery (no actual send)"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn console_channel_logs_and_succeeds() {
        let channel = ConsoleChannel::new();
        assert_eq!(channel.name(), "console");

        let message = OutboundMessage {
            username: "alice",
            event_type_code: "cl_comment_created",
            to: "alice@example.com",
            subject: "New comment",
            body_html: "<p>hello</p>",
            body_text: Some("hello"),
        };

        let result = channel.deliver(&message).await;
        assert!(result.is_ok());
    }
}
