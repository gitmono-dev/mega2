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
pub mod chat;
pub use crate::callisto::*;
mod ceres;
mod cli;
mod commands;
mod common;
pub mod config;
mod context;
mod contract;
mod jupiter;
pub mod mail;
mod notification;
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

// Public entry points for the thin `monoengine` binary (composition root). The
// binary registers an `ObjectStorageProvider` (backed by the `orbit` impl
// crate) via `set_object_storage_provider`, then dispatches the CLI via `parse`.
pub use cli::parse;
pub use common::errors::MegaError;
pub use jupiter::storage::object_storage::{ObjectStorageProvider, set_object_storage_provider};
