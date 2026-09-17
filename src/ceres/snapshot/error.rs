//! Typed errors for the MST/2 snapshot surface (spec 14). Every rejection is
//! explicit; nothing may degrade into an empty result or ENOENT.

use crate::common::errors::MegaError;

/// Snapshot-domain error codes surfaced as JSON `{"error": {"code": ...}}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotErrorCode {
    ScopeInvalid,
    ScopeForbidden,
    ViewNotFound,
    SnapshotNotReady,
    SnapshotUnknown,
    PathNotFound,
    NotDirectory,
    UnsupportedEntry,
    LeaseUnknown,
    LeaseExpired,
    /// Publication (T05): expected-old CAS, read-set predicate or writer
    /// epoch no longer holds; the writer must refetch and retry.
    Conflict,
    CursorInvalid,
    CursorStale,
    ProofBudgetExceeded,
    DigestMismatch,
    RangeNotSupported,
    SymlinkTraversal,
    Internal,
}

impl SnapshotErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapshotErrorCode::ScopeInvalid => "SCOPE_INVALID",
            SnapshotErrorCode::ScopeForbidden => "SCOPE_FORBIDDEN",
            SnapshotErrorCode::ViewNotFound => "VIEW_NOT_FOUND",
            SnapshotErrorCode::SnapshotNotReady => "SNAPSHOT_NOT_READY",
            SnapshotErrorCode::SnapshotUnknown => "SNAPSHOT_UNKNOWN",
            SnapshotErrorCode::PathNotFound => "PATH_NOT_FOUND",
            SnapshotErrorCode::NotDirectory => "NOT_DIRECTORY",
            SnapshotErrorCode::UnsupportedEntry => "UNSUPPORTED_ENTRY",
            SnapshotErrorCode::LeaseUnknown => "LEASE_UNKNOWN",
            SnapshotErrorCode::LeaseExpired => "LEASE_EXPIRED",
            SnapshotErrorCode::Conflict => "CONFLICT",
            SnapshotErrorCode::CursorInvalid => "CURSOR_INVALID",
            SnapshotErrorCode::CursorStale => "CURSOR_STALE",
            SnapshotErrorCode::ProofBudgetExceeded => "PROOF_BUDGET_EXCEEDED",
            SnapshotErrorCode::DigestMismatch => "OBJECT_DIGEST_MISMATCH",
            SnapshotErrorCode::RangeNotSupported => "RANGE_NOT_SUPPORTED",
            SnapshotErrorCode::SymlinkTraversal => "SYMLINK_TRAVERSAL",
            SnapshotErrorCode::Internal => "INTERNAL",
        }
    }

    /// HTTP status for the JSON error envelope. Proven not-found outcomes
    /// stay 404; storage failures map to 5xx and never masquerade as absence.
    pub fn http_status(&self) -> u16 {
        match self {
            SnapshotErrorCode::ScopeInvalid
            | SnapshotErrorCode::CursorInvalid
            | SnapshotErrorCode::CursorStale => 400,
            SnapshotErrorCode::ScopeForbidden => 403,
            SnapshotErrorCode::ViewNotFound
            | SnapshotErrorCode::SnapshotUnknown
            | SnapshotErrorCode::PathNotFound
            | SnapshotErrorCode::LeaseUnknown => 404,
            SnapshotErrorCode::SnapshotNotReady => 503,
            SnapshotErrorCode::NotDirectory => 409,
            SnapshotErrorCode::UnsupportedEntry => 422,
            SnapshotErrorCode::LeaseExpired => 410,
            SnapshotErrorCode::Conflict => 409,
            SnapshotErrorCode::ProofBudgetExceeded => 413,
            SnapshotErrorCode::DigestMismatch => 409,
            SnapshotErrorCode::RangeNotSupported => 400,
            SnapshotErrorCode::SymlinkTraversal => 400,
            SnapshotErrorCode::Internal => 500,
        }
    }
}

#[derive(Debug)]
pub struct SnapshotError {
    pub code: SnapshotErrorCode,
    pub message: String,
}

impl SnapshotError {
    pub fn new(code: SnapshotErrorCode, message: impl Into<String>) -> Self {
        SnapshotError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for SnapshotError {}

impl From<SnapshotError> for MegaError {
    fn from(e: SnapshotError) -> Self {
        MegaError::Other(e.to_string())
    }
}
