use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{ceres::api_service::mono_api_service::MonoServiceLogic, common::errors::MegaError};

pub const DEFAULT_PAGE_LIMIT: u32 = 50;
pub const MAX_PAGE_LIMIT: u32 = 200;

pub const CODE_UNAUTHORIZED: &str = "unauthorized";
pub const CODE_NOT_FOUND: &str = "not_found";
pub const CODE_CONFLICT: &str = "conflict";
pub const CODE_BAD_REQUEST: &str = "bad_request";
pub const CODE_PAYLOAD_TOO_LARGE: &str = "payload_too_large";

pub const BEARER_PREFIX: &str = "Bearer ";

const SESSION_PUT_STRIP_KEYS: &[&str] = &[
    "completeness",
    "partial_reason",
    "created_at",
    "updated_at",
    "capture_id",
    "lifecycle",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    ExternalCapture,
    InternalCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Empty,
    Incomplete,
    Complete,
    Truncated,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionView {
    pub capture_id: i64,
    pub client_session_id: String,
    pub tenant_id: String,
    pub deployment_id: String,
    pub repo_id: String,
    pub producer_id: String,
    pub session_kind: SessionKind,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub completeness: Completeness,
    pub partial_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionPutRequest {
    pub session_kind: SessionKind,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryResponse {
    pub raw_accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

impl ErrorEnvelope {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: ErrorBody {
                code: code.into(),
                message: message.into(),
            },
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(CODE_UNAUTHORIZED, "invalid ingest token")
    }

    pub fn not_found() -> Self {
        Self::new(CODE_NOT_FOUND, "not found")
    }

    pub fn conflict() -> Self {
        Self::new(CODE_CONFLICT, "conflict")
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(CODE_BAD_REQUEST, message)
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(CODE_PAYLOAD_TOO_LARGE, message)
    }
}

#[derive(Debug, Clone)]
pub struct AgentCaptureHttpError {
    pub status: StatusCode,
    pub envelope: ErrorEnvelope,
}

impl AgentCaptureHttpError {
    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            envelope: ErrorEnvelope::unauthorized(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            envelope: ErrorEnvelope::not_found(),
        }
    }

    pub fn conflict() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            envelope: ErrorEnvelope::conflict(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            envelope: ErrorEnvelope::bad_request(message),
        }
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            envelope: ErrorEnvelope::payload_too_large(message),
        }
    }
}

impl IntoResponse for AgentCaptureHttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.envelope)).into_response()
    }
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct PaginationQuery {
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

impl PaginationQuery {
    pub fn effective_limit(&self) -> u32 {
        self.limit.unwrap_or(DEFAULT_PAGE_LIMIT).min(MAX_PAGE_LIMIT)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageEnvelope<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

pub fn fingerprint_json(json_text: &str) -> Result<String, MegaError> {
    crate::common::canonical_json::fingerprint(json_text)
}

pub fn fingerprint_session_put_body(json_text: &str) -> Result<String, MegaError> {
    let canonical = crate::common::canonical_json::canonicalize(json_text)?;
    let mut value: serde_json::Value = serde_json::from_str(&canonical)?;
    if let Some(obj) = value.as_object_mut() {
        for key in SESSION_PUT_STRIP_KEYS {
            obj.remove(*key);
        }
    }
    let stripped = serde_json::to_string(&value)?;
    crate::common::canonical_json::fingerprint(&stripped)
}

pub fn decode_repo_path_segment(segment: &str) -> Result<String, MegaError> {
    let decoded = percent_decode_str(segment)
        .decode_utf8()
        .map_err(|err| MegaError::Other(format!("invalid repo path encoding: {err}")))?;
    MonoServiceLogic::normalize_repo_path(&decoded)
}

pub fn bearer_token(authorization: Option<&str>) -> Result<&str, AgentCaptureHttpError> {
    let Some(value) = authorization else {
        return Err(AgentCaptureHttpError::unauthorized());
    };
    let Some(token) = value.strip_prefix(BEARER_PREFIX) else {
        return Err(AgentCaptureHttpError::unauthorized());
    };
    if token.is_empty() {
        return Err(AgentCaptureHttpError::unauthorized());
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample_session() -> SessionView {
        let ts = DateTime::parse_from_rfc3339("2026-09-13T00:00:00Z")
            .expect("ts")
            .with_timezone(&Utc);
        SessionView {
            capture_id: 7,
            client_session_id: "sess-1".into(),
            tenant_id: "default".into(),
            deployment_id: "default".into(),
            repo_id: "/third-part/mega".into(),
            producer_id: "hook".into(),
            session_kind: SessionKind::ExternalCapture,
            started_at: Some(ts),
            ended_at: None,
            completeness: Completeness::Empty,
            partial_reason: None,
            created_at: ts,
            updated_at: ts,
        }
    }

    #[test]
    fn agent_capture_schema() {
        let value = serde_json::to_value(sample_session()).expect("session json");
        for key in [
            "capture_id",
            "client_session_id",
            "tenant_id",
            "deployment_id",
            "repo_id",
            "producer_id",
            "session_kind",
            "started_at",
            "ended_at",
            "completeness",
            "partial_reason",
            "created_at",
            "updated_at",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert!(value.get("user_id").is_none());
        assert_eq!(value["session_kind"], "external_capture");
        assert_eq!(value["completeness"], "empty");

        let extra = serde_json::from_str::<SessionPutRequest>(
            r#"{"session_kind":"external_capture","user_id":"nope"}"#,
        );
        assert!(extra.is_err());

        let reordered_a = r#"{"session_kind":"external_capture","started_at":null}"#;
        let reordered_b = r#"{"started_at":null,"session_kind":"external_capture"}"#;
        let fp_a = fingerprint_json(reordered_a).expect("fp a");
        let fp_b = fingerprint_json(reordered_b).expect("fp b");
        assert_eq!(fp_a, fp_b);
        assert_eq!(
            fp_a,
            crate::common::canonical_json::fingerprint(reordered_a).expect("direct")
        );

        let with_lifecycle =
            r#"{"session_kind":"internal_code","completeness":"complete","lifecycle":"x"}"#;
        let without_lifecycle = r#"{"session_kind":"internal_code"}"#;
        assert_eq!(
            fingerprint_session_put_body(with_lifecycle).expect("strip"),
            fingerprint_session_put_body(without_lifecycle).expect("plain")
        );
        assert!(
            fingerprint_session_put_body(
                r#"{"session_kind":"external_capture","session_kind":"internal_code"}"#
            )
            .is_err()
        );

        assert_eq!(
            decode_repo_path_segment("third-part%2Fmega").expect("decode"),
            "/third-part/mega"
        );
        assert_eq!(
            decode_repo_path_segment("%2Fthird-part%2Fmega").expect("decode slash"),
            "/third-part/mega"
        );
    }

    #[test]
    fn error_envelope() {
        let envelope = ErrorEnvelope::unauthorized();
        let value = serde_json::to_value(&envelope).expect("json");
        assert_eq!(
            value,
            json!({
                "error": {
                    "code": "unauthorized",
                    "message": "invalid ingest token"
                }
            })
        );
        assert!(value.get("req_result").is_none());
        let text = serde_json::to_string(&envelope).expect("text");
        assert!(!text.contains("secret-ci"));
        assert_eq!(
            bearer_token(None).unwrap_err().status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            bearer_token(Some("Bearer ")).unwrap_err().status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            bearer_token(Some("Bearer secret-ci")).expect("token"),
            "secret-ci"
        );
        let unauthorized = AgentCaptureHttpError::unauthorized();
        assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);
        assert_eq!(unauthorized.envelope.error.code, CODE_UNAUTHORIZED);
    }

    #[test]
    fn pagination_envelope() {
        assert_eq!(
            PaginationQuery::default().effective_limit(),
            DEFAULT_PAGE_LIMIT
        );
        assert_eq!(
            PaginationQuery {
                limit: Some(200),
                cursor: None,
            }
            .effective_limit(),
            MAX_PAGE_LIMIT
        );
        assert_eq!(
            PaginationQuery {
                limit: Some(500),
                cursor: None,
            }
            .effective_limit(),
            MAX_PAGE_LIMIT
        );
        let page = PageEnvelope {
            items: vec![1, 2],
            next_cursor: None,
        };
        let value = serde_json::to_value(&page).expect("page json");
        assert_eq!(value["items"], json!([1, 2]));
        assert!(value["next_cursor"].is_null());
        let more = PageEnvelope {
            items: Vec::<u8>::new(),
            next_cursor: Some("abc".into()),
        };
        assert_eq!(
            serde_json::to_value(&more).expect("cursor")["next_cursor"],
            "abc"
        );
    }
}
