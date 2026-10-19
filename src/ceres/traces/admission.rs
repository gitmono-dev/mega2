//! Complexity and deadline admission for one traces unpack.
//!
//! Counts, depths, object size, and the deadline come from
//! [`crate::config::TracesProtocolLimits`]. A cancelled attempt drops its
//! counters and must not continue.

use std::time::{Duration, Instant};

use crate::{
    common::errors::{MegaError, TracesError},
    config::TracesProtocolLimits,
};

/// Running complexity for one ingest worker.
#[derive(Debug, Clone)]
pub struct IngestAdmission {
    started: Instant,
    elapsed_secs: u64,
    cancelled: bool,
    /// Time the caller attributes to each object already in this attempt.
    pub seconds_per_object: u64,
    pub objects: u64,
    pub new_commits: u64,
    pub delta_depth: u64,
    pub tree_depth: u64,
}

impl IngestAdmission {
    /// Start a worker at the real clock with empty counters.
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
            elapsed_secs: 0,
            cancelled: false,
            seconds_per_object: 0,
            objects: 0,
            new_commits: 0,
            delta_depth: 0,
            tree_depth: 0,
        }
    }

    /// Record time already spent on this attempt, in whole seconds.
    pub fn record_elapsed(&mut self, elapsed_secs: u64) {
        self.elapsed_secs = self.elapsed_secs.max(elapsed_secs);
    }

    /// Fold the real clock, and one caller's per-object step, into elapsed time.
    pub fn refresh_elapsed(&mut self) {
        self.elapsed_secs = self
            .elapsed_secs
            .saturating_add(self.seconds_per_object)
            .max(self.started.elapsed().as_secs());
    }

    /// Fold only the wall clock into the recorded elapsed time.
    pub fn record_wall_clock(&mut self) {
        self.record_elapsed(self.started.elapsed().as_secs());
    }

    /// Cancel this worker and drop the counters it was holding.
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.objects = 0;
        self.new_commits = 0;
        self.delta_depth = 0;
        self.tree_depth = 0;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

/// Reject a sample that is outside the protocol budget.
pub fn admit_ingest(admission: &IngestAdmission, max_object_bytes: u64) -> Result<(), MegaError> {
    let elapsed = Duration::from_secs(admission.elapsed_secs).max(admission.started.elapsed());
    if admission.cancelled
        || elapsed > Duration::from_secs(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS)
    {
        return Err(MegaError::Traces(TracesError::QueryTimeout));
    }
    if admission.objects > TracesProtocolLimits::MAX_OBJECTS
        || admission.new_commits > TracesProtocolLimits::MAX_NEW_COMMITS
        || admission.delta_depth > TracesProtocolLimits::MAX_DELTA_DEPTH
        || admission.tree_depth > TracesProtocolLimits::MAX_TREE_DEPTH
        || max_object_bytes > TracesProtocolLimits::MAX_OBJECT_BYTES
    {
        return Err(MegaError::Traces(TracesError::ResponseBudget));
    }
    Ok(())
}

/// Reject a checkpoint metadata or manifest blob above its raw-byte limit.
///
/// Any other name is not one of those two roles.
pub fn admit_raw_capture_role(role: &str, raw_bytes: u64) -> Result<(), MegaError> {
    let limit = match role {
        "metadata" => TracesProtocolLimits::MAX_METADATA_BYTES,
        "manifest" => TracesProtocolLimits::MAX_MANIFEST_BYTES,
        _ => return Ok(()),
    };
    if raw_bytes > limit {
        return Err(MegaError::Traces(TracesError::ResponseBudget));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_limit() -> IngestAdmission {
        let mut admission = IngestAdmission::start();
        admission.objects = TracesProtocolLimits::MAX_OBJECTS;
        admission.new_commits = TracesProtocolLimits::MAX_NEW_COMMITS;
        admission.delta_depth = TracesProtocolLimits::MAX_DELTA_DEPTH;
        admission.tree_depth = TracesProtocolLimits::MAX_TREE_DEPTH;
        admission.record_elapsed(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS);
        admission
    }

    #[test]
    fn ingest_admission_budget_accepts_each_limit_and_rejects_one_past_it() {
        let admission = at_limit();
        admit_ingest(&admission, TracesProtocolLimits::MAX_OBJECT_BYTES)
            .expect("every limit is inclusive");

        let mut objects = at_limit();
        objects.objects = TracesProtocolLimits::MAX_OBJECTS + 1;
        assert!(admit_ingest(&objects, 1).is_err(), "object count");

        let mut commits = at_limit();
        commits.new_commits = TracesProtocolLimits::MAX_NEW_COMMITS + 1;
        assert!(admit_ingest(&commits, 1).is_err(), "new commits");

        let mut delta = at_limit();
        delta.delta_depth = TracesProtocolLimits::MAX_DELTA_DEPTH + 1;
        assert!(admit_ingest(&delta, 1).is_err(), "delta depth");

        let mut tree = at_limit();
        tree.tree_depth = TracesProtocolLimits::MAX_TREE_DEPTH + 1;
        assert!(admit_ingest(&tree, 1).is_err(), "tree depth");

        let mut deadline = at_limit();
        deadline.record_elapsed(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS + 1);
        assert!(admit_ingest(&deadline, 1).is_err(), "deadline");

        let mut fraction = IngestAdmission::start();
        fraction.started = Instant::now()
            .checked_sub(
                Duration::from_secs(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS)
                    + Duration::from_millis(50),
            )
            .expect("deadline in the past");
        assert!(
            admit_ingest(&fraction, 1).is_err(),
            "a fractional second past 300s is over the deadline"
        );

        assert!(
            admit_ingest(&at_limit(), TracesProtocolLimits::MAX_OBJECT_BYTES + 1).is_err(),
            "object size"
        );

        let mut cancelled = at_limit();
        cancelled.cancel();
        assert!(cancelled.is_cancelled());
        assert_eq!(cancelled.objects, 0);
        assert_eq!(cancelled.new_commits, 0);
        assert_eq!(cancelled.delta_depth, 0);
        assert_eq!(cancelled.tree_depth, 0);
        assert!(admit_ingest(&cancelled, 1).is_err(), "cancelled worker");
    }

    #[test]
    fn raw_capture_document_size_accepts_each_role_limit_and_rejects_one_past_it() {
        admit_raw_capture_role("metadata", TracesProtocolLimits::MAX_METADATA_BYTES)
            .expect("metadata limit is inclusive");
        assert!(
            admit_raw_capture_role("metadata", TracesProtocolLimits::MAX_METADATA_BYTES + 1)
                .is_err(),
            "metadata one byte over"
        );
        admit_raw_capture_role("manifest", TracesProtocolLimits::MAX_MANIFEST_BYTES)
            .expect("manifest limit is inclusive");
        assert!(
            admit_raw_capture_role("manifest", TracesProtocolLimits::MAX_MANIFEST_BYTES + 1)
                .is_err(),
            "manifest one byte over"
        );
        assert!(
            admit_raw_capture_role("notes", TracesProtocolLimits::MAX_METADATA_BYTES + 1).is_ok(),
            "another file name is not a capture role"
        );
    }
}
