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

#[derive(Error, Debug)]
pub enum StatusParseError {
    #[error("Unexpected line format: {0}")]
    UnexpectedFormat(String),
    #[error("Unknown line prefix: {0}")]
    UnknownPrefix(String),
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
}

impl From<MegaError> for ProtocolError {
    fn from(err: MegaError) -> ProtocolError {
        ProtocolError::InvalidInput(err.to_string())
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
    fn print_does_not_panic() {
        MegaError::Other("print smoke".to_owned()).print();
    }
}
