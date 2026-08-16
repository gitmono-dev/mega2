//! UN-35: baseline promotion CAS under the shared maintenance lock.
//! UN-40: crash-window end states and retry convergence for that CAS.
//!
//! Codec/version rules are UN-39. This module freezes the critical-section
//! order, expect fencing, promotion producer wiring into
//! `admit_and_reserve_locked`, and the W1..W6 retry table.
//!
//! # Crash windows (UN-40)
//!
//! | Window | Visible state | Retry | Temps | Exit |
//! |---|---|---|---|---|
//! | W1 | version `.tmp-*` left; pointer unchanged | normal promote | sweep clears by age (UN-38) | 0 on success |
//! | W2 | version may exist after rename; dir fsync pending | UN-39 idempotent version write | — | 0 |
//! | W3 | version present; pointer still old | continue (fence on old) | — | 0 |
//! | W4 | pointer `.tmp-*` left | same as W1 | sweep by age | 0 |
//! | W5 | pointer may be old or new after rename | read pointer → already-current or continue | — | 0 |
//! | W6 | fully committed before return | already-current | — | 0 |
//!
//! **Already-current (frozen):** pointer digest equals the target **and** the
//! version file bytes match the candidate → success with
//! [`ALREADY_CURRENT_MARKER`] (CLI maps this to exit 0 + stderr), independent of
//! expect fencing flags. `.tmp-*` cleanup stays UN-38's age rule.

use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::contract::policy::{
    baseline_pointer::{
        read_current_pointer, read_version_for_digest, validate_artifact_digest,
        write_current_pointer, write_version_for_digest,
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

/// Stderr token for already-current idempotent success (UN-40 / CLI).
pub const ALREADY_CURRENT_MARKER: &str = "already-current";

/// Named crash windows from the UN-40 end-state table (documentary + tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashWindow {
    /// Version temp left; pointer unchanged.
    W1VersionTemp,
    /// Version renamed; directory fsync not yet durable.
    W2VersionRenamed,
    /// Version present; pointer still previous digest / absent.
    W3VersionWithoutPointer,
    /// Pointer temp left.
    W4PointerTemp,
    /// Pointer renamed; directory fsync not yet durable.
    W5PointerRenamed,
    /// Promote returned successfully; caller crashed before observing it.
    W6FullyCommitted,
}

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
    /// Pointer already named the target digest with matching version bytes.
    AlreadyCurrent { digest: String },
}

impl PromoteOutcome {
    /// CLI/stderr token for [`PromoteOutcome::AlreadyCurrent`].
    pub fn already_current_marker(&self) -> Option<&'static str> {
        match self {
            Self::AlreadyCurrent { .. } => Some(ALREADY_CURRENT_MARKER),
            Self::Promoted { .. } => None,
        }
    }
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
///
/// Crash recovery is **retry this function** against the durable end state
/// (UN-40 W1..W6); temps are left for UN-38 sweep.
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

    // 2. Already-current / repair (UN-40): pointer == target.
    let mut skip_fencing = false;
    if let Some(current) = read_current_pointer(root)?
        && current.digest == target
    {
        match read_version_for_digest(root, &target)? {
            Some(existing) if existing.as_slice() == request.candidate => {
                // W5/W6 / mid-settle crash: durable promote is done but the
                // reservation may still be open — settle it before returning.
                settle_matching_promotion(
                    root,
                    lock,
                    &target,
                    i64::try_from(request.candidate.len()).unwrap_or(i64::MAX),
                    request.now,
                )?;
                return Ok(PromoteOutcome::AlreadyCurrent { digest: target });
            }
            Some(_) => {
                return Err(ArtifactError::BaselineVersionConflict {
                    name: crate::contract::policy::baseline_pointer::version_file_name(&target)?,
                });
            }
            // Pointer ahead of version (crash mid-repair): continue without fencing.
            None => skip_fencing = true,
        }
    }

    // 3. Expect fencing (zero writes on mismatch) — skipped when repairing.
    if !skip_fencing {
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

/// Commit any open promotion reservation whose payload is `target` (crash after
/// durable pointer switch but before settle).
fn settle_matching_promotion(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    target: &str,
    settled_delta: i64,
    now: SystemTime,
) -> ArtifactResult<()> {
    loop {
        let counter = crate::contract::policy::secure_counter::load_counter(root, lock)?;
        let Some(reservation) = counter.reservations.iter().find(|r| {
            r.kind == crate::contract::policy::secure_counter::ReservationKind::Promotion
                && r.payload == target
        }) else {
            return Ok(());
        };
        commit_locked(
            root,
            lock,
            SettleRequest {
                op_id: reservation.op_id.clone(),
                settled_delta,
                final_state: "committed".into(),
                settled_at: rfc3339_utc(now),
                run_id: None,
                cap_hash: None,
            },
        )?;
    }
}
