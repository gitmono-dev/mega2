//! Git smart-protocol HTTP path parser.
//!
//! Normalizes repository paths and identifies the three smart-protocol endpoints,
//! stripping only a trailing `.git` suffix (not `.git` segments embedded earlier
//! in the path).

use std::path::PathBuf;

use http::Method;

use crate::{ceres::protocol::ServiceType, common::errors::ProtocolError};

/// A recognized Git smart-protocol HTTP endpoint.
#[derive(Debug, PartialEq, Eq)]
pub enum GitProtocolEndpoint {
    /// `GET /$repo/info/refs?service=...`
    InfoRefs,
    /// `POST /$repo/git-upload-pack`
    UploadPack,
    /// `POST /$repo/git-receive-pack`
    ReceivePack,
}

impl GitProtocolEndpoint {
    /// Returns the service type used to drive protocol logic.
    pub fn service_type(&self) -> ServiceType {
        match self {
            GitProtocolEndpoint::InfoRefs => ServiceType::UploadPack,
            GitProtocolEndpoint::UploadPack => ServiceType::UploadPack,
            GitProtocolEndpoint::ReceivePack => ServiceType::ReceivePack,
        }
    }
}

/// Parsed Git smart-protocol HTTP route.
#[derive(Debug, PartialEq, Eq)]
pub struct GitProtocolPath {
    /// Repository path with a single trailing `.git` suffix removed.
    pub repo_path: PathBuf,
    /// Identified endpoint.
    pub endpoint: GitProtocolEndpoint,
}

/// Parses a Git smart-protocol HTTP request path and method.
///
/// Returns an error for unknown endpoints, unsupported methods, or the legacy
/// disallowed root repository `third-party.git`.
pub fn parse_git_protocol_path(
    method: &Method,
    full_path: &str,
) -> Result<GitProtocolPath, ProtocolError> {
    if is_disallowed_root_repo_path(full_path) {
        return Err(ProtocolError::InvalidInput(
            "Repository third-party.git is not supported".to_string(),
        ));
    }

    const INFO_REFS: &str = "/info/refs";
    const UPLOAD_PACK: &str = "/git-upload-pack";
    const RECEIVE_PACK: &str = "/git-receive-pack";

    if let Some(prefix) = full_path.strip_suffix(INFO_REFS) {
        if method != Method::GET {
            return Err(ProtocolError::InvalidInput(format!(
                "{INFO_REFS} only supports GET"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: strip_trailing_git_suffix(prefix),
            endpoint: GitProtocolEndpoint::InfoRefs,
        });
    }

    if let Some(prefix) = full_path.strip_suffix(UPLOAD_PACK) {
        if method != Method::POST {
            return Err(ProtocolError::InvalidInput(format!(
                "{UPLOAD_PACK} only supports POST"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: strip_trailing_git_suffix(prefix),
            endpoint: GitProtocolEndpoint::UploadPack,
        });
    }

    if let Some(prefix) = full_path.strip_suffix(RECEIVE_PACK) {
        if method != Method::POST {
            return Err(ProtocolError::InvalidInput(format!(
                "{RECEIVE_PACK} only supports POST"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: strip_trailing_git_suffix(prefix),
            endpoint: GitProtocolEndpoint::ReceivePack,
        });
    }

    Err(ProtocolError::InvalidInput(
        "Operation not supported".to_string(),
    ))
}

/// Strips a single trailing `.git` suffix from a path string.
fn strip_trailing_git_suffix(path: &str) -> PathBuf {
    let stripped = path
        .rsplit_once(".git")
        .filter(|(_, after)| after.is_empty())
        .map(|(before, _)| before)
        .unwrap_or(path);
    PathBuf::from(stripped)
}

/// The legacy `third-party.git` root repo is reserved and must not be served.
fn is_disallowed_root_repo_path(full_path: &str) -> bool {
    matches!(
        full_path.trim_start_matches('/').split('/').next(),
        Some("third-party.git")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_info_refs_requires_get() {
        let parsed = parse_git_protocol_path(&Method::GET, "/project.git/info/refs").unwrap();
        assert_eq!(parsed.repo_path, PathBuf::from("/project"));
        assert_eq!(parsed.endpoint, GitProtocolEndpoint::InfoRefs);
    }

    #[test]
    fn parse_info_refs_rejects_post() {
        let err = parse_git_protocol_path(&Method::POST, "/project.git/info/refs").unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidInput(_)));
        assert!(err.to_string().contains("only supports GET"));
    }

    #[test]
    fn parse_upload_pack_requires_post() {
        let parsed =
            parse_git_protocol_path(&Method::POST, "/project.git/git-upload-pack").unwrap();
        assert_eq!(parsed.repo_path, PathBuf::from("/project"));
        assert_eq!(parsed.endpoint, GitProtocolEndpoint::UploadPack);
    }

    #[test]
    fn parse_receive_pack_requires_post() {
        let parsed =
            parse_git_protocol_path(&Method::POST, "/project.git/git-receive-pack").unwrap();
        assert_eq!(parsed.repo_path, PathBuf::from("/project"));
        assert_eq!(parsed.endpoint, GitProtocolEndpoint::ReceivePack);
    }

    #[test]
    fn parse_preserves_embedded_git_segment() {
        let parsed = parse_git_protocol_path(&Method::GET, "/foo.git/bar.git/info/refs").unwrap();
        assert_eq!(parsed.repo_path, PathBuf::from("/foo.git/bar"));
    }

    #[test]
    fn parse_rejects_disallowed_root_repo() {
        let err = parse_git_protocol_path(&Method::GET, "/third-party.git/info/refs").unwrap_err();
        assert!(err.to_string().contains("third-party.git is not supported"));
    }

    #[test]
    fn parse_rejects_unknown_endpoint() {
        let err = parse_git_protocol_path(&Method::GET, "/project.git/git-status").unwrap_err();
        assert!(err.to_string().contains("Operation not supported"));
    }

    #[test]
    fn strip_trailing_git_suffix_leaves_non_trailing_segment() {
        assert_eq!(
            strip_trailing_git_suffix("/foo.git/bar.git"),
            PathBuf::from("/foo.git/bar")
        );
        assert_eq!(
            strip_trailing_git_suffix("/foo/bar.git"),
            PathBuf::from("/foo/bar")
        );
        assert_eq!(
            strip_trailing_git_suffix("/foo/bar"),
            PathBuf::from("/foo/bar")
        );
    }
}
