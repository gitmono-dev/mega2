//! Test helpers for the `mail` module.
//!
//! Provides a capturing `MockMailer` that records every email it would have
//! sent. This is intended for unit/integration tests that need to assert on
//! mailer behavior without relying on external SMTP or Mailpit.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{MailAttachment, Mailer};
use crate::common::errors::MegaError;

/// A single captured email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEmail {
    pub to: String,
    pub subject: String,
    pub html: String,
    pub text: Option<String>,
    pub attachments: Vec<MailAttachment>,
}

/// In-memory mailer that records emails for inspection in tests.
#[derive(Clone)]
pub struct MockMailer {
    enabled: bool,
    mailbox: Arc<Mutex<Vec<CapturedEmail>>>,
}

impl MockMailer {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            mailbox: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns a clone of all captured emails without clearing the mailbox.
    pub fn sent(&self) -> Vec<CapturedEmail> {
        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Returns all captured emails and clears the mailbox.
    pub fn take_sent(&self) -> Vec<CapturedEmail> {
        std::mem::take(&mut self.mailbox.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

impl Default for MockMailer {
    fn default() -> Self {
        Self::new(true)
    }
}

#[async_trait]
impl Mailer for MockMailer {
    async fn send_html(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        text: Option<&str>,
    ) -> Result<(), MegaError> {
        if !self.enabled {
            return Ok(());
        }

        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(CapturedEmail {
                to: to.to_owned(),
                subject: subject.to_owned(),
                html: html.to_owned(),
                text: text.map(str::to_owned),
                attachments: Vec::new(),
            });
        Ok(())
    }

    async fn send_html_with_attachments(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        text: Option<&str>,
        attachments: &[MailAttachment],
    ) -> Result<(), MegaError> {
        if !self.enabled {
            return Ok(());
        }

        self.mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(CapturedEmail {
                to: to.to_owned(),
                subject: subject.to_owned(),
                html: html.to_owned(),
                text: text.map(str::to_owned),
                attachments: attachments.to_vec(),
            });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_mailer_captures_html_email() {
        let mailer = MockMailer::new(true);
        mailer
            .send_html("alice@example.com", "Hello", "<p>hi</p>", Some("hi"))
            .await
            .unwrap();

        let sent = mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, "alice@example.com");
        assert_eq!(sent[0].subject, "Hello");
        assert_eq!(sent[0].html, "<p>hi</p>");
        assert_eq!(sent[0].text, Some("hi".to_string()));
    }

    #[tokio::test]
    async fn mock_mailer_captures_email_with_attachments() {
        let mailer = MockMailer::new(true);
        let attachment = MailAttachment::new("file.txt", "text/plain", b"content");
        mailer
            .send_html_with_attachments(
                "bob@example.com",
                "Report",
                "<p>report</p>",
                None,
                std::slice::from_ref(&attachment),
            )
            .await
            .unwrap();

        let sent = mailer.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].attachments.len(), 1);
        assert_eq!(sent[0].attachments[0], attachment);
    }

    #[tokio::test]
    async fn disabled_mock_mailer_does_not_capture() {
        let mailer = MockMailer::new(false);
        mailer
            .send_html("carol@example.com", "Subject", "body", None)
            .await
            .unwrap();

        assert!(mailer.sent().is_empty());
    }
}
