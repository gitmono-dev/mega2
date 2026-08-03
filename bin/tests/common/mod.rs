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

/// Write the repo default `config/config.toml`, then append extra TOML.
///
/// Used by GM-03 to inject a per-case `[git] anonymous_access = false` block
/// without modifying the checked-in sample config (which has no `[git]`).
pub fn write_full_config_with_append(path: &Path, append: &str) {
    if append.is_empty() {
        write_full_config(path);
        return;
    }
    let mut body = include_str!("../../../config/config.toml").to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(append);
    if !append.ends_with('\n') {
        body.push('\n');
    }
    fs::write(path, body).expect("write full config");
}
