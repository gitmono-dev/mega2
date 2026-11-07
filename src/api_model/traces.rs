//! Wire filter for session and checkpoint capture queries (MTR-53).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::TracesProtocolLimits;

/// Fixed protocol budget fields for the capabilities response (MTR-84).
/// These values come only from `TracesProtocolLimits`, never from operator TOML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TracesCapabilityLimits {
    pub max_object_bytes: u64,
    pub max_metadata_bytes: u64,
    pub max_manifest_bytes: u64,
    pub max_pack_bytes: u64,
    pub max_decoded_bytes: u64,
    pub max_objects: u64,
    pub max_new_commits: u64,
    pub max_delta_depth: u64,
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
        }
    }
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
}
