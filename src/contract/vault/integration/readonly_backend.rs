//! A physical backend that refuses to write.
//!
//! Readonly bootstrap mode (UN-31) works by not *asking* for writes: the mount
//! table is loaded rather than defaulted, the auth mount is not repaired, the
//! default ACL policies are not planted, and no worker that revokes leases is
//! started. This wrapper is the layer underneath all of that — the last line of
//! defence rather than the mechanism.
//!
//! The distinction matters when reasoning about an audit command's claim to have
//! changed nothing. Every code path above can be reviewed and can drift; this
//! one cannot be bypassed by a caller that forgot the rule, because there is no
//! path from `put`/`delete` to the wrapped backend at all. A denial is a hard
//! error, never a silent no-op: a swallowed write would leave the caller
//! believing its state was persisted.
//!
//! # Error identity
//!
//! This used to live inside the library, where it could invent an `RvError`
//! variant of its own for "readonly, write denied". The `libvault` crate has no
//! such variant and should not grow one — "this handle was opened readonly" is
//! mega2's concept, not the library's. So the two layers are
//! named separately:
//!
//! * above the backstop, the integration layer refuses with
//!   [`VaultError::ReadonlyWriteDenied`] before a write is ever attempted;
//! * at the backstop itself, the only channel back into the library is
//!   `RvError`, and the single variant this wrapper uses is
//!   [`RvError::ErrString`] carrying [`READONLY_WRITE_DENIED`] — the one
//!   upstream variant that carries a caller-facing reason and compares by
//!   equality, so a test can assert the exact refusal rather than "some error".

use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use libvault::{
    errors::RvError,
    storage::{Backend, BackendEntry},
};

use crate::common::errors::VaultError;

/// The reason the backstop gives when it refuses a write.
///
/// Exported so tests assert against the same string the wrapper produces
/// instead of a copy that can drift.
pub const READONLY_WRITE_DENIED: &str =
    "vault is open in readonly bootstrap mode; the write was denied";

/// The single `libvault` error variant the backstop maps onto.
pub fn readonly_write_denied() -> RvError {
    RvError::ErrString(READONLY_WRITE_DENIED.to_string())
}

/// Whether `error` is this wrapper's refusal.
pub fn is_readonly_write_denied(error: &RvError) -> bool {
    matches!(error, RvError::ErrString(message) if message == READONLY_WRITE_DENIED)
}

/// The named, mega2-side counterpart of [`readonly_write_denied`], for the
/// layer above the backstop.
pub fn readonly_write_denied_error() -> VaultError {
    VaultError::ReadonlyWriteDenied
}

pub struct ReadonlyBackend {
    inner: Arc<dyn Backend>,
    denied_writes: AtomicUsize,
}

impl ReadonlyBackend {
    pub fn new(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            denied_writes: AtomicUsize::new(0),
        }
    }

    /// How many writes this wrapper has refused.
    ///
    /// A non-zero count on a healthy readonly run means some path above still
    /// tried to write and was only stopped here — worth investigating even
    /// though nothing was persisted.
    pub fn denied_writes(&self) -> usize {
        self.denied_writes.load(Ordering::Relaxed)
    }

    fn deny(&self, operation: &'static str, key: &str) -> RvError {
        self.denied_writes.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            target: "vault_readonly",
            operation,
            key,
            "denied a write against a vault opened in readonly bootstrap mode"
        );
        readonly_write_denied()
    }
}

#[async_trait]
impl Backend for ReadonlyBackend {
    async fn list(&self, prefix: &str) -> Result<Vec<String>, RvError> {
        self.inner.list(prefix).await
    }

    async fn get(&self, key: &str) -> Result<Option<BackendEntry>, RvError> {
        self.inner.get(key).await
    }

    async fn put(&self, entry: &BackendEntry) -> Result<(), RvError> {
        Err(self.deny("put", &entry.key))
    }

    async fn delete(&self, key: &str) -> Result<(), RvError> {
        Err(self.deny("delete", key))
    }

    async fn lock(&self, lock_name: &str) -> Result<Box<dyn Any>, RvError> {
        self.inner.lock(lock_name).await
    }
}
