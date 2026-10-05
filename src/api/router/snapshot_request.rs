//! Bounded raw JSON input for the MST/2 POST surface (spec 14).

use std::time::Duration;

use axum::{
    extract::{FromRequest, Request},
    http::StatusCode,
};
use bytes::Bytes;

use crate::ceres::snapshot::error::{SnapshotError, SnapshotErrorCode};

/// One overall read deadline, including a body which keeps trickling bytes.
/// This bounds input collection only, not handler work or response streams.
pub(super) const JSON_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Preserve the original bytes for TreeFrame request-body digests. The router
/// supplies DefaultBodyLimit; its rejection is converted to the MST envelope.
pub(super) struct Mst2Bytes(pub(super) Bytes);

impl<S> FromRequest<S> for Mst2Bytes
where
    S: Send + Sync,
{
    type Rejection = SnapshotError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match tokio::time::timeout(JSON_REQUEST_TIMEOUT, Bytes::from_request(req, state)).await {
            Ok(Ok(body)) => Ok(Self(body)),
            Ok(Err(error)) => {
                let (code, message) = if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    (
                        SnapshotErrorCode::LimitExceeded,
                        "request body over the spec 14 limit",
                    )
                } else {
                    (
                        SnapshotErrorCode::InvalidRequest,
                        "could not read request body",
                    )
                };
                Err(SnapshotError::new(code, message))
            }
            Err(_) => Err(SnapshotError::new(
                SnapshotErrorCode::TemporaryUnavailable,
                "request body read deadline exceeded",
            )),
        }
    }
}
