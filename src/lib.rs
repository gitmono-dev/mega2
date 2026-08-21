#![allow(dead_code)]

//! `monoengine-core` — the monoengine library.
//!
//! This crate contains all of monoengine's logic and depends only on the
//! `orbit-api` contract crate for object storage (traits + config + wrapper).
//! The concrete object-storage implementation (the heavy `orbit` crate that
//! pulls `object_store` + cloud SDKs) is injected by the thin `monoengine`
//! binary via [`set_object_storage_provider`] at startup. See
//! `docs/refactoring/orbit.md`.

mod api;
mod bellatrix;
mod callisto;
pub use crate::callisto::*;
mod ceres;
mod cli;
mod commands;
mod common;
pub mod config;
mod context;
mod contract;
mod jupiter;
pub mod notification;
mod server;

/// Internal seam for the read-only ops assembly (UN-30).
///
/// The zero-side-effect proof drives the assembly from the bin IT target
/// without going through the CLI. `authz-audit` (UN-29) is the supported
/// operator surface; this module stays for the black-box before/after proof.
#[doc(hidden)]
pub mod readonly_ops {
    pub use crate::{
        commands::{LoadedConfigPaths, LoadedConfigSummary},
        context::ReadOnlyContext,
        jupiter::storage::ReadOnlyStorage,
    };
}

/// Internal seam for IT bridging promote (UN-35) until `authz-audit promote`
/// lands in UN-37. Not a supported API.
#[doc(hidden)]
pub mod authz_audit_ops {
    pub use crate::contract::policy::{
        baseline_promotion::{PromoteFence, PromoteRequest, content_digest, promote},
        secure_artifact::RestrictedRoot,
    };
}

// Public entry points for the thin `monoengine` binary (composition root). The
// binary registers an `ObjectStorageProvider` (backed by the `orbit` impl
// crate) via `set_object_storage_provider`, then dispatches the CLI via `parse`.
pub use cli::parse;
pub use common::errors::MegaError;
pub use jupiter::storage::object_storage::{ObjectStorageProvider, set_object_storage_provider};
