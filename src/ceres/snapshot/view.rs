//! Fixed namespace views (spec 02 §2, 03 §1).

use sha2::{Digest, Sha256};

use crate::ceres::snapshot::error::SnapshotError;

/// A fixed view over one monorepo commit. `view_id` is a labelled digest
/// (`mega.mst2.namespaceview` domain): it pins (commit) identity, not a full
/// NamespaceView encoding — the binding/release composition of spec 02 arrives
/// with T04/T05 and may extend what the digest covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotView {
    pub view_id: String,
    pub commit_oid: String,
    pub root_tree_oid: String,
}

impl SnapshotView {
    pub fn from_commit(commit_oid: &str, root_tree_oid: &str) -> Self {
        let mut h = Sha256::new();
        h.update(b"mega.mst2.namespaceview\0");
        h.update(commit_oid.as_bytes());
        let view_id = format!("sha256:{}", hex(&h.finalize()));
        SnapshotView {
            view_id,
            commit_oid: commit_oid.to_string(),
            root_tree_oid: root_tree_oid.to_string(),
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Validate a scope-relative request path per spec 04 §1: absolute shape
/// relative to the descriptor scope, exactly one URL decode, no dot
/// components, no NUL, no trailing slash except the root itself, UTF-8 only.
pub fn validate_scope_relative_path(path: &str) -> Result<(), SnapshotError> {
    let invalid = |msg: &str| {
        SnapshotError::new(
            crate::ceres::snapshot::error::SnapshotErrorCode::ScopeInvalid,
            msg,
        )
    };
    if path.is_empty() || !path.starts_with('/') {
        return Err(invalid("path must start with '/'"));
    }
    if path.len() > 4096 {
        return Err(invalid("path longer than 4096 bytes"));
    }
    if path != "/" && path.ends_with('/') {
        return Err(invalid("trailing slash"));
    }
    if path.contains('\0') {
        return Err(invalid("NUL in path"));
    }
    if path == "/" {
        return Ok(());
    }
    let mut component_count = 0;
    for comp in path[1..].split('/') {
        component_count += 1;
        if component_count > 256 {
            return Err(invalid("path has more than 256 components"));
        }
        if comp.is_empty() {
            return Err(invalid("empty path component"));
        }
        if comp == "." || comp == ".." {
            return Err(invalid("dot component"));
        }
        if comp.len() > 255 {
            return Err(invalid("path component over 255 bytes"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_id_is_domain_separated_and_stable() {
        let v1 = SnapshotView::from_commit("abc", "def");
        let v2 = SnapshotView::from_commit("abc", "def");
        assert_eq!(v1.view_id, v2.view_id);
        assert!(v1.view_id.starts_with("sha256:"));
        assert_ne!(v1.view_id, SnapshotView::from_commit("abd", "def").view_id);
    }

    #[test]
    fn scope_relative_path_rules() {
        assert!(validate_scope_relative_path("/").is_ok());
        assert!(validate_scope_relative_path("/src/a.rs").is_ok());
        assert!(validate_scope_relative_path("src/a.rs").is_err());
        assert!(validate_scope_relative_path("/a/..").is_err());
        assert!(validate_scope_relative_path("/a//b").is_err());
        assert!(validate_scope_relative_path("/a/").is_err());
        assert!(validate_scope_relative_path("/a\0").is_err());
    }

    #[test]
    fn scope_relative_path_enforces_byte_and_component_boundaries() {
        assert!(validate_scope_relative_path(&format!("/{}", "a".repeat(255))).is_ok());
        assert!(validate_scope_relative_path(&format!("/{}", "a".repeat(256))).is_err());
        assert!(validate_scope_relative_path(&format!("/{}a", "é".repeat(127))).is_ok());
        assert!(validate_scope_relative_path(&format!("/{}", "é".repeat(128))).is_err());
        assert!(validate_scope_relative_path(&format!("/{}", vec!["a"; 256].join("/"))).is_ok());
        assert!(validate_scope_relative_path(&format!("/{}", vec!["a"; 257].join("/"))).is_err());
    }
}
