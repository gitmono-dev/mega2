//! Git smart-protocol HTTP path parser.
//!
//! Normalizes repository paths and identifies the three smart-protocol endpoints,
//! stripping only a trailing `.git` suffix (not `.git` segments embedded earlier
//! in the path).

use std::path::PathBuf;

use http::Method;

use crate::{
    ceres::{
        protocol::ServiceType,
        view::{VIEW_URL_RESERVED_NAMES, name::validate_view_name},
    },
    common::errors::ProtocolError,
};

#[derive(Debug, PartialEq, Eq)]
pub enum RepoLocator {
    Path(PathBuf),
    View(ViewLocator),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ViewLocator {
    Named { name: String, version: Option<u32> },
    FilterId(String),
}

pub fn classify_repo_locator(raw: &str) -> Result<RepoLocator, ProtocolError> {
    let stripped = raw.strip_suffix(".git").unwrap_or(raw);
    let relative = stripped.trim_start_matches('/');
    let (first, rest) = relative.split_once('/').unwrap_or((relative, ""));
    if !VIEW_URL_RESERVED_NAMES.contains(&first) {
        return Ok(RepoLocator::Path(normalize_repo_path(raw)));
    }
    let not_found = || ProtocolError::NotFound("view not found".to_owned());
    if first == ".filter" {
        if rest.len() != 64
            || !rest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(not_found());
        }
        return Ok(RepoLocator::View(ViewLocator::FilterId(rest.to_owned())));
    }
    let (name, version) = if let Some((name, raw_version)) = rest.rsplit_once('@') {
        if raw_version.is_empty() || !raw_version.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(not_found());
        }
        let version = raw_version.parse::<u32>().map_err(|_| not_found())?;
        if version == 0 {
            return Err(not_found());
        }
        (name, Some(version))
    } else {
        (rest, None)
    };
    validate_view_name(name).map_err(|_| not_found())?;
    Ok(RepoLocator::View(ViewLocator::Named {
        name: name.to_owned(),
        version,
    }))
}

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
    pub locator: RepoLocator,
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
        let locator = classify_repo_locator(prefix)?;
        if method != Method::GET {
            return Err(ProtocolError::InvalidInput(format!(
                "{INFO_REFS} only supports GET"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: normalize_repo_path(prefix),
            locator,
            endpoint: GitProtocolEndpoint::InfoRefs,
        });
    }

    if let Some(prefix) = full_path.strip_suffix(UPLOAD_PACK) {
        let locator = classify_repo_locator(prefix)?;
        if method != Method::POST {
            return Err(ProtocolError::InvalidInput(format!(
                "{UPLOAD_PACK} only supports POST"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: normalize_repo_path(prefix),
            locator,
            endpoint: GitProtocolEndpoint::UploadPack,
        });
    }

    if let Some(prefix) = full_path.strip_suffix(RECEIVE_PACK) {
        let locator = classify_repo_locator(prefix)?;
        if method != Method::POST {
            return Err(ProtocolError::InvalidInput(format!(
                "{RECEIVE_PACK} only supports POST"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: normalize_repo_path(prefix),
            locator,
            endpoint: GitProtocolEndpoint::ReceivePack,
        });
    }

    Err(ProtocolError::NotFound(
        "Operation not supported".to_string(),
    ))
}

/// Strips a single trailing `.git` suffix from a path string and normalizes
/// the root repo path to `/`.  An empty prefix (e.g. from `GET /info/refs`)
/// maps to `/` so that the monorepo root's refs are correctly resolved.
fn normalize_repo_path(path: &str) -> PathBuf {
    let stripped = path
        .rsplit_once(".git")
        .filter(|(_, after)| after.is_empty())
        .map(|(before, _)| before)
        .unwrap_or(path);
    if stripped.is_empty() {
        PathBuf::from("/")
    } else {
        PathBuf::from(stripped)
    }
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
    fn classify_repo_locator_vectors() {
        for raw in ["/.view/web.git", ".view/web.git", "/.view/web"] {
            assert_eq!(
                classify_repo_locator(raw).unwrap(),
                RepoLocator::View(ViewLocator::Named {
                    name: "web".to_owned(),
                    version: None,
                })
            );
        }
        assert_eq!(
            classify_repo_locator("/.view/agent/task-1234@3.git").unwrap(),
            RepoLocator::View(ViewLocator::Named {
                name: "agent/task-1234".to_owned(),
                version: Some(3),
            })
        );
        let filter_id = "a".repeat(64);
        for raw in [
            format!("/.filter/{filter_id}.git"),
            format!(".filter/{filter_id}"),
        ] {
            assert_eq!(
                classify_repo_locator(&raw).unwrap(),
                RepoLocator::View(ViewLocator::FilterId(filter_id.clone()))
            );
        }
        for raw in [
            "/.view".to_owned(),
            "/.view.git".to_owned(),
            "/.filter".to_owned(),
            format!("/.filter/{}.git", "a".repeat(63)),
            format!("/.filter/{}.git", "A".repeat(64)),
            format!("/.filter/{filter_id}/x.git"),
            "/.view/a@0.git".to_owned(),
            "/.view/a@x.git".to_owned(),
            "/.view/a@.git".to_owned(),
            "/.view/a b.git".to_owned(),
            "/.view/a//b.git".to_owned(),
            "/.view/a/../b.git".to_owned(),
            "/.view/x.git.git".to_owned(),
        ] {
            assert!(
                matches!(classify_repo_locator(&raw), Err(ProtocolError::NotFound(_))),
                "{raw}"
            );
        }
        for (raw, expected) in [
            ("/project.git", "/project"),
            ("/.viewer/a.git", "/.viewer/a"),
            ("/project/.view/x.git", "/project/.view/x"),
            ("project.git", "project"),
            ("", "/"),
        ] {
            assert_eq!(
                classify_repo_locator(raw).unwrap(),
                RepoLocator::Path(PathBuf::from(expected))
            );
        }
    }

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
    fn invalid_view_locator_precedes_method_error() {
        let err = parse_git_protocol_path(&Method::POST, "/.view/a@0.git/info/refs").unwrap_err();
        assert!(matches!(err, ProtocolError::NotFound(_)));
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
    fn parse_rejects_unknown_endpoint_as_not_found() {
        let err = parse_git_protocol_path(&Method::GET, "/project.git/git-status").unwrap_err();
        assert!(matches!(err, ProtocolError::NotFound(_)));
        assert!(err.to_string().contains("Operation not supported"));
    }

    #[test]
    fn normalize_repo_path_leaves_non_trailing_segment() {
        assert_eq!(
            normalize_repo_path("/foo.git/bar.git"),
            PathBuf::from("/foo.git/bar")
        );
        assert_eq!(
            normalize_repo_path("/foo/bar.git"),
            PathBuf::from("/foo/bar")
        );
        assert_eq!(normalize_repo_path("/foo/bar"), PathBuf::from("/foo/bar"));
    }

    #[test]
    fn normalize_repo_path_maps_empty_to_root() {
        assert_eq!(normalize_repo_path(""), PathBuf::from("/"));
        assert_eq!(normalize_repo_path("/"), PathBuf::from("/"));
    }
}
