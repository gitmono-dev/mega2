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

