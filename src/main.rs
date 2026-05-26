#![allow(dead_code)]

mod api;
mod api_model;
mod bellatrix;
mod callisto;
pub use crate::callisto::*;
mod ceres;
mod cli;
mod commands;
mod common;
mod context;
mod email;
mod git_protocol;
mod io_orbit;
mod jupiter;
mod notification;
mod saturn;
mod server;
#[allow(hidden_glob_reexports)]
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
    }
}
