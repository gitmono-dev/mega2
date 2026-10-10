// Source: src/api/router/snapshot_router.rs at ac7acfdba08a427bc295be71ec5f2f465a515e32
// File SHA-256: d649130633f9b468c1154974dd542a694b2e7a1f7bbc8f4d1cf5e26279b2d2f9
// Excerpt lines: 497-521
// Exact capture: evidence/verification/production-mapping-base.stdout (exit 0)
fn mst2_error_response(err: SnapshotError) -> Response {
    let status =
        StatusCode::from_u16(err.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(json!({
            "error": {
                "code": err.code.as_str(),
                "message": err.message,
                "request_id": current_request_id(),
                "retryable": matches!(
                    err.code,
                    SnapshotErrorCode::SnapshotNotReady
                        | SnapshotErrorCode::MetadataNotReady
                        | SnapshotErrorCode::TemporaryUnavailable
                        | SnapshotErrorCode::Internal
                ),
            }
        })),
    )
        .into_response()
}

impl From<SnapshotError> for Response {
    fn from(err: SnapshotError) -> Self {
