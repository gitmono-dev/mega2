//! Finite source scanner for the legacy Capture retirement (MTR-26 stage).
//!
//! Included only by `tests/integration_traces.rs` through an explicit
//! `#[path]` module, so other targets gain no dead code or `mega2_core`
//! dependency. The scanner is a small lexical check, not a parser: it reads a
//! fixed set of source trees and reports the retired HTTP-surface symbols that
//! are not on the stage allowlist.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// Symbols that must not survive the MTR-26 HTTP retirement.
const FORBIDDEN: [&str; 5] = [
    "agent_capture_router",
    "api_model::agent_capture",
    "AgentCaptureHttpError",
    "AGENT_CAPTURE_TAG",
    "\"/api/v1/agent-capture",
];

/// Files that may still mention those symbols because their removal belongs to
/// a later card, or because they are the retirement tests themselves.
const ALLOWLIST: [&str; 6] = [
    "tests/integration_agent_capture.rs",
    "tests/integration_traces.rs",
    "tests/common/traces_retirement_inventory.rs",
    "src/ceres/agent_capture/",
    "src/jupiter/service/agent_capture_service.rs",
    "src/jupiter/storage/agent_capture_storage.rs",
];

fn rust_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Returns every non-allowlisted hit as `path:line: symbol`.
pub fn scan_stage_26(repo_root: &Path) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    rust_files(&repo_root.join("src"), &mut files)?;
    rust_files(&repo_root.join("tests"), &mut files)?;
    files.sort();
    let mut hits = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(repo_root)
            .map_err(|error| error.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWLIST
            .iter()
            .any(|allowed| relative == *allowed || relative.starts_with(allowed))
        {
            continue;
        }
        let text = fs::read_to_string(&file).map_err(|error| format!("{relative}: {error}"))?;
        for (index, line) in text.lines().enumerate() {
            for symbol in FORBIDDEN {
                if line.contains(symbol) {
                    hits.push(format!("{relative}:{}: {symbol}", index + 1));
                }
            }
        }
    }
    Ok(hits)
}
