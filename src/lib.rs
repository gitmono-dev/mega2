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
// The vendored RustyVault code intentionally keeps its upstream style and
// clippy policy while being compiled as a monoengine module.
#[allow(
    hidden_glob_reexports,
    clippy::await_holding_lock,
    clippy::collapsible_match,
    clippy::field_reassign_with_default,
    clippy::large_enum_variant,
    clippy::let_and_return,
    clippy::new_without_default,
    clippy::ptr_arg,
    clippy::result_large_err,
    clippy::should_implement_trait,
    clippy::too_many_arguments,
    clippy::unnecessary_map_or,
    clippy::upper_case_acronyms,
    clippy::wrong_self_convention,
    unused_imports
)]
mod vault;

/// Internal seam for the read-only ops assembly (UN-30).
///
/// The zero-side-effect proof is a black-box before/after comparison: seed a
/// real database through the real binary, snapshot it, run the read-only
/// assembly, snapshot again. Driving that assembly needs a way in, and the
/// command that will be its real entry point does not exist yet (UN-29).
///
/// Not a supported API. It is `#[doc(hidden)]` and exists so the proof can be
/// written against the real assembly rather than a stand-in; once the audit
/// command lands, the command is the black-box surface and this can go.
#[doc(hidden)]
pub mod readonly_ops {
    pub use crate::{
        commands::{LoadedConfigPaths, LoadedConfigSummary},
        context::ReadOnlyContext,
        jupiter::storage::ReadOnlyStorage,
    };
}

// Public entry points for the thin `monoengine` binary (composition root). The
// binary registers an `ObjectStorageProvider` (backed by the `orbit` impl
// crate) via `set_object_storage_provider`, then dispatches the CLI via `parse`.
pub use cli::parse;
pub use common::errors::MegaError;
pub use jupiter::storage::object_storage::{ObjectStorageProvider, set_object_storage_provider};
