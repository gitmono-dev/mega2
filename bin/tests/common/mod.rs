// Shared black-box test config generation (integration.md Phase 0).
//
// These helpers only emit TOML / copy the repo default config; they do NOT
// import the `monoengine-core` crate, so black-box tests that drive the real
// CLI binary via `CARGO_BIN_EXE_monoengine` stay decoupled from the library.
//
// Git-cli runner/credential helpers live in `common/git_cli.rs` and are
// included only by `integration_git_cli` (via `#[path]`), so
// `integration_vault` does not compile unused git symbols into its target.

use std::{fs, path::Path};

pub fn write_full_config(path: &Path) {
    fs::write(path, include_str!("../../../config/config.toml")).expect("write full config");
}
