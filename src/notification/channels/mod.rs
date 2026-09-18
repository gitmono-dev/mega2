//! Channel abstraction for the notification subsystem.
//!
//! [`NotificationChannel`] decouples console, Slack, and webhook delivery from
//! event triggers.

use async_trait::async_trait;

use crate::common::errors::MegaError;

mod console;
mod slack;
mod webhook;

pub use console::ConsoleChannel;
pub use slack::SlackChannel;
pub use webhook::WebhookChannel;

/// Stable identifier for the console (dry-run) channel.
pub const CHANNEL_CONSOLE: &str = "console";
/// Stable identifier for the Slack incoming-webhook channel.
pub const CHANNEL_SLACK: &str = "slack";
/// Stable identifier for the generic outbound webhook channel.
pub const CHANNEL_WEBHOOK: &str = "webhook";

/// A channel-agnostic outbound notification message.
///
/// `username` and `event_type_code` let channels address and categorize the
/// notification without using an email outbox.
pub struct OutboundMessage<'a> {
    pub username: &'a str,
    pub event_type_code: &'a str,
    pub to: &'a str,
    pub subject: &'a str,
    pub body_html: &'a str,
    pub body_text: Option<&'a str>,
}

/// A delivery channel for user notifications.
///
/// Implementations are held behind an `Arc`. They must return diagnostics
/// without leaking credentials or full recipient addresses.
#[async_trait]
pub trait NotificationChannel: Send + Sync {
    /// Stable channel identifier used for routing and structured diagnostics.
    fn name(&self) -> &'static str;

    /// Deliver a single outbound message.
    async fn deliver(&self, message: &OutboundMessage<'_>) -> Result<(), MegaError>;
}
