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

/// Stage 26: symbols that must not survive the legacy HTTP retirement.
const STAGE_26_FORBIDDEN: [&str; 5] = [
    "agent_capture_router",
    "api_model::agent_capture",
    "AgentCaptureHttpError",
    "AGENT_CAPTURE_TAG",
    "\"/api/v1/agent-capture",
];

/// Stage 26 allowlist: only the retirement tests may still name the old paths.
const STAGE_26_ALLOWLIST: [&str; 3] = [
    "tests/integration_agent_capture.rs",
    "tests/integration_traces.rs",
    "tests/common/traces_retirement_inventory.rs",
];

/// Stage 28: the removed internal runtime modules and types.
const STAGE_28_FORBIDDEN: [&str; 7] = [
    "agent_capture_service",
    "AgentCaptureService",
    "agent_capture_storage",
    "AgentCaptureStorage",
    "ceres::agent_capture",
    "lookup_ingest_token",
    "token_covers_capture_repo",
];

/// Stage 31: the removed legacy entity modules and re-exports.
const STAGE_31_FORBIDDEN: [&str; 3] = [
    "pub mod agent_capture_",
    "callisto::agent_capture_",
    "agent_capture_session::Entity",
];

/// Stage 31 allowlist: the scanner itself and the removal assertions.
const STAGE_31_ALLOWLIST: [&str; 2] = [
    "tests/common/traces_retirement_inventory.rs",
    "tests/integration_traces.rs",
];

/// Stage 35: the removed legacy object namespace.
const STAGE_35_FORBIDDEN: [&str; 2] = ["ObjectNamespace::Agent", "ObjectNamespace :: Agent"];

/// Stage 35 allowlist: nothing may still name the removed namespace.
const STAGE_35_ALLOWLIST: [&str; 2] = [
    "tests/common/traces_retirement_inventory.rs",
    "tests/integration_traces.rs",
];

/// Stage 27: the removed legacy config model and section. The storage-event
/// filter names and Capture event kinds stay with MTR-57.
const STAGE_27_FORBIDDEN: [&str; 4] = [
    "AgentCaptureConfig",
    "AgentCaptureIngestTokenConfig",
    "agent_capture.ingest_tokens",
    "MEGA_AGENT_CAPTURE__",
];

/// Stage 57: the removed Capture event kinds and target filters.
const STAGE_57_FORBIDDEN: [&str; 6] = [
    "agent_tenants",
    "agent_repo_paths",
    "agent_capture.events.committed",
    "agent_capture.checkpoint.committed",
    "AgentCaptureEventsCommitted",
    "AgentCaptureCheckpointCommitted",
];

/// Stage 57 allowlist: the retirement assertions that name the removed
/// symbols to prove they no longer resolve.
const STAGE_57_ALLOWLIST: [&str; 3] = [
    "src/jupiter/service/storage_event.rs",
    "tests/common/traces_retirement_inventory.rs",
    "tests/integration_traces.rs",
];

/// Stage 27 allowlist: the removal guard names what it rejects, and the
/// retirement tests prove the names are gone.
const STAGE_27_ALLOWLIST: [&str; 4] = [
    "src/config/validate.rs",
    "tests/common/traces_retirement_inventory.rs",
    "tests/integration_traces.rs",
    "tests/integration_agent_capture.rs",
];

/// Stage 28 allowlist: the scanner itself and the retirement assertions that
/// name the removed paths to prove they are gone.
const STAGE_28_ALLOWLIST: [&str; 2] = [
    "tests/common/traces_retirement_inventory.rs",
    "tests/integration_traces.rs",
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
fn scan(repo_root: &Path, forbidden: &[&str], allowlist: &[&str]) -> Result<Vec<String>, String> {
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
        if allowlist
            .iter()
            .any(|allowed| relative == *allowed || relative.starts_with(allowed))
        {
            continue;
        }
        let text = fs::read_to_string(&file).map_err(|error| format!("{relative}: {error}"))?;
        for (index, line) in text.lines().enumerate() {
            for symbol in forbidden {
                if line.contains(symbol) {
                    hits.push(format!("{relative}:{}: {symbol}", index + 1));
                }
            }
        }
    }
    Ok(hits)
}

/// Stage 26 scan (legacy HTTP surface).
pub fn scan_stage_26(repo_root: &Path) -> Result<Vec<String>, String> {
    scan(repo_root, &STAGE_26_FORBIDDEN, &STAGE_26_ALLOWLIST)
}

/// Stage 28 scan (legacy internal runtime).
pub fn scan_stage_28(repo_root: &Path) -> Result<Vec<String>, String> {
    scan(repo_root, &STAGE_28_FORBIDDEN, &STAGE_28_ALLOWLIST)
}

/// Stage 27 scan (removed legacy config and event names).
pub fn scan_stage_27(repo_root: &Path) -> Result<Vec<String>, String> {
    scan(repo_root, &STAGE_27_FORBIDDEN, &STAGE_27_ALLOWLIST)
}

/// Stage 57 scan (removed Capture events and target filters).
pub fn scan_stage_57(repo_root: &Path) -> Result<Vec<String>, String> {
    scan(repo_root, &STAGE_57_FORBIDDEN, &STAGE_57_ALLOWLIST)
}

/// Stage 35 scan (removed legacy object namespace).
pub fn scan_stage_35(repo_root: &Path) -> Result<Vec<String>, String> {
    scan(repo_root, &STAGE_35_FORBIDDEN, &STAGE_35_ALLOWLIST)
}

/// Stage 31 scan (removed legacy entity modules).
pub fn scan_stage_31(repo_root: &Path) -> Result<Vec<String>, String> {
    scan(repo_root, &STAGE_31_FORBIDDEN, &STAGE_31_ALLOWLIST)
}
