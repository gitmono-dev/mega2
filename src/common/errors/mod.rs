use std::convert::Infallible;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use cedar_policy::ParseErrors;
use config::ConfigError;
use git_internal::errors::GitError;
use thiserror::Error;

use crate::contract::api::common::CommonResult;
#[rustfmt::skip]
use crate::orbit_api::error::IoOrbitError;

mod api;
mod policy;
mod vault;

pub use api::ApiError;
pub(crate) use api::map_ceres_error;
pub use policy::{ContextError, SaturnContextError};
pub use vault::{RvError, VaultError, VaultResult};

pub type MegaResult = Result<(), MegaError>;

#[derive(Error, Debug)]
pub enum MegaError {
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("Redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("serialization error: {0}")]
    EncodeError(#[from] rkyv::rancor::Error),
    #[error("JSON serialization error: {0}")]
    SerdeJson(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Database error: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("PGP error: {0}")]
    Pgp(#[from] Box<pgp::errors::Error>),
    #[error("Clap error: {0}")]
    Clap(#[from] clap::Error),
    #[error("Generic error: {0}")]
    Anyhow(#[from] anyhow::Error),
    #[error("Git error: {0}")]
    Git(#[from] GitError),
    #[error("Buck API error: {0}")]
    Buck(#[from] BuckError),
    #[error("Not Found error: {0}")]
    NotFound(String),
    #[error("ObjStorage error: {0}")]
    ObjStorage(String),
    #[error("ObjStorage not found: {0}")]
    ObjStorageNotFound(String),
    #[error("ObjStorage inconsistent: {0}")]
    ObjStorageInconsistent(String),
    #[error("Monorepo root ref changed concurrently (attach should retry)")]
    StaleMonorepoRootRef,
    #[error("lazy materialize aborted after concurrent root updates; retry the advertise")]
    MaterializeAborted,
    /// New-branch push whose first-parent chain, walked through the pack,
    /// reaches a parentless root (plan-20260923 ADR-FU-07): for a new tip no
    /// known commit on the way to fork from; for a known tip any chain that
    /// reaches a root, even history other refs already reference. The
    /// display keeps the historical text, including the `Other error: `
    /// prefix it always had, so push rejections read the same as before the
    /// variant existed.
    #[error("Other error: Can not init directory under monorepo directory!")]
    OrphanChain,
    /// Monorepo path creation policy (plan-20260923 ADR-FU-03); the text is
    /// the stable `<CODE>: <message>` shared by Git `ng` lines and the API.
    #[error("{0}")]
    PathPolicy(#[from] PathPolicyError),
    /// ImportRepo lifecycle and ref errors (plan-20260923 ADR-FU-03); the
    /// text is the stable `<CODE>: <message>` shared by Git `ng` lines and
    /// the API.
    #[error("{0}")]
    ImportRepo(#[from] ImportRepoError),
    #[error("Other error: {0}")]
    Other(String),
    /// Process exit with a frozen CLI status code (UN-29 authz-audit table).
    #[error("{message}")]
    CliExit { code: i32, message: String },
}

impl MegaError {
    pub fn print(&self) {
        eprintln!("{}", self);
    }

    /// Status code for the process. Most errors remain exit 1; CLI modes that
    /// freeze a table (UN-29) use [`MegaError::CliExit`].
    pub fn process_exit_code(&self) -> i32 {
        match self {
            Self::CliExit { code, .. } => *code,
            _ => 1,
        }
    }

    pub fn cli_exit(code: i32, message: impl Into<String>) -> Self {
        Self::CliExit {
            code,
            message: message.into(),
        }
    }

    /// True for Postgres deadlock (`40P01`) or serialization failure (`40001`).
    pub fn is_retryable_db_serialization(&self) -> bool {
        match self {
            MegaError::Db(err) => db_err_is_retryable_serialization(err),
            _ => is_retryable_pg_conflict(&self.to_string()),
        }
    }
}

/// True when a SeaORM error is a Postgres deadlock (`40P01`) or
/// serialization failure (`40001`) that is safe to retry.
pub fn db_err_is_retryable_serialization(err: &sea_orm::DbErr) -> bool {
    is_retryable_pg_conflict(&err.to_string()) || is_retryable_pg_conflict(&format!("{err:?}"))
}

fn is_retryable_pg_conflict(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("deadlock detected")
        || lower.contains("40p01")
        || lower.contains("40001")
        || lower.contains("serialization failure")
        || lower.contains("could not serialize access")
}

impl From<Infallible> for MegaError {
    fn from(err: Infallible) -> MegaError {
        match err {}
    }
}

impl From<ParseErrors> for MegaError {
    fn from(err: ParseErrors) -> MegaError {
        MegaError::Other(err.to_string())
    }
}

impl From<IoOrbitError> for MegaError {
    fn from(err: IoOrbitError) -> Self {
        let is_not_found = err.is_not_found();
        match err {
            IoOrbitError::ObjectStore { message, .. } if is_not_found => {
                MegaError::ObjStorageNotFound(message)
            }
            IoOrbitError::ObjectStore { message, .. } => MegaError::ObjStorage(message),
            IoOrbitError::Io(e) => MegaError::Io(e),
            IoOrbitError::SerdeJson(e) => MegaError::SerdeJson(e),
            IoOrbitError::TomlDe(e) => MegaError::Other(e.to_string()),
            IoOrbitError::WriteManifestPreconditionFailed => {
                MegaError::Other("write manifest precondition failed".to_string())
            }
            IoOrbitError::Other(e) => MegaError::Other(e),
        }
    }
}

impl From<MegaError> for GitError {
    fn from(val: MegaError) -> Self {
        match val {
            MegaError::NotFound(msg) => GitError::CustomError(format!("[code:404] {msg}")),
            // Product writes return `GitError`; the marker keeps the API status
            // and `ApiError` strips it, so `err_message` is the policy text.
            MegaError::PathPolicy(err) => {
                GitError::CustomError(format!("[code:{}] {err}", err.http_status().as_u16()))
            }
            MegaError::ImportRepo(err) => {
                GitError::CustomError(format!("[code:{}] {err}", err.http_status().as_u16()))
            }
            MegaError::ObjStorageNotFound(msg) => {
                GitError::CustomError(format!("[code:404] ObjStorage not found: {msg}"))
            }
            other => GitError::CustomError(other.to_string()),
        }
    }
}

/// Why a monorepo path may not be created or written (plan-20260923
/// ADR-FU-03). `Display` starts with a stable code followed by `: ` and is a
/// single line (paths, root names and reasons have control characters
/// escaped) so it can travel in a Git `ng` line; messages never include
/// config file paths, `base_dir`, service addresses or credentials.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum PathPolicyError {
    #[error(
        "MONO_PATH_NOT_ALLOWED: cannot create {path:?}: new paths must be under one of the monorepo roots ({roots}); paths under {import_dir:?} are ImportRepos, created by pushing a repository to them",
        roots = single_line(&.allowed_roots.join(", "))
    )]
    NotAllowed {
        path: String,
        allowed_roots: Vec<String>,
        import_dir: String,
    },
    #[error(
        "MONO_PATH_UNINITIALIZED: {path:?} does not exist yet; provision it with `mega2 path provision --server <url> {command_path}` or `POST /api/v1/path/provision`, then clone it, commit on top and push",
        command_path = single_line(.path)
    )]
    Uninitialized { path: String },
    #[error("MONO_PATH_INVALID: {path:?}: {}", single_line(.reason))]
    Invalid { path: String, reason: String },
    #[error("MONO_PATH_CONFLICT: {path:?}: {component:?} exists and is not a directory")]
    Conflict { path: String, component: String },
}

impl PathPolicyError {
    /// Stable machine-readable code (the `Display` prefix).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAllowed { .. } => "MONO_PATH_NOT_ALLOWED",
            Self::Uninitialized { .. } => "MONO_PATH_UNINITIALIZED",
            Self::Invalid { .. } => "MONO_PATH_INVALID",
            Self::Conflict { .. } => "MONO_PATH_CONFLICT",
        }
    }

    /// HTTP status for the API face (`ApiError` and the `[code:…]` marker the
    /// `GitError` channel carries).
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::NotAllowed { .. } | Self::Invalid { .. } => StatusCode::BAD_REQUEST,
            Self::Uninitialized { .. } | Self::Conflict { .. } => StatusCode::CONFLICT,
        }
    }
}

/// Why an ImportRepo operation was refused (plan-20260923 ADR-FU-03 item 2).
/// `Display` starts with a stable code followed by `: ` and is a single line
/// (paths, ref names and ids have control characters escaped) so it can
/// travel in a Git `ng` line.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ImportRepoError {
    #[error(
        "IMPORT_REPO_PATH_OCCUPIED: {path:?} already holds content that is not this ImportRepo"
    )]
    PathOccupied { path: String },
    #[error(
        "IMPORT_REPO_STALE_REF: {ref_name:?} {}; fetch and push again",
        stale_ref_detail(.expected)
    )]
    StaleRef { ref_name: String, expected: String },
    #[error("IMPORT_REPO_REMOVED: {path:?} was removed; push again to import it anew")]
    Removed { path: String },
    #[error("IMPORT_REPO_PATH_INVALID: {path:?}: {}", single_line(.reason))]
    PathInvalid { path: String, reason: String },
    #[error("IMPORT_REPO_HAS_CHILDREN: {path:?} contains other ImportRepos; remove them first")]
    HasChildren { path: String },
    #[error("IMPORT_REPO_CLEANUP_NOT_FOUND: no cleanup {cleanup_id:?} for {path:?}")]
    CleanupNotFound { path: String, cleanup_id: String },
}

impl ImportRepoError {
    /// Stable machine-readable code (the `Display` prefix).
    pub fn code(&self) -> &'static str {
        match self {
            Self::PathOccupied { .. } => "IMPORT_REPO_PATH_OCCUPIED",
            Self::StaleRef { .. } => "IMPORT_REPO_STALE_REF",
            Self::Removed { .. } => "IMPORT_REPO_REMOVED",
            Self::PathInvalid { .. } => "IMPORT_REPO_PATH_INVALID",
            Self::HasChildren { .. } => "IMPORT_REPO_HAS_CHILDREN",
            Self::CleanupNotFound { .. } => "IMPORT_REPO_CLEANUP_NOT_FOUND",
        }
    }

    /// HTTP status for the API face; `Removed` reaches the API as 409 from
    /// the edit/save object batch (plan-20260923 FU-18) and from the
    /// edit/save pre-reads, default-ref write and tag writes (FU-19).
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::PathInvalid { .. } => StatusCode::BAD_REQUEST,
            Self::CleanupNotFound { .. } => StatusCode::NOT_FOUND,
            Self::PathOccupied { .. }
            | Self::StaleRef { .. }
            | Self::Removed { .. }
            | Self::HasChildren { .. } => StatusCode::CONFLICT,
        }
    }
}

/// `StaleRef` detail: a `Create` (zero `expected`) found the ref already
/// there; anything else found it moved.
fn stale_ref_detail(expected: &str) -> String {
    if !expected.is_empty() && expected.bytes().all(|b| b == b'0') {
        "already exists".to_owned()
    } else {
        format!(
            "changed since it was advertised (expected {})",
            single_line(expected)
        )
    }
}

/// Escape control characters (newline, NUL, …) so the text stays one line.
fn single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Error, Debug)]
pub enum GitLFSError {
    #[error("Something went wrong in Git LFS: {0}")]
    GeneralError(String),
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum DiffParseError {
    InvalidHunkHeader(String),
    InvalidRange(String),
    InvalidNumber(String),
}

#[derive(Debug, Error)]
pub enum BuckError {
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("Session expired")]
    SessionExpired,
    #[error("File not in manifest: {0}")]
    FileNotInManifest(String),
    #[error("Rate limit exceeded")]
    RateLimitExceeded,
    #[error("File size exceeds limit: {0} > {1}")]
    FileSizeExceedsLimit(u64, u64),
    #[error("File already uploaded: {0}")]
    FileAlreadyUploaded(String),
    #[error("Hash mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("Validation error: {0}")]
    ValidationError(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Invalid session status: expected {expected:?}, got {actual:?}")]
    InvalidSessionStatus { expected: String, actual: String },
    #[error("Files not fully uploaded: {missing_count} files remaining")]
    FilesNotFullyUploaded { missing_count: u32 },
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("{0}")]
    IO(#[from] std::io::Error),
    #[error("Authentication failed: {0}")]
    Deny(String),
    #[error("Forbidden: {0}")]
    Forbidden(String),
    #[error("Repository not found: {0}")]
    NotFound(String),
    #[error("PackFile too large: {0}")]
    TooLarge(String),
    #[error("Invalid Input: {0}")]
    InvalidInput(String),
    #[error("HTTP Push Has Been Disabled")]
    Disabled,
    #[error("lazy materialize aborted after concurrent root updates; retry the advertise")]
    AdvertiseFailed,
}

impl From<MegaError> for ProtocolError {
    fn from(err: MegaError) -> ProtocolError {
        match err {
            MegaError::MaterializeAborted => ProtocolError::AdvertiseFailed,
            other => ProtocolError::InvalidInput(other.to_string()),
        }
    }
}

impl IntoResponse for ProtocolError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ProtocolError::Deny(err) => (StatusCode::UNAUTHORIZED, err),
            ProtocolError::Forbidden(err) => (StatusCode::FORBIDDEN, err),
            ProtocolError::TooLarge(err) => (StatusCode::PAYLOAD_TOO_LARGE, err),
            ProtocolError::NotFound(err) => (StatusCode::NOT_FOUND, err),
            ProtocolError::InvalidInput(err) => (StatusCode::BAD_REQUEST, err),
            ProtocolError::AdvertiseFailed => (
                StatusCode::SERVICE_UNAVAILABLE,
                MegaError::MaterializeAborted.to_string(),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong".to_owned(),
            ),
        };

        (status, Json(CommonResult::<String>::failed(&message))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path_policy_samples() -> [PathPolicyError; 4] {
        [
            PathPolicyError::NotAllowed {
                path: "/vendor/lib".to_owned(),
                allowed_roots: vec!["/project".to_owned(), "/third-party".to_owned()],
                import_dir: "/third-party".to_owned(),
            },
            PathPolicyError::Uninitialized {
                path: "/project/new".to_owned(),
            },
            PathPolicyError::Invalid {
                path: "/project//x".to_owned(),
                reason: "path must be canonical".to_owned(),
            },
            PathPolicyError::Conflict {
                path: "/project/a/b".to_owned(),
                component: "/project/a".to_owned(),
            },
        ]
    }

    #[test]
    fn path_policy_display_codes() {
        let codes: Vec<&str> = path_policy_samples().iter().map(|err| err.code()).collect();
        assert_eq!(
            codes,
            [
                "MONO_PATH_NOT_ALLOWED",
                "MONO_PATH_UNINITIALIZED",
                "MONO_PATH_INVALID",
                "MONO_PATH_CONFLICT",
            ]
        );
        for err in path_policy_samples() {
            let code = err.code();
            assert!(err.to_string().starts_with(&format!("{code}: ")), "{err}");
            // Wrapping keeps the text verbatim (no `Other error:` prefix), and
            // the Git face carries the same line.
            let mega = MegaError::from(err.clone());
            assert_eq!(mega.to_string(), err.to_string());
            let git: GitError = mega.into();
            assert!(git.to_string().contains(&err.to_string()), "{git}");
        }
        let uninitialized = path_policy_samples()[1].to_string();
        assert!(uninitialized.contains("mega2 path provision --server <url> /project/new"));
        assert!(uninitialized.contains("POST /api/v1/path/provision"));
    }

    async fn api_error_body(err: ApiError) -> (StatusCode, String) {
        let response = err.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        (
            status,
            json["err_message"]
                .as_str()
                .expect("err_message")
                .to_owned(),
        )
    }

    #[tokio::test]
    async fn path_policy_api_status() {
        let expected = [
            StatusCode::BAD_REQUEST,
            StatusCode::CONFLICT,
            StatusCode::BAD_REQUEST,
            StatusCode::CONFLICT,
        ];
        for (err, status) in path_policy_samples().into_iter().zip(expected) {
            let text = err.to_string();
            assert_eq!(err.http_status(), status, "{text}");
            // Typed MegaError channel.
            let direct = ApiError::from(MegaError::from(err.clone()));
            assert_eq!(api_error_body(direct).await, (status, text.clone()));
            // Bare PathPolicyError (e.g. `classify_creation_path(..)?` in a handler).
            let bare = ApiError::from(err.clone());
            assert_eq!(api_error_body(bare).await, (status, text.clone()));
            // GitError channel used by product writes.
            let git: GitError = MegaError::from(err).into();
            assert_eq!(api_error_body(ApiError::from(git)).await, (status, text));
        }

        // A `[code:N]` inside a user path must not truncate the stable text.
        let tricky = PathPolicyError::NotAllowed {
            path: "/vendor/[code:1]x".to_owned(),
            allowed_roots: vec!["/project".to_owned()],
            import_dir: "/third-party".to_owned(),
        };
        let text = tricky.to_string();
        for err in [
            ApiError::from(tricky.clone()),
            ApiError::from(MegaError::from(tricky.clone())),
            ApiError::from(GitError::from(MegaError::from(tricky))),
        ] {
            assert_eq!(
                api_error_body(err).await,
                (StatusCode::BAD_REQUEST, text.clone())
            );
        }
    }

    #[tokio::test]
    async fn import_repo_error_domain() {
        let samples = [
            ImportRepoError::PathOccupied {
                path: "/third-party/lib".to_owned(),
            },
            ImportRepoError::StaleRef {
                ref_name: "refs/heads/main".to_owned(),
                expected: "a".repeat(40),
            },
            ImportRepoError::Removed {
                path: "/third-party/lib".to_owned(),
            },
            ImportRepoError::PathInvalid {
                path: "/third-party".to_owned(),
                reason: "the ImportRepo directory itself is not a repository".to_owned(),
            },
            ImportRepoError::HasChildren {
                path: "/third-party/lib".to_owned(),
            },
            ImportRepoError::CleanupNotFound {
                path: "/third-party/lib".to_owned(),
                cleanup_id: "c1".to_owned(),
            },
        ];
        let expected = [
            ("IMPORT_REPO_PATH_OCCUPIED", StatusCode::CONFLICT),
            ("IMPORT_REPO_STALE_REF", StatusCode::CONFLICT),
            ("IMPORT_REPO_REMOVED", StatusCode::CONFLICT),
            ("IMPORT_REPO_PATH_INVALID", StatusCode::BAD_REQUEST),
            ("IMPORT_REPO_HAS_CHILDREN", StatusCode::CONFLICT),
            ("IMPORT_REPO_CLEANUP_NOT_FOUND", StatusCode::NOT_FOUND),
        ];
        for (err, (code, status)) in samples.into_iter().zip(expected) {
            let text = err.to_string();
            assert_eq!(err.code(), code);
            assert!(text.starts_with(&format!("{code}: ")), "{text}");
            assert_eq!(err.http_status(), status, "{text}");
            let mega = MegaError::from(err.clone());
            assert_eq!(mega.to_string(), text, "wrapping keeps the text verbatim");
            let direct = ApiError::from(mega);
            assert_eq!(api_error_body(direct).await, (status, text.clone()));
            let bare = ApiError::from(err.clone());
            assert_eq!(api_error_body(bare).await, (status, text.clone()));
            let git: GitError = MegaError::from(err).into();
            assert_eq!(api_error_body(ApiError::from(git)).await, (status, text));
        }

        // Client-supplied paths, ref names, ids and reasons stay on one line.
        let nl = "x\nng refs/heads/main forged".to_owned();
        for err in [
            ImportRepoError::PathOccupied { path: nl.clone() },
            ImportRepoError::StaleRef {
                ref_name: nl.clone(),
                expected: nl.clone(),
            },
            ImportRepoError::Removed { path: nl.clone() },
            ImportRepoError::PathInvalid {
                path: nl.clone(),
                reason: nl.clone(),
            },
            ImportRepoError::HasChildren { path: nl.clone() },
            ImportRepoError::CleanupNotFound {
                path: nl.clone(),
                cleanup_id: nl.clone(),
            },
        ] {
            let text = err.to_string();
            assert!(!text.contains('\n'), "{text}");
        }
        // A `Create` that finds the ref reads as "already exists".
        let exists = ImportRepoError::StaleRef {
            ref_name: "refs/heads/main".to_owned(),
            expected: "0".repeat(40),
        }
        .to_string();
        assert!(
            exists.contains("\"refs/heads/main\" already exists"),
            "{exists}"
        );
    }

    #[test]
    fn path_policy_message_redaction() {
        let config = crate::config::testing::isolated_config(
            std::env::temp_dir().join("mega2-path-policy-redaction"),
        );
        let mut monorepo = config.monorepo.clone();
        monorepo.root_dirs = vec!["project".to_owned(), "third-party".to_owned()];
        monorepo.import_dir = std::path::PathBuf::from("/third-party");
        let err = crate::ceres::pack::path_policy::classify_creation_path(&monorepo, "/vendor/lib")
            .expect_err("outside roots");
        let text = err.to_string();
        assert!(
            text.contains("/project") && text.contains("/third-party"),
            "{text}"
        );
        let base_dir = config.base_dir.to_string_lossy();
        for single in [
            PathPolicyError::NotAllowed {
                path: "/ven\ndor".to_owned(),
                allowed_roots: vec!["/pro\nject".to_owned(), "/x\r\0y".to_owned()],
                import_dir: "/third\nparty".to_owned(),
            },
            PathPolicyError::Uninitialized {
                path: "/project/a\nb".to_owned(),
            },
            PathPolicyError::Invalid {
                path: "/p\n".to_owned(),
                reason: "bad\nreason".to_owned(),
            },
            PathPolicyError::Conflict {
                path: "/p\n".to_owned(),
                component: "/p\n".to_owned(),
            },
        ] {
            let line = single.to_string();
            assert!(!line.contains(['\n', '\r', '\0']), "{line:?}");
            assert!(
                line.starts_with(&format!("{}: ", single.code())),
                "{line:?}"
            );
        }
        let escaped = PathPolicyError::NotAllowed {
            path: "/x".to_owned(),
            allowed_roots: vec!["/pro\nject".to_owned()],
            import_dir: "/third-party".to_owned(),
        }
        .to_string();
        assert!(escaped.contains("/pro\\nject"), "{escaped}");

        for leaked in [
            base_dir.as_ref(),
            config.database.db_url.as_str(),
            config.redis.url.as_str(),
            "config.toml",
            "http://",
            "postgres://",
        ] {
            assert!(!text.contains(leaked), "{text} leaks {leaked}");
        }
    }

    #[test]
    fn converts_not_found_to_git_404_marker() {
        let err: GitError = MegaError::NotFound("repo missing".to_owned()).into();

        assert!(err.to_string().contains("[code:404] repo missing"));
    }

    #[test]
    fn protocol_error_returns_expected_http_status() {
        let response = ProtocolError::NotFound("repo missing".to_owned()).into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn materialize_aborted_maps_to_advertise_5xx() {
        let mapped: ProtocolError = MegaError::MaterializeAborted.into();
        assert!(matches!(mapped, ProtocolError::AdvertiseFailed));
        let response = mapped.into_response();
        assert!(response.status().is_server_error());
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let parse: ProtocolError = MegaError::Other("bad pkt-line".to_owned()).into();
        let parse_response = parse.into_response();
        assert_eq!(parse_response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn print_does_not_panic() {
        MegaError::Other("print smoke".to_owned()).print();
    }

    #[test]
    fn db_err_detects_deadlock_message() {
        let err = sea_orm::DbErr::Custom(
            "deadlock detected\nCONTEXT: while inserting index tuple in relation \"git_blob\""
                .into(),
        );
        assert!(db_err_is_retryable_serialization(&err));
        assert!(MegaError::Db(err).is_retryable_db_serialization());
    }

    #[test]
    fn db_err_detects_sqlstate_40p01() {
        let err = sea_orm::DbErr::Custom("ERROR: 40P01 deadlock detected".into());
        assert!(db_err_is_retryable_serialization(&err));
    }

    #[test]
    fn db_err_detects_sqlstate_40001() {
        let err = sea_orm::DbErr::Custom("ERROR: 40001 could not serialize access".into());
        assert!(db_err_is_retryable_serialization(&err));
        assert!(MegaError::Db(err).is_retryable_db_serialization());
    }

    #[test]
    fn db_err_detects_serialization_failure_wording() {
        let err = sea_orm::DbErr::Custom("ERROR: serialization failure".into());
        assert!(db_err_is_retryable_serialization(&err));
    }

    #[test]
    fn db_err_ignores_unrelated_unique_violations() {
        let err = sea_orm::DbErr::Custom(
            "duplicate key value violates unique constraint \"git_repo_pkey\"".into(),
        );
        assert!(!db_err_is_retryable_serialization(&err));
        assert!(!MegaError::Db(err).is_retryable_db_serialization());
    }

    #[test]
    fn non_db_errors_use_message_classifier() {
        assert!(MegaError::Other("deadlock detected".into()).is_retryable_db_serialization());
        assert!(!MegaError::Other("duplicate key value".into()).is_retryable_db_serialization());
        assert!(!MegaError::NotFound("missing".into()).is_retryable_db_serialization());
    }
}
