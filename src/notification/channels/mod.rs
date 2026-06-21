//! Channel abstraction for the notification subsystem.
//!
//! [`NotificationChannel`] decouples the delivery mechanism (email today; in-app
//! inbox, and slack / webhook in future) from the outbox-driven dispatcher.
//! The `email_jobs` outbox routes to [`EmailChannel`] (primary, retried); the
//! [`NotificationService`](crate::notification::service::NotificationService)
//! coordinator registers secondary channels (e.g. [`InAppChannel`]) that the
//! dispatcher fans each delivered notification out to.
//!
//! This is docs/notification.md phase 1/4: a provider-agnostic channel trait
//! plus a non-email channel ([`InAppChannel`]) proving the abstraction is not
//! email-specific.

use async_trait::async_trait;

use crate::{common::errors::MegaError, mail::MailAttachment};

mod email;
mod inapp;

pub use email::{EmailChannel, MailerHandle, MailerSlot};
pub use inapp::InAppChannel;

/// Stable identifier for the SMTP/console-mailer-backed email channel.
pub const CHANNEL_EMAIL: &str = "email";
/// Stable identifier for the in-app (inbox) channel.
pub const CHANNEL_IN_APP: &str = "in_app";

/// A channel-agnostic outbound notification message.
///
/// Fields borrow from the originating outbox job so delivery does not re-clone
/// the body/attachments per channel. `username` and `event_type_code` let
/// non-email channels (e.g. in-app inbox) address and categorize the
/// notification; the email channel ignores them.
pub struct OutboundMessage<'a> {
    pub username: &'a str,
    pub event_type_code: &'a str,
    pub to: &'a str,
    pub subject: &'a str,
    pub body_html: &'a str,
    pub body_text: Option<&'a str>,
    pub attachments: &'a [MailAttachment],
}

/// A delivery channel for user notifications.
///
/// Implementations are held behind an `Arc` and called concurrently: the
/// dispatcher fans out bounded-parallel deliveries. A failed `deliver` is
/// retried / dead-lettered by the dispatcher according to the configured retry
/// policy, so implementations should return a diagnostic error rather than
/// swallowing failures — and must never leak credentials or full recipient
/// addresses in the returned error.
#[async_trait]
pub trait NotificationChannel: Send + Sync {
    /// Stable channel identifier used for routing and structured diagnostics.
    fn name(&self) -> &'static str;

    /// Deliver a single outbound message.
    async fn deliver(&self, message: &OutboundMessage<'_>) -> Result<(), MegaError>;
}
