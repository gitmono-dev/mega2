//! Test helpers for the notification subsystem.
//!
//! Provides a capturing [`MockChannel`] that records every outbound message it
//! would have delivered. Intended for unit and integration tests that need to
//! assert on dispatcher fan-out or channel routing without depending on
//! external SMTP, Slack, or webhook endpoints.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::channels::{NotificationChannel, OutboundMessage};
use crate::common::errors::MegaError;

/// A single captured outbound notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedNotification {
    pub channel_name: String,
    pub username: String,
    pub event_type_code: String,
    pub to: String,
    pub subject: String,
    pub body_html: String,
    pub body_text: Option<String>,
    pub attachment_count: usize,
}

/// In-memory notification channel that records every delivered message.
#[derive(Clone)]
pub struct MockChannel {
    name: &'static str,
    enabled: bool,
    mailbox: Arc<Mutex<Vec<CapturedNotification>>>,
}

impl MockChannel {
    pub fn new(name: &'static str, enabled: bool) -> Self {
        Self {
            name,
            enabled,
            mailbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns a clone of all captured notifications without clearing the mailbox.
    pub fn sent(&self) -> Vec<CapturedNotification> {
        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Returns all captured notifications and clears the mailbox.
    pub fn take_sent(&self) -> Vec<CapturedNotification> {
        std::mem::take(&mut self.mailbox.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Default for MockChannel {
    fn default() -> Self {
        Self::new("mock", true)
    }
}

#[async_trait]
impl NotificationChannel for MockChannel {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn deliver(&self, message: &OutboundMessage<'_>) -> Result<(), MegaError> {
        if !self.enabled {
            return Ok(());
        }

        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(CapturedNotification {
                channel_name: self.name.to_string(),
                username: message.username.to_owned(),
                event_type_code: message.event_type_code.to_owned(),
                to: message.to.to_owned(),
                subject: message.subject.to_owned(),
                body_html: message.body_html.to_owned(),
                body_text: message.body_text.map(str::to_owned),
                attachment_count: message.attachments.len(),
            });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::MailAttachment;

    #[tokio::test]
    async fn mock_channel_captures_notification() {
        let channel = MockChannel::default();
        let attachment = MailAttachment::new("file.txt", "text/plain", b"content");
        let message = OutboundMessage {
            username: "alice",
            event_type_code: "cl.comment.created",
            to: "alice@example.com",
            subject: "New comment",
            body_html: "<p>hello</p>",
            body_text: Some("hello"),
            attachments: std::slice::from_ref(&attachment),
        };

        channel.deliver(&message).await.unwrap();

        let sent = channel.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel_name, "mock");
        assert_eq!(sent[0].username, "alice");
        assert_eq!(sent[0].event_type_code, "cl.comment.created");
        assert_eq!(sent[0].attachment_count, 1);
    }

    #[tokio::test]
    async fn disabled_mock_channel_does_not_capture() {
        let channel = MockChannel::new("silent", false);
        let message = OutboundMessage {
            username: "bob",
            event_type_code: "issue.comment.created",
            to: "bob@example.com",
            subject: "Comment",
            body_html: "body",
            body_text: None,
            attachments: &[],
        };

        channel.deliver(&message).await.unwrap();

        assert!(channel.sent().is_empty());
    }

    #[test]
    fn take_sent_clears_mailbox() {
        let channel = MockChannel::default();
        let message = OutboundMessage {
            username: "carol",
            event_type_code: "cl.merged",
            to: "carol@example.com",
            subject: "Merged",
            body_html: "merged",
            body_text: None,
            attachments: &[],
        };

        // `deliver` is async; for this test we only need the mailbox helper.
        channel.mailbox.lock().unwrap().push(CapturedNotification {
            channel_name: "mock".to_string(),
            username: message.username.to_owned(),
            event_type_code: message.event_type_code.to_owned(),
            to: message.to.to_owned(),
            subject: message.subject.to_owned(),
            body_html: message.body_html.to_owned(),
            body_text: message.body_text.map(str::to_owned),
            attachment_count: 0,
        });

        assert_eq!(channel.take_sent().len(), 1);
        assert!(channel.sent().is_empty());
    }
}
