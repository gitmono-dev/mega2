//! Session and checkpoint query projection for MTR-53.

use chrono::{DateTime, Utc};

use crate::api_model::traces::CaptureQueryFilter;

/// One checkpoint with the metadata needed by the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedCapture {
    pub session_key: String,
    pub checkpoint_id: String,
    pub agent_kind: String,
    pub created_at: DateTime<Utc>,
    pub code_commit: Option<String>,
}

/// A session assembled from every matching checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProjection {
    pub session_key: String,
    pub checkpoints: Vec<ProjectedCapture>,
    /// Maximum `created_at` among matching checkpoints.
    pub capture_time: DateTime<Utc>,
}

pub fn capture_matches(capture: &ProjectedCapture, filter: &CaptureQueryFilter) -> bool {
    filter.matches_agent(&capture.agent_kind)
        && filter.matches_code_commit(capture.code_commit.as_deref())
        && filter.window.contains(capture.created_at)
}

pub fn filter_captures(
    captures: &[ProjectedCapture],
    filter: &CaptureQueryFilter,
) -> Vec<ProjectedCapture> {
    captures
        .iter()
        .filter(|capture| capture_matches(capture, filter))
        .cloned()
        .collect()
}

/// Group matching checkpoints by session and sort sessions by their newest
/// matching checkpoint, newest first. Session key breaks ties deterministically.
pub fn project_sessions(
    captures: &[ProjectedCapture],
    filter: &CaptureQueryFilter,
) -> Vec<SessionProjection> {
    let mut sessions = std::collections::BTreeMap::<String, Vec<ProjectedCapture>>::new();
    for capture in filter_captures(captures, filter) {
        sessions
            .entry(capture.session_key.clone())
            .or_default()
            .push(capture);
    }
    let mut projected = Vec::new();
    for (session_key, checkpoints) in sessions {
        let Some(capture_time) = checkpoints.iter().map(|capture| capture.created_at).max() else {
            continue;
        };
        projected.push(SessionProjection {
            session_key,
            checkpoints,
            capture_time,
        });
    }
    projected.sort_by(|left, right| {
        right
            .capture_time
            .cmp(&left.capture_time)
            .then_with(|| left.session_key.cmp(&right.session_key))
    });
    projected
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn capture(
        session: &str,
        checkpoint: &str,
        agent: &str,
        seconds: i64,
        code: &str,
    ) -> ProjectedCapture {
        ProjectedCapture {
            session_key: session.to_owned(),
            checkpoint_id: checkpoint.to_owned(),
            agent_kind: agent.to_owned(),
            created_at: Utc.timestamp_opt(seconds, 0).unwrap(),
            code_commit: Some(code.to_owned()),
        }
    }

    fn make_filter(
        agent: Option<&str>,
        code: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
    ) -> CaptureQueryFilter {
        CaptureQueryFilter::new(
            agent.map(str::to_owned),
            code.map(str::to_owned),
            since,
            until,
        )
        .unwrap()
    }

    #[test]
    fn capture_query_filters_match_agent_code_and_half_open_interval() {
        let rows = [
            capture("s1", "c1", "claude_code", 100, &"a".repeat(40)),
            capture("s1", "c2", "other", 200, &"b".repeat(40)),
            capture("s2", "c3", "claude_code", 300, &"b".repeat(40)),
        ];
        let filter = make_filter(Some("claude_code"), Some(&"b".repeat(40)), None, None);
        assert!(capture_matches(&rows[2], &filter));
        assert!(!capture_matches(&rows[0], &filter));
        assert!(!capture_matches(&rows[1], &filter));

        let window = make_filter(
            Some("claude_code"),
            None,
            Some("1970-01-01T00:01:40Z"),
            Some("1970-01-01T00:06:40Z"),
        );
        assert!(capture_matches(&rows[2], &window));
        assert!(!capture_matches(&rows[1], &window));
        let until_only = make_filter(
            Some("claude_code"),
            None,
            None,
            Some("1970-01-01T00:05:00Z"),
        );
        assert!(!capture_matches(&rows[2], &until_only));
    }

    #[test]
    fn capture_query_filters_include_session_by_any_checkpoint_and_sort_by_max_time() {
        let rows = [
            capture("older", "o1", "claude_code", 100, &"a".repeat(40)),
            capture("newer", "n1", "claude_code", 300, &"a".repeat(40)),
            capture("older", "o2", "claude_code", 200, &"a".repeat(40)),
            capture("excluded", "x1", "other", 50, &"a".repeat(40)),
        ];
        let sessions = project_sessions(&rows, &make_filter(Some("claude_code"), None, None, None));
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_key, "newer");
        assert_eq!(sessions[1].session_key, "older");
        assert_eq!(sessions[1].capture_time, Utc.timestamp_opt(200, 0).unwrap());
        assert!(
            !sessions
                .iter()
                .any(|session| session.session_key == "excluded")
        );

        let filtered = project_sessions(&rows, &make_filter(Some("claude_code"), None, None, None));
        assert_eq!(filtered[1].checkpoints.len(), 2);
    }
}
