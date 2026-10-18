//! Traces error contract used by later HTTP and Git callers.
//!
//! The typed variants and the single status table live in
//! [`crate::common::errors::TracesError`]. This module does not keep a second
//! mapping.

use axum::http::StatusCode;

use crate::common::errors::{MegaError, ProtocolError, TracesError};

/// HTTP status and stable code from the central traces table.
pub fn http_contract(err: &TracesError) -> (StatusCode, &'static str) {
    (err.http_status(), err.code())
}

/// Git transport error. The traces class is preserved.
pub fn git_contract(err: TracesError) -> ProtocolError {
    MegaError::from(err).into()
}

/// Public error text. Object keys and transcript bytes are not inputs.
pub fn payload_contract(err: &TracesError) -> String {
    err.public_message()
}

/// Integrity failure. The object key and transcript are dropped.
pub fn object_integrity_error(object_key: &str, transcript: &str) -> TracesError {
    let err = TracesError::ObjectIntegrity;
    debug_assert!(
        object_key.is_empty() || !payload_contract(&err).contains(object_key),
        "object key must not enter the traces error"
    );
    debug_assert!(
        transcript.is_empty() || !payload_contract(&err).contains(transcript),
        "transcript text must not enter the traces error"
    );
    err
}

/// Search outage. A path or sentence is replaced by the stable fallback token.
pub fn search_unavailable_error(reason: &str) -> TracesError {
    TracesError::search_unavailable(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<TracesError> {
        vec![
            TracesError::Unauthenticated,
            TracesError::UnknownScope,
            TracesError::InvalidParameter,
            TracesError::InvalidCursor,
            TracesError::UnsupportedSchema,
            TracesError::InvalidCapture,
            object_integrity_error(
                "traces/v1/stream/objects/deadbeef",
                "secret transcript line",
            ),
            TracesError::ResponseBudget,
            TracesError::QueryTimeout,
            search_unavailable_error("traces/v1/stream/objects/deadbeef secret transcript line"),
            TracesError::TraceHistoryInCode,
        ]
    }

    #[test]
    fn traces_error_contract_git_keeps_each_class() {
        for err in samples() {
            let code = err.code();
            let payload = payload_contract(&err);
            let protocol = git_contract(err);
            assert!(
                matches!(protocol, ProtocolError::Traces(_)),
                "git mapping collapsed {code} into {}",
                protocol
            );
            assert_eq!(protocol.to_string(), payload);
            assert!(protocol.to_string().contains(code));
            assert!(!protocol.to_string().starts_with("Invalid Input"));
        }
    }

    #[test]
    fn traces_error_contract_http_uses_central_status() {
        let timeout = TracesError::QueryTimeout;
        assert_eq!(
            http_contract(&timeout),
            (StatusCode::SERVICE_UNAVAILABLE, "query_timeout")
        );
        let schema = TracesError::UnsupportedSchema;
        assert_eq!(
            http_contract(&schema),
            (StatusCode::CONFLICT, "unsupported_schema")
        );
        let history = TracesError::TraceHistoryInCode;
        assert_eq!(http_contract(&history).1, "trace_history_in_code");
        assert_eq!(payload_contract(&history), "trace_history_in_code");
    }

    #[test]
    fn traces_error_contract_payload_omits_key_and_text() {
        let object_key = "traces/v1/stream/objects/deadbeef";
        let transcript = "secret transcript line";
        let integrity = object_integrity_error(object_key, transcript);
        let search = search_unavailable_error(&format!("{object_key} {transcript}"));
        for err in [integrity, search] {
            let text = payload_contract(&err);
            assert!(!text.contains("deadbeef"), "{text}");
            assert!(!text.contains(transcript), "{text}");
            assert!(!text.contains(object_key), "{text}");
        }
    }
}
