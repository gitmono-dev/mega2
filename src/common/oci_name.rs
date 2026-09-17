//! Shared OCI distribution repository-name validation.
//!
//! The `/v2` router validates every inbound `<name>` path with this rule, and
//! `[storage_events].targets.*.oci_repositories` must reject anything the
//! router would never produce — a filter entry that cannot match any inbound
//! repository is a silently dead subscription. Both callers share this single
//! implementation (plan-20260912 / GC-02 / ADR-WH-01); do not re-derive the
//! regex elsewhere.

use std::sync::LazyLock;

use regex::Regex;

/// OCI repository path (`remoteName`): one or more `/`-separated path components.
/// Each component is `alphanumeric(?:(?:[._]|__|[-]+)alphanumeric)*`, matching
/// distribution `reference.pathComponent` / `remoteName` (without optional domain).
static REPOSITORY_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[a-z0-9]+(?:(?:[._]|__|[-]+)[a-z0-9]+)*(?:/[a-z0-9]+(?:(?:[._]|__|[-]+)[a-z0-9]+)*)*$",
    )
    .expect("repository name regex")
});

/// `true` when `name` is a canonical distribution `remoteName`. Case-sensitive:
/// the regex only admits lowercase, so `Team/Image` is rejected rather than
/// folded.
pub(crate) fn valid_repository_name(name: &str) -> bool {
    if name.is_empty() || name.split('/').any(|segment| segment == "..") {
        return false;
    }
    REPOSITORY_NAME.is_match(name)
}
