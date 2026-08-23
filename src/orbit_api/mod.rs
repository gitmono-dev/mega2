//! `orbit-api` — the stable interface for orbit object and log storage.
//!
//! This crate defines the traits, config, error, and key types that consumers
//! depend on. It is intentionally dependency-light (no tokio/tracing; no
//! `object_store` cloud features) so interface-only consumers avoid the heavy
//! implementation stack in the `orbit` crate.
//!
//! ## Stability / compatibility
//!
//! The following are treated as **stable** and only change via a documented,
//! versioned migration (see `docs/compatibility.md`):
//!
//! - Public trait method signatures ([`MegaObjectStorage`], [`LogStorage`]).
//! - [`ObjectKey::default_sharding`] output — the on-disk object path format.
//! - [`ObjectNamespace`] variant string values
//!   (`git`/`lfs`/`log`/`artifact`/`attachment`).
//! - Serialized manifest layout: new [`LogManifest`]/[`LogSegmentMeta`] fields
//!   must be backward-compatible (`#[serde(default)]`).
//!
//! New capabilities are added via new methods, fields, or config — not by
//! changing the items above.
//!
//! [`MegaObjectStorage`]: object_storage::MegaObjectStorage
//! [`LogStorage`]: log_storage::LogStorage
//! [`ObjectKey::default_sharding`]: object_storage::ObjectKey::default_sharding
//! [`ObjectNamespace`]: object_storage::ObjectNamespace
//! [`LogManifest`]: log_storage::LogManifest
//! [`LogSegmentMeta`]: log_storage::LogSegmentMeta

pub mod error;
pub mod factory;
pub mod log_storage;
pub mod object_storage;
