//! Compatibility shim (historical `src/email` location).
//!
//! The real implementation is the top-level `mail` module (`src/mail/mod.rs`).
//! All new code and the notification layer import from `crate::mail`.
//!
//! This shim only re-exports so that any stray historical references continue
//! to compile during transition. Do not add new logic here.
//!
//! See docs/mail.md for the full design of the一级 mail 模块, its integration
//! with Config + future SecretRef (password), the notification outbox, and
//! the phased plan.

pub use crate::mail::{MailAttachment, Mailer, NoopMailer, SmtpMailer};
