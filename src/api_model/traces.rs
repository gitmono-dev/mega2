//! Wire filter for session and checkpoint capture queries (MTR-53).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::config::TracesProtocolLimits;

pub const TRACES_PROTOCOL_ID: &str = "libra-traces/v1";

/// Fixed protocol budget fields for the capabilities response (MTR-84).
/// These values come only from `TracesProtocolLimits`, never from operator TOML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TracesCapabilityLimits {
    pub max_object_bytes: u64,
    pub max_metadata_bytes: u64,
    pub max_manifest_bytes: u64,
    pub max_pack_bytes: u64,
    pub max_decoded_bytes: u64,
    pub max_objects: u64,
    pub max_new_commits: u64,
    pub max_delta_depth: u64,
    pub max_tree_depth: u64,
    pub ingest_timeout_seconds: u64,
    pub metadata_limit_default: u64,
    pub search_limit_default: u64,
    pub text_bytes_default: u64,
    pub metadata_limit_max: u64,
    pub search_limit_max: u64,
    pub text_bytes_max: u64,
    pub query_bytes_min: u64,
    pub query_bytes_max: u64,
    pub response_bytes_max: u64,
    pub candidate_fragments_max: u64,
    pub scan_bytes_max: u64,
    pub query_timeout_ms: u64,
}

impl TracesCapabilityLimits {
    pub fn from_protocol_limits() -> Self {
        Self {
            max_object_bytes: TracesProtocolLimits::MAX_OBJECT_BYTES,
            max_metadata_bytes: TracesProtocolLimits::MAX_METADATA_BYTES,
            max_manifest_bytes: TracesProtocolLimits::MAX_MANIFEST_BYTES,
            max_pack_bytes: TracesProtocolLimits::MAX_PACK_BYTES,
            max_decoded_bytes: TracesProtocolLimits::MAX_DECODED_BYTES,
            max_objects: TracesProtocolLimits::MAX_OBJECTS,
            max_new_commits: TracesProtocolLimits::MAX_NEW_COMMITS,
            max_delta_depth: TracesProtocolLimits::MAX_DELTA_DEPTH,
            max_tree_depth: TracesProtocolLimits::MAX_TREE_DEPTH,
            ingest_timeout_seconds: TracesProtocolLimits::INGEST_TIMEOUT_SECONDS,
            metadata_limit_default: TracesProtocolLimits::METADATA_LIMIT_DEFAULT,
            search_limit_default: TracesProtocolLimits::SEARCH_LIMIT_DEFAULT,
            text_bytes_default: TracesProtocolLimits::TEXT_BYTES_DEFAULT,
            metadata_limit_max: TracesProtocolLimits::METADATA_LIMIT_MAX,
            search_limit_max: TracesProtocolLimits::SEARCH_LIMIT_MAX,
            text_bytes_max: TracesProtocolLimits::TEXT_BYTES_MAX,
            query_bytes_min: TracesProtocolLimits::QUERY_BYTES_MIN,
            query_bytes_max: TracesProtocolLimits::QUERY_BYTES_MAX,
            response_bytes_max: TracesProtocolLimits::RESPONSE_BYTES_MAX,
            candidate_fragments_max: TracesProtocolLimits::CANDIDATE_FRAGMENTS_MAX,
            scan_bytes_max: TracesProtocolLimits::SCAN_BYTES_MAX,
            query_timeout_ms: TracesProtocolLimits::QUERY_TIMEOUT_MS,
        }
    }
}

/// Authenticated `GET /api/v1/traces/capabilities` body (MTR-34).
/// `limits` is only `TracesCapabilityLimits::from_protocol_limits`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TracesCapabilities {
    pub protocol: String,
    pub server_id: String,
    pub git_transports: Vec<String>,
    pub object_formats: Vec<String>,
    pub api_version: String,
    pub schema_version: u64,
    pub index_version: u64,
    pub limits: TracesCapabilityLimits,
    pub search_ready: bool,
    pub cjk_index_supported: bool,
    pub bounded_scan_supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_unavailable_reason: Option<String>,
}

impl TracesCapabilities {
    pub fn project(
        server_id: impl Into<String>,
        search_ready: bool,
        reason: Option<String>,
    ) -> Self {
        Self {
            protocol: TRACES_PROTOCOL_ID.to_owned(),
            server_id: server_id.into(),
            git_transports: vec!["http".to_owned(), "ssh".to_owned()],
            object_formats: vec!["sha1".to_owned(), "sha256".to_owned()],
            api_version: "v1".to_owned(),
            schema_version: 1,
            index_version: 1,
            limits: TracesCapabilityLimits::from_protocol_limits(),
            search_ready,
            cjk_index_supported: false,
            bounded_scan_supported: false,
            search_unavailable_reason: if search_ready { None } else { reason },
        }
    }
}

/// `GET /api/v1/traces/targets/resolve` body (MTR-88).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TargetResolve {
    pub target_id: String,
    pub project_path: String,
    pub target_kind: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TargetResolveQuery {
    pub path: String,
}

/// `GET /api/v1/traces/targets/{target_id}/streams` (MTR-55).
pub const STREAM_DIRECTORY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StreamDirectory {
    pub schema_version: u32,
    pub items: Vec<StreamDirectoryItem>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StreamDirectoryItem {
    pub stream_id: String,
    pub target_id: String,
    pub owner: String,
    pub source: String,
    pub epoch: String,
    pub hash_kind: String,
    pub accepted_tip: Option<String>,
    pub indexed_tip: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamDirectoryQuery {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub epoch: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Admitted stream-directory query. `limit` is the protocol default or an in-range value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDirectoryRequest {
    pub source: Option<String>,
    pub owner: Option<String>,
    pub epoch: Option<String>,
    pub limit: u64,
    pub after_id: Option<String>,
}

fn present_filter(
    value: Option<String>,
) -> Result<Option<String>, crate::common::errors::TracesError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() || value.chars().all(char::is_whitespace) {
        return Err(crate::common::errors::TracesError::InvalidParameter);
    }
    Ok(Some(value))
}

fn filter_key(
    target_id: &str,
    source: &Option<String>,
    owner: &Option<String>,
    epoch: &Option<String>,
) -> String {
    format!(
        "{target_id}\n{}\n{}\n{}",
        source.as_deref().unwrap_or(""),
        owner.as_deref().unwrap_or(""),
        epoch.as_deref().unwrap_or("")
    )
}

fn cursor_binding(target_id: &str, request: &StreamDirectoryRequest) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        filter_key(target_id, &request.source, &request.owner, &request.epoch).as_bytes(),
    );
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Keyset cursor bound to the target and the current filters.
pub fn stream_directory_cursor(
    target_id: &str,
    request: &StreamDirectoryRequest,
    after_id: &str,
) -> String {
    format!("v1.{}.{}", cursor_binding(target_id, request), after_id)
}

/// Apply the fixed metadata page bounds and decode a bound cursor.
pub fn admit_stream_directory_query(
    target_id: &str,
    query: StreamDirectoryQuery,
) -> Result<StreamDirectoryRequest, crate::common::errors::TracesError> {
    use crate::common::errors::TracesError;
    let source = present_filter(query.source)?;
    let owner = present_filter(query.owner)?;
    let epoch = present_filter(query.epoch)?;
    let limit = match query.limit {
        None => TracesProtocolLimits::METADATA_LIMIT_DEFAULT,
        Some(limit) if (1..=TracesProtocolLimits::METADATA_LIMIT_MAX).contains(&limit) => limit,
        Some(_) => return Err(TracesError::InvalidParameter),
    };
    let request = StreamDirectoryRequest {
        source,
        owner,
        epoch,
        limit,
        after_id: None,
    };
    let after_id = match query.cursor.as_deref() {
        None => None,
        Some(cursor) if cursor.is_empty() || cursor.chars().all(char::is_whitespace) => {
            return Err(TracesError::InvalidCursor);
        }
        Some(cursor) => {
            let rest = cursor
                .strip_prefix("v1.")
                .ok_or(TracesError::InvalidCursor)?;
            let (binding, after_id) = rest.split_once('.').ok_or(TracesError::InvalidCursor)?;
            if binding != cursor_binding(target_id, &request) || after_id.is_empty() {
                return Err(TracesError::InvalidCursor);
            }
            Some(after_id.to_owned())
        }
    };
    Ok(StreamDirectoryRequest {
        after_id,
        ..request
    })
}

/// Errors returned when the wire filter itself is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceFilterError {
    InvalidSince,
    InvalidUntil,
    ReverseInterval,
}

impl std::fmt::Display for TraceFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::InvalidSince => "since must be RFC3339 with a timezone",
            Self::InvalidUntil => "until must be RFC3339 with a timezone",
            Self::ReverseInterval => "since must not be later than until",
        };
        f.write_str(text)
    }
}

impl std::error::Error for TraceFilterError {}

/// `[since, until)` in UTC. Equal bounds are an empty interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTimeWindow {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

impl CaptureTimeWindow {
    pub fn contains(&self, time: DateTime<Utc>) -> bool {
        self.since.is_none_or(|since| time >= since) && self.until.is_none_or(|until| time < until)
    }
}

/// Filter parsed from the wire. `agent` and `code_commit` are literal values,
/// not aliases for other fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureQueryFilter {
    pub agent: Option<String>,
    pub code_commit: Option<String>,
    pub window: CaptureTimeWindow,
}

impl CaptureQueryFilter {
    pub fn new(
        agent: Option<String>,
        code_commit: Option<String>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> Result<Self, TraceFilterError> {
        let since = since
            .map(|text| DateTime::parse_from_rfc3339(text).map(|value| value.with_timezone(&Utc)))
            .transpose()
            .map_err(|_| TraceFilterError::InvalidSince)?;
        let until = until
            .map(|text| DateTime::parse_from_rfc3339(text).map(|value| value.with_timezone(&Utc)))
            .transpose()
            .map_err(|_| TraceFilterError::InvalidUntil)?;
        if let (Some(since), Some(until)) = (since, until)
            && since > until
        {
            return Err(TraceFilterError::ReverseInterval);
        }
        Ok(Self {
            agent,
            code_commit,
            window: CaptureTimeWindow { since, until },
        })
    }

    pub fn matches_agent(&self, agent_kind: &str) -> bool {
        self.agent
            .as_deref()
            .is_none_or(|agent| agent == agent_kind)
    }

    pub fn matches_code_commit(&self, code_commit: Option<&str>) -> bool {
        self.code_commit
            .as_deref()
            .is_none_or(|expected| Some(expected) == code_commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_filter_wire_defaults_and_cursor_binding() {
        use crate::common::errors::TracesError;
        let admitted = admit_stream_directory_query(
            "target",
            StreamDirectoryQuery {
                source: None,
                owner: None,
                epoch: None,
                limit: None,
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(admitted.limit, TracesProtocolLimits::METADATA_LIMIT_DEFAULT);
        assert!(admitted.after_id.is_none());
        let max = admit_stream_directory_query(
            "target",
            StreamDirectoryQuery {
                source: Some("repo".to_owned()),
                owner: Some("owner".to_owned()),
                epoch: Some("epoch".to_owned()),
                limit: Some(TracesProtocolLimits::METADATA_LIMIT_MAX),
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(max.limit, TracesProtocolLimits::METADATA_LIMIT_MAX);
        assert!(
            admit_stream_directory_query(
                "target",
                StreamDirectoryQuery {
                    source: None,
                    owner: None,
                    epoch: None,
                    limit: Some(TracesProtocolLimits::METADATA_LIMIT_MAX + 1),
                    cursor: None,
                },
            )
            .is_err()
        );
        assert!(matches!(
            admit_stream_directory_query(
                "target",
                StreamDirectoryQuery {
                    source: Some(" ".to_owned()),
                    owner: None,
                    epoch: None,
                    limit: None,
                    cursor: None,
                },
            ),
            Err(TracesError::InvalidParameter)
        ));
        let cursor = stream_directory_cursor("target", &max, "stream-1");
        let page = admit_stream_directory_query(
            "target",
            StreamDirectoryQuery {
                source: Some("repo".to_owned()),
                owner: Some("owner".to_owned()),
                epoch: Some("epoch".to_owned()),
                limit: Some(TracesProtocolLimits::METADATA_LIMIT_MAX),
                cursor: Some(cursor),
            },
        )
        .unwrap();
        assert_eq!(page.after_id.as_deref(), Some("stream-1"));
        assert!(matches!(
            admit_stream_directory_query(
                "target",
                StreamDirectoryQuery {
                    source: Some("other".to_owned()),
                    owner: Some("owner".to_owned()),
                    epoch: Some("epoch".to_owned()),
                    limit: Some(10),
                    cursor: Some(stream_directory_cursor("target", &max, "stream-1")),
                },
            ),
            Err(TracesError::InvalidCursor)
        ));
        let body = StreamDirectory {
            schema_version: STREAM_DIRECTORY_SCHEMA_VERSION,
            items: vec![StreamDirectoryItem {
                stream_id: "stream-1".to_owned(),
                target_id: "target".to_owned(),
                owner: "owner".to_owned(),
                source: "repo".to_owned(),
                epoch: "epoch".to_owned(),
                hash_kind: "sha1".to_owned(),
                accepted_tip: Some("a".repeat(40)),
                indexed_tip: None,
                status: "index_pending".to_owned(),
            }],
            next_cursor: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert!(json["next_cursor"].is_null());
        assert_eq!(json["items"][0]["status"], "index_pending");
        let decoded: StreamDirectory = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn capture_query_filter_wire_parses_literals_and_window() {
        let filter = CaptureQueryFilter::new(
            Some("claude_code".to_owned()),
            Some("b".repeat(40)),
            Some("2026-01-01T00:00:00+08:00"),
            Some("2026-01-01T01:00:00Z"),
        )
        .unwrap();
        assert!(filter.matches_agent("claude_code"));
        assert!(!filter.matches_agent("other"));
        assert!(filter.matches_code_commit(Some(&"b".repeat(40))));
        assert!(!filter.matches_code_commit(Some(&"a".repeat(40))));
        assert_eq!(
            filter.window.since.map(|time| time.to_rfc3339()),
            Some("2025-12-31T16:00:00+00:00".to_owned())
        );
        let at_start = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(filter.window.contains(at_start));
        let at_end = DateTime::parse_from_rfc3339("2026-01-01T01:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert!(!filter.window.contains(at_end));
    }

    #[test]
    fn capture_query_filter_wire_rejects_bad_and_reverse_intervals() {
        assert_eq!(
            CaptureQueryFilter::new(None, None, Some("not-time"), None),
            Err(TraceFilterError::InvalidSince)
        );
        assert_eq!(
            CaptureQueryFilter::new(None, None, None, Some("not-time")),
            Err(TraceFilterError::InvalidUntil)
        );
        assert_eq!(
            CaptureQueryFilter::new(
                None,
                None,
                Some("2026-01-02T00:00:00Z"),
                Some("2026-01-01T00:00:00Z")
            ),
            Err(TraceFilterError::ReverseInterval)
        );
        let empty = CaptureQueryFilter::new(
            None,
            None,
            Some("2026-01-01T00:00:00Z"),
            Some("2026-01-01T00:00:00Z"),
        )
        .unwrap();
        assert!(!empty.window.contains(Utc::now()));
    }

    #[test]
    fn capability_limits_84_projects_every_protocol_scalar() {
        let limits = TracesCapabilityLimits::from_protocol_limits();
        assert_eq!(
            limits.max_object_bytes,
            TracesProtocolLimits::MAX_OBJECT_BYTES
        );
        assert_eq!(
            limits.max_metadata_bytes,
            TracesProtocolLimits::MAX_METADATA_BYTES
        );
        assert_eq!(
            limits.max_manifest_bytes,
            TracesProtocolLimits::MAX_MANIFEST_BYTES
        );
        assert_eq!(limits.max_pack_bytes, TracesProtocolLimits::MAX_PACK_BYTES);
        assert_eq!(
            limits.max_decoded_bytes,
            TracesProtocolLimits::MAX_DECODED_BYTES
        );
        assert_eq!(limits.max_objects, TracesProtocolLimits::MAX_OBJECTS);
        assert_eq!(
            limits.max_new_commits,
            TracesProtocolLimits::MAX_NEW_COMMITS
        );
        assert_eq!(
            limits.max_delta_depth,
            TracesProtocolLimits::MAX_DELTA_DEPTH
        );
        let json = serde_json::to_value(&limits).unwrap();
        assert_eq!(json["max_object_bytes"], 67_108_864);
        assert_eq!(json["max_metadata_bytes"], 1_048_576);
        assert_eq!(json["max_manifest_bytes"], 1_048_576);
        assert_eq!(json["max_pack_bytes"], 536_870_912);
        assert_eq!(json["max_decoded_bytes"], 1_073_741_824);
        assert_eq!(json["max_objects"], 100_000);
        assert_eq!(json["max_new_commits"], 10_000);
        assert_eq!(json["max_delta_depth"], 64);
        let decoded: TracesCapabilityLimits = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, limits);
    }

    #[test]
    fn capability_limits_85_projects_timeout_and_limit_scalars() {
        let limits = TracesCapabilityLimits::from_protocol_limits();
        assert_eq!(limits.max_tree_depth, TracesProtocolLimits::MAX_TREE_DEPTH);
        assert_eq!(
            limits.ingest_timeout_seconds,
            TracesProtocolLimits::INGEST_TIMEOUT_SECONDS
        );
        assert_eq!(
            limits.metadata_limit_default,
            TracesProtocolLimits::METADATA_LIMIT_DEFAULT
        );
        assert_eq!(
            limits.search_limit_default,
            TracesProtocolLimits::SEARCH_LIMIT_DEFAULT
        );
        assert_eq!(
            limits.text_bytes_default,
            TracesProtocolLimits::TEXT_BYTES_DEFAULT
        );
        assert_eq!(
            limits.metadata_limit_max,
            TracesProtocolLimits::METADATA_LIMIT_MAX
        );
        assert_eq!(
            limits.search_limit_max,
            TracesProtocolLimits::SEARCH_LIMIT_MAX
        );
        assert_eq!(limits.text_bytes_max, TracesProtocolLimits::TEXT_BYTES_MAX);
        let json = serde_json::to_value(&limits).unwrap();
        assert_eq!(json["max_tree_depth"], 64);
        assert_eq!(json["ingest_timeout_seconds"], 300);
        assert_eq!(json["metadata_limit_default"], 50);
        assert_eq!(json["search_limit_default"], 20);
        assert_eq!(json["text_bytes_default"], 16_384);
        assert_eq!(json["metadata_limit_max"], 200);
        assert_eq!(json["search_limit_max"], 100);
        assert_eq!(json["text_bytes_max"], 131_072);
        let decoded: TracesCapabilityLimits = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, limits);
    }

    #[test]
    fn capability_limits_86_projects_query_and_scan_scalars() {
        let limits = TracesCapabilityLimits::from_protocol_limits();
        assert_eq!(
            limits.query_bytes_min,
            TracesProtocolLimits::QUERY_BYTES_MIN
        );
        assert_eq!(
            limits.query_bytes_max,
            TracesProtocolLimits::QUERY_BYTES_MAX
        );
        assert_eq!(
            limits.response_bytes_max,
            TracesProtocolLimits::RESPONSE_BYTES_MAX
        );
        assert_eq!(
            limits.candidate_fragments_max,
            TracesProtocolLimits::CANDIDATE_FRAGMENTS_MAX
        );
        assert_eq!(limits.scan_bytes_max, TracesProtocolLimits::SCAN_BYTES_MAX);
        assert_eq!(
            limits.query_timeout_ms,
            TracesProtocolLimits::QUERY_TIMEOUT_MS
        );
        let json = serde_json::to_value(&limits).unwrap();
        assert_eq!(json["query_bytes_min"], 1);
        assert_eq!(json["query_bytes_max"], 256);
        assert_eq!(json["response_bytes_max"], 1_048_576);
        assert_eq!(json["candidate_fragments_max"], 2_000);
        assert_eq!(json["scan_bytes_max"], 8_388_608);
        assert_eq!(json["query_timeout_ms"], 2_000);
        let decoded: TracesCapabilityLimits = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, limits);
    }

    #[test]
    fn capabilities_limits_wire_uses_the_protocol_mapper_and_openapi_schema() {
        let projected = TracesCapabilities::project(
            "11111111-1111-4111-8111-111111111111",
            false,
            Some("index_missing".to_owned()),
        );
        assert_eq!(projected.protocol, TRACES_PROTOCOL_ID);
        assert_eq!(
            projected.limits,
            TracesCapabilityLimits::from_protocol_limits()
        );
        let json = serde_json::to_value(&projected).unwrap();
        assert_eq!(
            json["limits"],
            serde_json::to_value(TracesCapabilityLimits::from_protocol_limits()).unwrap()
        );
        assert_eq!(json["git_transports"], serde_json::json!(["http", "ssh"]));
        assert_eq!(
            json["object_formats"],
            serde_json::json!(["sha1", "sha256"])
        );
        assert_eq!(json["search_unavailable_reason"], "index_missing");

        let doc = crate::server::http_server::storage_only_openapi_doc_with_traces();
        let components = doc.components.expect("components");
        let limits_schema = components
            .schemas
            .get("TracesCapabilityLimits")
            .expect("limits schema");
        let parent = components
            .schemas
            .get("TracesCapabilities")
            .expect("capabilities schema");
        let parent_json = serde_json::to_value(parent).unwrap();
        let limits_ref = parent_json["properties"]["limits"].to_string();
        assert!(
            limits_ref.contains("TracesCapabilityLimits"),
            "{limits_ref}"
        );
        let _ = limits_schema;
    }

    #[test]
    fn target_resolve_wire_round_trips_the_registered_identity() {
        let body = TargetResolve {
            target_id: "11111111-1111-4111-8111-111111111111".to_owned(),
            project_path: "/project/app".to_owned(),
            target_kind: "native".to_owned(),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["target_id"], body.target_id);
        assert_eq!(json["project_path"], "/project/app");
        assert_eq!(json["target_kind"], "native");
        let decoded: TargetResolve = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, body);
    }
}
