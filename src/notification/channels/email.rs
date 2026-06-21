use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;

use super::{CHANNEL_EMAIL, NotificationChannel, OutboundMessage};
use crate::{common::errors::MegaError, mail::Mailer};

/// Sized wrapper holding the current `Arc<dyn Mailer>` so it can live inside an
/// [`ArcSwap`] (which requires a `Sized` payload).
pub struct MailerSlot(pub Arc<dyn Mailer>);

/// Shared, hot-swappable mailer handle. The email channel reads the current
/// mailer per send; a config-reload subscriber can replace it at runtime
/// (docs/mail.md phase 4 dynamic mailer rebuild).
pub type MailerHandle = Arc<ArcSwap<MailerSlot>>;

/// Email delivery channel backed by a [`Mailer`] (SMTP / console / noop).
///
/// Wraps the `email_jobs` outbox delivery: the dispatcher claims a job and hands
/// its rendered payload here, which delegates to the configured mailer. Must be
/// constructed post-Vault so any resolved `mail.password_ref` is already
/// available (see docs/mail.md and docs/config.md phase 5).
///
/// The mailer is held behind an [`ArcSwap`] so it can be rebuilt and swapped at
/// runtime when SMTP host/port/credentials/provider change, without restarting.
pub struct EmailChannel {
    mailer: MailerHandle,
}

impl EmailChannel {
    pub fn new(mailer: Arc<dyn Mailer>) -> Self {
        Self {
            mailer: Arc::new(ArcSwap::from_pointee(MailerSlot(mailer))),
        }
    }

    /// A clonable handle to hot-swap the underlying mailer at runtime. Shared
    /// with the config-reload mailer-rebuild subscriber.
    pub fn mailer_handle(&self) -> MailerHandle {
        Arc::clone(&self.mailer)
    }
}

#[async_trait]
impl NotificationChannel for EmailChannel {
    fn name(&self) -> &'static str {
        CHANNEL_EMAIL
    }

    async fn deliver(&self, message: &OutboundMessage<'_>) -> Result<(), MegaError> {
        let slot = self.mailer.load_full();
        slot.0
            .send_html_with_attachments(
                message.to,
                message.subject,
                message.body_html,
                message.body_text,
                message.attachments,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;

    use super::*;
    use crate::mail::{MailAttachment, Mailer};

    #[derive(Default)]
    struct RecordingMailer {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Mailer for RecordingMailer {
        async fn send_html(
            &self,
            _to: &str,
            _subject: &str,
            _html: &str,
            _text: Option<&str>,
        ) -> Result<(), MegaError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn email_channel_forwards_to_mailer() {
        let mailer = Arc::new(RecordingMailer::default());
        let channel = EmailChannel::new(mailer.clone());
        assert_eq!(channel.name(), "email");

        let attachments: Vec<MailAttachment> = Vec::new();
        let message = OutboundMessage {
            username: "alice",
            event_type_code: "cl.comment.created",
            to: "alice@example.com",
            subject: "Subject",
            body_html: "<p>Body</p>",
            body_text: Some("Body"),
            attachments: &attachments,
        };

        channel
            .deliver(&message)
            .await
            .expect("delivery should succeed");
        assert_eq!(mailer.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn email_channel_hot_swaps_mailer_via_handle() {
        let first = Arc::new(RecordingMailer::default());
        let channel = EmailChannel::new(first.clone());
        let handle = channel.mailer_handle();

        let attachments: Vec<MailAttachment> = Vec::new();
        let message = OutboundMessage {
            username: "alice",
            event_type_code: "cl.comment.created",
            to: "alice@example.com",
            subject: "Subject",
            body_html: "<p>Body</p>",
            body_text: None,
            attachments: &attachments,
        };

        channel.deliver(&message).await.expect("first delivery");
        assert_eq!(first.calls.load(Ordering::SeqCst), 1);

        // Swap the underlying mailer at runtime via the shared handle.
        let second = Arc::new(RecordingMailer::default());
        handle.store(Arc::new(MailerSlot(second.clone())));

        channel.deliver(&message).await.expect("second delivery");
        assert_eq!(
            first.calls.load(Ordering::SeqCst),
            1,
            "old mailer unused after swap"
        );
        assert_eq!(
            second.calls.load(Ordering::SeqCst),
            1,
            "new mailer used after swap"
        );
    }
}
