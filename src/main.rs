#![allow(dead_code)]

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
mod mail;
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

use crate::cli::parse;

#[cfg(not(target_os = "windows"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[cfg(target_os = "windows")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let result = parse(None);

    // If there was an error, print it
    if let Err(e) = result {
        e.print();
        std::process::exit(1);
    }
}
