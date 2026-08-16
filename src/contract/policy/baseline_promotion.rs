//! UN-35: baseline promotion CAS under the shared maintenance lock.
//!
//! Codec/version rules are UN-39; crash-window recovery tables are UN-40.
//! This module freezes the critical-section order, expect fencing, and the
//! promotion producer wiring into `admit_and_reserve_locked`.

use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::contract::policy::{
    baseline_pointer::{
        read_current_pointer, validate_artifact_digest, write_current_pointer,
        write_version_for_digest,
    },
    secure_artifact::{ArtifactError, ArtifactResult, RestrictedRoot},
    secure_hardcap::{new_op_id, rfc3339_utc},
    secure_lifecycle::{
        NoLeases, ReserveRequest, SettleRequest, abort_locked, admit_and_reserve_locked,
        commit_locked,
    },
    secure_producer::Producer,
    secure_sweep::MaintenanceLock,
};

/// Frozen process exit code when fencing rejects a promote (UN-29 table).
pub const PROMOTION_FENCING_EXIT_CODE: i32 = 3;

/// Caller-supplied fencing expectation (mutually exclusive, one required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteFence {
    /// First promote: pointer must be absent.
    ExpectNoCurrent,
    /// Update: pointer must currently equal this digest.
    ExpectCurrentDigest(String),
}

/// Inputs for one promote attempt (aside from the held lock / root).
#[derive(Debug, Clone)]
pub struct PromoteRequest<'a> {
    pub candidate: &'a [u8],
    pub expect_digest: &'a str,
    pub fence: PromoteFence,
    pub now: SystemTime,
    pub directory_entries: usize,
}

/// Result of a successful promote critical section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// Version written and pointer switched.
    Promoted { digest: String },
    /// Pointer already named the target digest (fencing skipped).
    AlreadyCurrent { digest: String },
}

/// Resolve CLI-style expect flags into a single fence value.
pub fn resolve_promote_fence(
    expect_no_current: bool,
    expect_current_digest: Option<&str>,
) -> ArtifactResult<PromoteFence> {
    match (expect_no_current, expect_current_digest) {
        (true, Some(_)) => Err(ArtifactError::PromotionRejected {
            reason: "--expect-no-current and --expect-current-digest are mutually exclusive".into(),
        }),
        (false, None) => Err(ArtifactError::PromotionRejected {
            reason: "one of --expect-no-current or --expect-current-digest is required".into(),
        }),
        (true, None) => Ok(PromoteFence::ExpectNoCurrent),
        (false, Some(digest)) => {
            validate_artifact_digest(digest)?;
            Ok(PromoteFence::ExpectCurrentDigest(digest.to_string()))
        }
    }
}

/// SHA-256 of `bytes` as `sha256:<64 lowercase hex>`.
pub fn content_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Promote under an already-held maintenance lock.
///
/// Critical-section order (frozen): integrity → already-current → fencing →
/// admit → write version → switch pointer → settle.
pub fn promote_locked(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: PromoteRequest<'_>,
) -> ArtifactResult<PromoteOutcome> {
    promote_locked_inner(root, lock, request, None)
}

#[cfg(test)]
pub(crate) fn promote_locked_with_between_hook(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: PromoteRequest<'_>,
    between_version_and_pointer: &dyn Fn(),
) -> ArtifactResult<PromoteOutcome> {
    promote_locked_inner(root, lock, request, Some(between_version_and_pointer))
}

fn promote_locked_inner(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: PromoteRequest<'_>,
    between_version_and_pointer: Option<&dyn Fn()>,
) -> ArtifactResult<PromoteOutcome> {
    lock.assert_guards(root)?;
    validate_artifact_digest(request.expect_digest)?;

    // 1. Candidate integrity: recompute digest vs --expect-digest.
    let actual = content_digest(request.candidate);
    if actual != request.expect_digest {
        return Err(ArtifactError::PromotionRejected {
            reason: format!(
                "candidate digest {actual} does not match --expect-digest {}",
                request.expect_digest
            ),
        });
    }
    let target = actual;

    // 2. Already-current: unique exception to fencing.
    if let Some(current) = read_current_pointer(root)?
        && current.digest == target
    {
        return Ok(PromoteOutcome::AlreadyCurrent { digest: target });
    }

    // 3. Expect fencing (zero writes on mismatch).
    match &request.fence {
        PromoteFence::ExpectNoCurrent => {
            if read_current_pointer(root)?.is_some() {
                return Err(fencing(
                    "pointer already exists; --expect-no-current failed",
                ));
            }
        }
        PromoteFence::ExpectCurrentDigest(expected) => match read_current_pointer(root)? {
            None => {
                return Err(fencing("pointer is absent; --expect-current-digest failed"));
            }
            Some(current) if current.digest != *expected => {
                return Err(fencing(format!(
                    "pointer is {}; expected {expected}",
                    current.digest
                )));
            }
            Some(_) => {}
        },
    }

    // 4. Admit under the held lock (kind=promotion).
    let op_id = new_op_id();
    let version_name = crate::contract::policy::baseline_pointer::version_file_name(&target)?;
    admit_and_reserve_locked(
        root,
        lock,
        Producer::Promote,
        ReserveRequest {
            op_id: op_id.clone(),
            target: format!("baselines/{version_name} + pointer"),
            payload: target.clone(),
            owner_fenced: None,
            created_at: rfc3339_utc(request.now),
            active_runs: 0,
            protected_count: 0,
            directory_entries: request.directory_entries,
        },
        &NoLeases,
        request.now,
    )?;

    // 5–6. Write version, then switch pointer (hook may observe the window).
    let write_result = (|| -> ArtifactResult<()> {
        write_version_for_digest(root, &target, request.candidate)?;
        if let Some(hook) = between_version_and_pointer {
            hook();
        }
        write_current_pointer(root, lock, &target)?;
        Ok(())
    })();

    match write_result {
        Ok(()) => {
            let settled_delta = i64::try_from(request.candidate.len()).unwrap_or(i64::MAX);
            commit_locked(
                root,
                lock,
                SettleRequest {
                    op_id,
                    settled_delta,
                    final_state: "committed".into(),
                    settled_at: rfc3339_utc(request.now),
                    run_id: None,
                    cap_hash: None,
                },
            )?;
            Ok(PromoteOutcome::Promoted { digest: target })
        }
        Err(err) => {
            let _ = abort_locked(
                root,
                lock,
                SettleRequest {
                    op_id,
                    settled_delta: 0,
                    final_state: "aborted".into(),
                    settled_at: rfc3339_utc(request.now),
                    run_id: None,
                    cap_hash: None,
                },
            );
            Err(err)
        }
    }
}

/// Acquire the maintenance lock and run [`promote_locked`].
pub fn promote(
    root: &RestrictedRoot,
    request: PromoteRequest<'_>,
) -> ArtifactResult<PromoteOutcome> {
    let lock = MaintenanceLock::acquire(root)?;
    promote_locked(root, &lock, request)
}

fn fencing(reason: impl Into<String>) -> ArtifactError {
    ArtifactError::PromotionFencing {
        reason: reason.into(),
        code: PROMOTION_FENCING_EXIT_CODE,
    }
}
