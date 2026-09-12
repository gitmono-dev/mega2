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
            MegaError::ObjStorageNotFound(msg) => {
                GitError::CustomError(format!("[code:404] ObjStorage not found: {msg}"))
            }
            other => GitError::CustomError(other.to_string()),
        }
    }
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
