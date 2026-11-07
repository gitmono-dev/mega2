//! Wire filter for session and checkpoint capture queries (MTR-53).

use chrono::{DateTime, Utc};

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
}
