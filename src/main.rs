//! `mega2` binary entry point (composition root).
//!
//! Installs the global allocator and dispatches the CLI through [`parse`] on
//! the `mega2_core` library crate in this package.

use mega2_core::parse;

#[cfg(not(target_os = "windows"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[cfg(target_os = "windows")]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let result = parse(None);

    if let Err(e) = result {
        e.print();
        std::process::exit(e.process_exit_code());
    }
}
