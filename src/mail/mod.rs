//! Top-level `mail` module (一级 mail 模块).
//!
//! Provides the `Mailer` trait and implementations for sending system notification
//! emails. The primary implementation is SMTP via `lettre`.
//!
//! This module is designed as a first-class citizen (parallel to the planned
//! top-level `config` module) so that:
//! - Mail configuration participates in the unified Config pipeline.
//! - The mailer can be constructed **after** VaultCore is ready (prerequisite
//!   for using `SecretRef` on `password` per docs/config.md phase 5).
//! - Notification outbox (email_jobs + dispatcher) can obtain a real mailer
//!   without pulling secrets into early bootstrap paths (Storage::new, redis init, etc.).
//!
//! Current status (see docs/mail.md for the full analysis and phased plan):
//! - MailConfig lives in `crate::config` (will co-evolve with the config
//!   module split).
//! - SMTP, console and Noop are implemented.
//! - The background EmailDispatcher (in `crate::notification`) processes the
//!   `email_jobs` outbox table using a `NotificationStorage`.
//! - Triggers (e.g. on_cl_comment_created) enqueue jobs respecting user
//!   notification preferences and event types (ported from mega lineage).
//!
//! Security notes:
//! - `password` (and future `password_ref`) must only be read/used after vault
//!   is initialized.
//! - Never log the password or resolved secret values.
//! - Construction of SmtpMailer with real credentials must happen in post-vault
//!   startup paths.

use std::sync::Arc;

use async_trait::async_trait;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{MultiPart, SinglePart, header::ContentType},
    transport::smtp::authentication::Credentials,
};

use crate::{
    common::errors::MegaError,
    config::{MailConfig, MailProvider},
};

pub mod template;

#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send_html(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        text: Option<&str>,
    ) -> Result<(), MegaError>;
}

pub struct NoopMailer;

#[async_trait]
impl Mailer for NoopMailer {
    async fn send_html(
        &self,
        _to: &str,
        _subject: &str,
        _html: &str,
        _text: Option<&str>,
    ) -> Result<(), MegaError> {
        Ok(())
    }
}

pub struct ConsoleMailer {
    enabled: bool,
    from: String,
}

impl ConsoleMailer {
    pub fn new(cfg: &MailConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            from: cfg.from.clone(),
        }
    }
}

#[async_trait]
impl Mailer for ConsoleMailer {
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

        tracing::info!(
            from = %self.from,
            to = %to,
            subject = %subject,
            html_len = html.len(),
            text_len = text.map(str::len).unwrap_or_default(),
            "console mailer accepted email"
        );

        Ok(())
    }
}

pub struct SmtpMailer {
    enabled: bool,
    from: String,
    transport: Option<AsyncSmtpTransport<Tokio1Executor>>,
}

impl SmtpMailer {
    /// Construct from MailConfig.
    ///
    /// Callers **must** ensure this is invoked only after VaultCore (and any
    /// future SecretRef resolver) is available. See docs/config.md "secret 解析的依赖顺序"
    /// and docs/mail.md for the bootstrap rules and migration to `password_ref`.
    pub fn new(cfg: &MailConfig) -> Result<Self, MegaError> {
        let password = cfg
            .password
            .as_ref()
            .map(|password| password.expose_secret().to_string());
        Self::new_with_password(cfg, password)
    }

    pub fn new_with_password(
        cfg: &MailConfig,
        resolved_password: Option<String>,
    ) -> Result<Self, MegaError> {
        cfg.validate_secret_fields()?;

        if !cfg.enabled {
            return Ok(Self {
                enabled: false,
                from: cfg.from.clone(),
                transport: None,
            });
        }

        let mut builder = if cfg.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)
                .map_err(|e| MegaError::Other(format!("smtp starttls relay error: {e}")))?
                .port(cfg.smtp_port)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.smtp_host)
                .port(cfg.smtp_port)
        };

        if let (Some(user), Some(pass)) = (&cfg.username, &resolved_password) {
            builder = builder.credentials(Credentials::new(user.clone(), pass.clone()));
        }

        Ok(Self {
            enabled: true,
            from: cfg.from.clone(),
            transport: Some(builder.build()),
        })
    }

    fn build_message(
        &self,
        to: &str,
        subject: &str,
        html: &str,
        text: Option<&str>,
    ) -> Result<Message, MegaError> {
        let from = self
            .from
            .parse()
            .map_err(|e| MegaError::Other(format!("invalid from email: {e}")))?;
        let to = to
            .parse()
            .map_err(|e| MegaError::Other(format!("invalid to email: {e}")))?;

        let html_part = SinglePart::builder()
            .header(ContentType::TEXT_HTML)
            .body(html.to_string());

        let multipart = if let Some(text) = text {
            let text_part = SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(text.to_string());
            MultiPart::alternative()
                .singlepart(text_part)
                .singlepart(html_part)
        } else {
            MultiPart::alternative().singlepart(html_part)
        };

        Message::builder()
            .from(from)
            .to(to)
            .subject(subject)
            .multipart(multipart)
            .map_err(|e| MegaError::Other(format!("build email message error: {e}")))
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
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
        let transport = self
            .transport
            .as_ref()
            .ok_or_else(|| MegaError::Other("smtp transport missing while enabled".to_string()))?;

        let msg = self.build_message(to, subject, html, text)?;
        transport
            .send(msg)
            .await
            .map(|_| ())
            .map_err(|e| MegaError::Other(format!("smtp send error: {e}")))
    }
}

pub fn mailer_from_config(
    cfg: &MailConfig,
    resolved_password: Option<String>,
) -> Result<Arc<dyn Mailer>, MegaError> {
    cfg.validate_secret_fields()?;

    match cfg.provider {
        MailProvider::Smtp => {
            let password = if cfg.password_ref.is_some() {
                Some(resolved_password.ok_or_else(|| {
                    MegaError::Other(
                        "mail.password_ref must be resolved before constructing smtp mailer"
                            .to_string(),
                    )
                })?)
            } else {
                cfg.password
                    .as_ref()
                    .map(|password| password.expose_secret().to_string())
            };

            Ok(Arc::new(SmtpMailer::new_with_password(cfg, password)?))
        }
        MailProvider::Console => Ok(Arc::new(ConsoleMailer::new(cfg))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MailConfig, MailProvider};

    #[test]
    fn test_smtp_mailer_disabled_is_noop() {
        let cfg = MailConfig {
            enabled: false,
            provider: MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
        };

        let mailer = SmtpMailer::new(&cfg).expect("create mailer");
        assert!(!mailer.enabled);
    }

    #[test]
    fn test_build_message_validates_addresses() {
        let cfg = MailConfig {
            enabled: false,
            provider: MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
        };
        let mailer = SmtpMailer::new(&cfg).unwrap();

        let msg = mailer
            .build_message("user@example.com", "Subj", "<p>Hi</p>", Some("Hi"))
            .expect("message should build");

        let raw = msg.formatted();
        assert!(!raw.is_empty());
    }

    #[test]
    fn test_build_message_rejects_bad_to() {
        let cfg = MailConfig {
            enabled: false,
            provider: MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: "no-reply@example.com".to_string(),
            starttls: true,
        };
        let mailer = SmtpMailer::new(&cfg).unwrap();
        let err = mailer
            .build_message("not-an-email", "Subj", "<p>Hi</p>", None)
            .expect_err("should fail");
        let _ = format!("{err:?}");
    }

    #[test]
    fn test_mailer_from_config_builds_console_provider_without_smtp_fields() {
        let cfg = MailConfig {
            enabled: true,
            provider: MailProvider::Console,
            smtp_host: String::new(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: None,
            from: String::new(),
            starttls: true,
        };

        mailer_from_config(&cfg, None).expect("console mailer should build");
    }
}
