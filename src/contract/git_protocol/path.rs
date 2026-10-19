//! Git smart-protocol HTTP path parser.
//!
//! Normalizes repository paths and identifies the three smart-protocol endpoints,
//! stripping only a trailing `.git` suffix (not `.git` segments embedded earlier
//! in the path).

use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use http::Method;

use crate::{
    ceres::{
        protocol::ServiceType,
        view::{VIEW_URL_RESERVED_NAMES, name::validate_view_name},
    },
    common::errors::ProtocolError,
};

const TRACES_LOCATOR_NOT_FOUND: &str = "traces locator not found";

#[derive(Debug, PartialEq, Eq)]
pub enum RepoLocator {
    Path(PathBuf),
    View(ViewLocator),
}

/// Virtual traces URL. Decided before a path is treated as Native or ImportRepo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TracesLocator {
    ByPath {
        target_path: String,
        source_id: String,
        epoch: String,
        hash_kind: TracesObjectFormat,
    },
    ByScope {
        stream_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TracesObjectFormat {
    Sha1,
    Sha256,
}

impl TracesObjectFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    pub fn zero_id(self) -> String {
        "0".repeat(match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        })
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            _ => None,
        }
    }
}

/// Locator decision. Traces is resolved before view and before a native/import path.
#[derive(Debug, PartialEq, Eq)]
pub enum LocatorDispatch {
    Traces(TracesLocator),
    View(ViewLocator),
    NativeOrImport(PathBuf),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ViewLocator {
    Named { name: String, version: Option<u32> },
    FilterId(String),
}

fn traces_not_found() -> ProtocolError {
    ProtocolError::NotFound(TRACES_LOCATOR_NOT_FOUND.to_owned())
}

/// Parses a traces virtual URL.
///
/// `Ok(None)` means the path is not in the `.traces` namespace. `Err` means it
/// is in that namespace but the grammar does not match, so the caller must not
/// fall through to Native or ImportRepo.
pub fn parse_traces_locator(raw: &str) -> Result<Option<TracesLocator>, ProtocolError> {
    let stripped = raw.strip_suffix(".git").unwrap_or(raw);
    let Some(rest) = stripped.strip_prefix("/.traces/") else {
        if in_traces_namespace(stripped) {
            return Err(traces_not_found());
        }
        return Ok(None);
    };
    if rest.is_empty() {
        return Err(traces_not_found());
    }
    let mut parts = rest.split('/');
    match parts.next() {
        Some("by-path") => parse_by_path(&mut parts).map(Some),
        Some("by-scope") => parse_by_scope(&mut parts).map(Some),
        _ => Err(traces_not_found()),
    }
}

fn parse_by_scope<'a>(
    parts: &mut impl Iterator<Item = &'a str>,
) -> Result<TracesLocator, ProtocolError> {
    let stream_id = canonical_uuid(parts.next().ok_or_else(traces_not_found)?)?;
    if parts.next().is_some() {
        return Err(traces_not_found());
    }
    Ok(TracesLocator::ByScope { stream_id })
}

fn parse_by_path<'a>(
    parts: &mut impl Iterator<Item = &'a str>,
) -> Result<TracesLocator, ProtocolError> {
    let target_path =
        canonical_target_path(&decode_segment(parts.next().ok_or_else(traces_not_found)?)?)?;
    let self_lit = parts.next().ok_or_else(traces_not_found)?;
    if self_lit != "self" {
        return Err(traces_not_found());
    }
    let source_id = decode_segment(parts.next().ok_or_else(traces_not_found)?)?;
    let epoch = canonical_uuid(parts.next().ok_or_else(traces_not_found)?)?;
    let hash_kind = parts
        .next()
        .and_then(TracesObjectFormat::parse)
        .ok_or_else(traces_not_found)?;
    if parts.next().is_some() {
        return Err(traces_not_found());
    }
    Ok(TracesLocator::ByPath {
        target_path,
        source_id,
        epoch,
        hash_kind,
    })
}

fn decode_segment(encoded: &str) -> Result<String, ProtocolError> {
    if encoded.is_empty() || encoded.contains('=') {
        return Err(traces_not_found());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| traces_not_found())?;
    let text = String::from_utf8(bytes).map_err(|_| traces_not_found())?;
    if text.is_empty() || text.contains('\0') {
        return Err(traces_not_found());
    }
    Ok(text)
}

fn canonical_uuid(text: &str) -> Result<String, ProtocolError> {
    let id = uuid::Uuid::parse_str(text).map_err(|_| traces_not_found())?;
    let canonical = id.as_hyphenated().to_string();
    if text != canonical {
        return Err(traces_not_found());
    }
    Ok(canonical)
}

fn canonical_target_path(raw: &str) -> Result<String, ProtocolError> {
    if raw.contains('\0') || raw.contains('\\') || !raw.starts_with('/') {
        return Err(traces_not_found());
    }
    if raw == "/" {
        return Ok(raw.to_owned());
    }
    if raw.ends_with('/') {
        return Err(traces_not_found());
    }
    if raw[1..]
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(traces_not_found());
    }
    Ok(raw.to_owned())
}

fn in_traces_namespace(raw: &str) -> bool {
    let stripped = raw.strip_suffix(".git").unwrap_or(raw);
    if !stripped.contains(".traces") {
        return false;
    }
    let mut parts = Vec::new();
    for part in stripped.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    parts.first() == Some(&".traces")
}

/// Resolves traces before view and before a native or import path.
///
/// The contract URL ends in `.git`. `parse_traces_locator` still accepts the
/// normalized repository path after that suffix is removed, which is what
/// `SmartSession` stores.
pub fn dispatch_repo_locator(raw: &str) -> Result<LocatorDispatch, ProtocolError> {
    if in_traces_namespace(raw) && !raw.ends_with(".git") {
        return Err(traces_not_found());
    }
    if let Some(locator) = parse_traces_locator(raw)? {
        return Ok(LocatorDispatch::Traces(locator));
    }
    match classify_view_or_native(raw)? {
        RepoLocator::View(view) => Ok(LocatorDispatch::View(view)),
        RepoLocator::Path(path) => Ok(LocatorDispatch::NativeOrImport(path)),
    }
}

pub fn classify_repo_locator(raw: &str) -> Result<RepoLocator, ProtocolError> {
    // Code-repository classification never returns Path for `.traces`.
    // Smart HTTP uses `dispatch_repo_locator` / `GitProtocolPath.traces`, and
    // `repo_handler_with_commands` builds TraceRepo before Native/ImportRepo.
    match dispatch_repo_locator(raw)? {
        LocatorDispatch::Traces(_) => Err(ProtocolError::NotFound(
            "traces locator is not a code repository".to_owned(),
        )),
        LocatorDispatch::View(view) => Ok(RepoLocator::View(view)),
        LocatorDispatch::NativeOrImport(path) => Ok(RepoLocator::Path(path)),
    }
}

fn classify_view_or_native(raw: &str) -> Result<RepoLocator, ProtocolError> {
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
    /// Set when the URL is a traces virtual repository. HTTP routing still
    /// carries `locator` as `Path` so existing matches compile; the handler
    /// reads this field's path through `parse_traces_locator` and selects
    /// TraceRepo before Native/ImportRepo.
    pub traces: Option<TracesLocator>,
    /// Identified endpoint.
    pub endpoint: GitProtocolEndpoint,
}

fn locate_http_prefix(prefix: &str) -> Result<(RepoLocator, Option<TracesLocator>), ProtocolError> {
    match dispatch_repo_locator(prefix)? {
        LocatorDispatch::Traces(locator) => Ok((
            RepoLocator::Path(normalize_repo_path(prefix)),
            Some(locator),
        )),
        LocatorDispatch::View(view) => Ok((RepoLocator::View(view), None)),
        LocatorDispatch::NativeOrImport(path) => Ok((RepoLocator::Path(path), None)),
    }
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
        let (locator, traces) = locate_http_prefix(prefix)?;
        if method != Method::GET {
            return Err(ProtocolError::InvalidInput(format!(
                "{INFO_REFS} only supports GET"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: normalize_repo_path(prefix),
            locator,
            traces,
            endpoint: GitProtocolEndpoint::InfoRefs,
        });
    }

    if let Some(prefix) = full_path.strip_suffix(UPLOAD_PACK) {
        let (locator, traces) = locate_http_prefix(prefix)?;
        if method != Method::POST {
            return Err(ProtocolError::InvalidInput(format!(
                "{UPLOAD_PACK} only supports POST"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: normalize_repo_path(prefix),
            locator,
            traces,
            endpoint: GitProtocolEndpoint::UploadPack,
        });
    }

    if let Some(prefix) = full_path.strip_suffix(RECEIVE_PACK) {
        let (locator, traces) = locate_http_prefix(prefix)?;
        if method != Method::POST {
            return Err(ProtocolError::InvalidInput(format!(
                "{RECEIVE_PACK} only supports POST"
            )));
        }
        return Ok(GitProtocolPath {
            repo_path: normalize_repo_path(prefix),
            locator,
            traces,
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
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

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

    const STREAM_ID: &str = "abcdef01-2345-4678-89ab-cdef01234567";
    const EPOCH_ID: &str = "22222222-2222-4222-8222-222222222222";
    const TARGET_SEGMENT: &str = "L3Byb2plY3QvZm9v";
    const SOURCE_SEGMENT: &str = "bGVnYWN5L3JlcG8gaWQ";

    fn by_scope_url() -> String {
        format!("/.traces/by-scope/{STREAM_ID}.git")
    }

    fn by_path_url() -> String {
        format!("/.traces/by-path/{TARGET_SEGMENT}/self/{SOURCE_SEGMENT}/{EPOCH_ID}/sha256.git")
    }

    #[test]
    fn trace_locator_dispatch_parses_by_path_and_by_scope_before_native() {
        assert_eq!(
            dispatch_repo_locator(&by_scope_url()).unwrap(),
            LocatorDispatch::Traces(TracesLocator::ByScope {
                stream_id: STREAM_ID.to_owned(),
            })
        );
        assert_eq!(
            parse_traces_locator(by_scope_url().trim_end_matches(".git")).unwrap(),
            Some(TracesLocator::ByScope {
                stream_id: STREAM_ID.to_owned(),
            })
        );
        let missing_suffix =
            dispatch_repo_locator(by_scope_url().trim_end_matches(".git")).unwrap_err();
        assert!(
            matches!(missing_suffix, ProtocolError::NotFound(ref message) if message == "traces locator not found"),
            "{missing_suffix}"
        );
        assert_eq!(
            dispatch_repo_locator(&by_path_url()).unwrap(),
            LocatorDispatch::Traces(TracesLocator::ByPath {
                target_path: "/project/foo".to_owned(),
                source_id: "legacy/repo id".to_owned(),
                epoch: EPOCH_ID.to_owned(),
                hash_kind: TracesObjectFormat::Sha256,
            })
        );

        let root = URL_SAFE_NO_PAD.encode("/");
        let spaced = URL_SAFE_NO_PAD.encode("/project/foo ");
        let source = URL_SAFE_NO_PAD.encode("opaque");
        let sha1 = format!("/.traces/by-path/{root}/self/{source}/{EPOCH_ID}/sha1.git");
        match dispatch_repo_locator(&sha1).unwrap() {
            LocatorDispatch::Traces(TracesLocator::ByPath {
                target_path,
                source_id,
                hash_kind,
                ..
            }) => {
                assert_eq!(target_path, "/");
                assert_eq!(source_id, "opaque");
                assert_eq!(hash_kind, TracesObjectFormat::Sha1);
            }
            other => panic!("by-path sha1 was classified as {other:?}"),
        }
        let spaced_url = format!("/.traces/by-path/{spaced}/self/{source}/{EPOCH_ID}/sha1.git");
        match dispatch_repo_locator(&spaced_url).unwrap() {
            LocatorDispatch::Traces(TracesLocator::ByPath { target_path, .. }) => {
                assert_eq!(target_path, "/project/foo ");
            }
            other => panic!("spaced target was classified as {other:?}"),
        }

        let parsed =
            parse_git_protocol_path(&Method::GET, &format!("{}/info/refs", by_scope_url()))
                .unwrap();
        assert_eq!(
            parsed.traces,
            Some(TracesLocator::ByScope {
                stream_id: STREAM_ID.to_owned(),
            })
        );
        assert!(matches!(parsed.locator, RepoLocator::Path(_)));
        assert_eq!(parsed.endpoint, GitProtocolEndpoint::InfoRefs);
        assert!(!matches!(
            dispatch_repo_locator(&by_scope_url()).unwrap(),
            LocatorDispatch::NativeOrImport(_)
        ));
        let code_class = classify_repo_locator(&by_scope_url()).unwrap_err();
        assert!(
            matches!(code_class, ProtocolError::NotFound(ref message) if message == "traces locator is not a code repository"),
            "{code_class}"
        );
        let code_path = classify_repo_locator(&by_path_url()).unwrap_err();
        assert!(
            matches!(code_path, ProtocolError::NotFound(ref message) if message == "traces locator is not a code repository"),
            "{code_path}"
        );

        let upload =
            parse_git_protocol_path(&Method::POST, &format!("{}/git-upload-pack", by_path_url()))
                .unwrap();
        assert!(matches!(upload.traces, Some(TracesLocator::ByPath { .. })));
        assert_eq!(upload.endpoint, GitProtocolEndpoint::UploadPack);

        let receive = parse_git_protocol_path(
            &Method::POST,
            &format!("{}/git-receive-pack", by_scope_url()),
        )
        .unwrap();
        assert!(receive.traces.is_some());
        assert_eq!(receive.endpoint, GitProtocolEndpoint::ReceivePack);

        let native =
            parse_git_protocol_path(&Method::GET, "/third-party/vendor.git/info/refs").unwrap();
        assert!(native.traces.is_none());
        assert_eq!(
            dispatch_repo_locator("/third-party/vendor.git").unwrap(),
            LocatorDispatch::NativeOrImport(PathBuf::from("/third-party/vendor"))
        );
        assert!(matches!(
            dispatch_repo_locator("/.view/web.git").unwrap(),
            LocatorDispatch::View(_)
        ));
        assert!(
            parse_git_protocol_path(&Method::GET, "/.view/web.git/info/refs")
                .unwrap()
                .traces
                .is_none()
        );

        for raw in [
            "/.traces.git",
            "/.traces/by-scope",
            &format!("//.traces/by-scope/{STREAM_ID}.git"),
            &format!(".traces/by-scope/{STREAM_ID}.git"),
            &format!("/./.traces/by-scope/{STREAM_ID}.git"),
            &format!("/other/../.traces/by-scope/{STREAM_ID}.git"),
            "/.traces/by-scope/not-a-uuid.git",
            "/.traces/by-scope/abcdef01-2345-4678-89ab-cdef01234567/extra.git",
            "/.traces/by-scope/abcdef01-2345-4678-89ab-cdef0123456g.git",
            &format!("/.traces/by-scope/{}.git", STREAM_ID.to_ascii_uppercase()),
            "/.traces/by-path/L3Byb2plY3QvZm9v/owner/bGVnYWN5/abcdef01-2345-4678-89ab-cdef01234567/sha1.git",
            &format!(
                "/.traces/by-path/{TARGET_SEGMENT}/self/{SOURCE_SEGMENT}/{EPOCH_ID}/sha512.git"
            ),
            &format!(
                "/.traces/by-path/{TARGET_SEGMENT}/self/{SOURCE_SEGMENT}/{EPOCH_ID}/sha256/extra.git"
            ),
            &format!(
                "/.traces/by-path/{TARGET_SEGMENT}=/self/{SOURCE_SEGMENT}/{EPOCH_ID}/sha1.git"
            ),
        ] {
            let err = dispatch_repo_locator(raw).unwrap_err();
            assert!(
                matches!(err, ProtocolError::NotFound(ref message) if message == "traces locator not found"),
                "{raw}: {err}"
            );
            let http = format!("{}/info/refs", raw.trim_end_matches(".git"));
            let http_err = parse_git_protocol_path(&Method::GET, &http).unwrap_err();
            assert!(
                matches!(http_err, ProtocolError::NotFound(_)),
                "{http}: {http_err}"
            );
        }

        let dotted = URL_SAFE_NO_PAD.encode("/project/../secret");
        let bare = URL_SAFE_NO_PAD.encode("project/foo");
        let trailed = URL_SAFE_NO_PAD.encode("/project/foo/");
        for encoded in [dotted, bare, trailed] {
            let raw =
                format!("/.traces/by-path/{encoded}/self/{SOURCE_SEGMENT}/{EPOCH_ID}/sha1.git");
            assert!(
                matches!(dispatch_repo_locator(&raw), Err(ProtocolError::NotFound(_))),
                "{raw}"
            );
        }

        assert!(matches!(
            dispatch_repo_locator(
                "/project/.traces/by-scope/abcdef01-2345-4678-89ab-cdef01234567.git"
            )
            .unwrap(),
            LocatorDispatch::NativeOrImport(_)
        ));
        assert!(matches!(
            dispatch_repo_locator("/.traces-backup/repo.git").unwrap(),
            LocatorDispatch::NativeOrImport(_)
        ));
    }
}
