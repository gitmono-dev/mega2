//! UN-57: reservation lifecycle under the maintenance lock.
//!
//! `admit_and_reserve_locked` takes an already-held [`MaintenanceLock`] and never
//! acquires it again. Commit / abort settle into `settled[]` or `delete_settled[]`
//! tombs; stale reservations older than one hour are reclaimed inline before
//! admission (lease-alive runs are left alone).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::contract::policy::{
    secure_admission::{AdmissionDecision, AdmissionDeny, AdmissionSnapshot, admit},
    secure_artifact::{ArtifactError, ArtifactResult, RestrictedRoot},
    secure_counter::{
        CapacityCounter, DeleteSettledRecord, MAX_SETTLED, ReservationAction, ReservationKind,
        ReservationRecord, SettledRecord, load_counter, store_counter, validate_cap_hash,
    },
    secure_producer::{Producer, mapping_for, signed_length_delta},
    secure_sweep::MaintenanceLock,
};

/// Age after which a reservation may be reclaimed if its lease is gone.
pub const RESERVATION_RECLAIM_AGE: Duration = Duration::from_secs(60 * 60);
/// Settled tombs older than this may be pruned during reclaim.
pub const SETTLED_TOMB_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Inputs for [`admit_and_reserve_locked`].
#[derive(Debug, Clone)]
pub struct ReserveRequest {
    pub op_id: String,
    pub target: String,
    pub payload: String,
    /// Required for `kind=run` (evidence=`true`, audit=`false`).
    pub owner_fenced: Option<bool>,
    pub created_at: String,
    pub active_runs: u64,
    pub protected_count: usize,
    pub directory_entries: usize,
}

/// A reservation that was just admitted and persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveOutcome {
    pub reservation: ReservationRecord,
}

/// Inputs for commit / abort settlement.
#[derive(Debug, Clone)]
pub struct SettleRequest {
    pub op_id: String,
    pub settled_delta: i64,
    pub final_state: String,
    pub settled_at: String,
    pub run_id: Option<String>,
    /// Evidence owner capability hash (`sha256:<64hex>`); required to settle
    /// owner-fenced runs and to authorize idempotent retries.
    pub cap_hash: Option<String>,
}

/// Result of a settle that may be a first-time write or an idempotent retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleOutcome {
    Settled {
        settled_delta: i64,
        final_state: String,
    },
    IdempotentRetry {
        settled_delta: i64,
        final_state: String,
    },
}

/// Whether a run-directory lease is still held (caller probes `.lease.lock`).
pub trait LeaseView {
    fn lease_alive(&self, run_id_or_target: &str) -> bool;
}

/// Default: no leases alive (tests / paths without run leases).
pub struct NoLeases;

impl LeaseView for NoLeases {
    fn lease_alive(&self, _run_id_or_target: &str) -> bool {
        false
    }
}

/// Admit + reserve under an already-held maintenance lock.
///
/// Critical section: reclaim stale → admit (UN-58) → append reservation →
/// bump `reserved_bytes` → persist (create vs delete ceiling).
pub fn admit_and_reserve_locked(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    producer: Producer,
    request: ReserveRequest,
    leases: &dyn LeaseView,
    now: SystemTime,
) -> ArtifactResult<ReserveOutcome> {
    let mapping = mapping_for(producer);
    let mut counter = load_counter(root, lock)?;
    reclaim_stale_locked(&mut counter, leases, now)?;

    let create_ledger_bytes = match counter.to_canonical_bytes(true) {
        Ok(bytes) => bytes.len(),
        Err(ArtifactError::CounterTooLarge { bytes, .. }) => bytes,
        Err(err) => return Err(err),
    };

    let snapshot = AdmissionSnapshot {
        total_bytes: counter.total_bytes,
        reserved_bytes: counter.reserved_bytes,
        active_runs: request.active_runs,
        protected_count: request.protected_count,
        directory_entries: request.directory_entries,
        create_reservations: counter.create_reservation_count(),
        settled_count: counter.settled.len(),
        create_ledger_bytes,
    };

    match admit(producer, &snapshot) {
        AdmissionDecision::Allow => {}
        AdmissionDecision::Deny(reason) => {
            return Err(ArtifactError::AdmissionDenied {
                reason: format!("{reason:?}"),
            });
        }
    }

    if mapping.kind == ReservationKind::Run && request.owner_fenced.is_none() {
        return Err(ArtifactError::LifecycleRejected {
            reason: "run reservation requires owner_fenced".into(),
        });
    }
    if mapping.kind != ReservationKind::Run && request.owner_fenced.is_some() {
        return Err(ArtifactError::LifecycleRejected {
            reason: "owner_fenced is only valid on kind=run".into(),
        });
    }

    if mapping.kind == ReservationKind::Run && request.owner_fenced == Some(true) {
        validate_cap_hash(&request.payload).map_err(|err| match err {
            ArtifactError::CounterInvalid { reason } => ArtifactError::LifecycleRejected { reason },
            other => other,
        })?;
    }

    if counter
        .reservations
        .iter()
        .any(|r| r.op_id == request.op_id)
        || counter.settled.iter().any(|t| t.op_id == request.op_id)
        || counter
            .delete_settled
            .iter()
            .any(|t| t.op_id == request.op_id)
    {
        return Err(ArtifactError::LifecycleRejected {
            reason: format!("reservation op_id {} already used", request.op_id),
        });
    }

    let reservation = ReservationRecord {
        op_id: request.op_id.clone(),
        kind: mapping.kind,
        action: mapping.action,
        created_at: request.created_at,
        max_bytes: mapping.max_bytes,
        target: request.target,
        payload: request.payload,
        owner_fenced: request.owner_fenced,
        settle_key: request.op_id.clone(),
    };

    let reserved_delta = mapping.reserved_delta;
    if reserved_delta > 0 {
        counter.reserved_bytes = counter.reserved_bytes.saturating_add(reserved_delta as u64);
    }

    counter.reservations.push(reservation.clone());
    let for_create = mapping.action == ReservationAction::Create;
    store_counter(root, lock, &counter, for_create)?;
    Ok(ReserveOutcome { reservation })
}

/// Commit a reservation: apply `settled_delta`, drop the reservation, write a tomb.
pub fn commit_locked(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: SettleRequest,
) -> ArtifactResult<SettleOutcome> {
    settle_locked(root, lock, request, /*abort=*/ false)
}

/// Abort a reservation: release reserved bytes (delta typically 0 or negative
/// accounting), drop the reservation, write a tomb.
pub fn abort_locked(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: SettleRequest,
) -> ArtifactResult<SettleOutcome> {
    settle_locked(root, lock, request, /*abort=*/ true)
}

fn settle_locked(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    request: SettleRequest,
    abort: bool,
) -> ArtifactResult<SettleOutcome> {
    let mut counter = load_counter(root, lock)?;

    if let Some(outcome) = idempotent_settled_lookup(&counter, &request)? {
        return Ok(outcome);
    }

    let index = counter
        .reservations
        .iter()
        .position(|r| r.op_id == request.op_id)
        .ok_or_else(|| ArtifactError::LifecycleRejected {
            reason: format!("no active reservation for op_id {}", request.op_id),
        })?;
    let reservation = counter.reservations.remove(index);

    if reservation.kind == ReservationKind::Run && reservation.owner_fenced == Some(true) {
        let Some(cap) = request.cap_hash.as_deref() else {
            return Err(ArtifactError::LifecycleRejected {
                reason: "owner-fenced run settle requires cap_hash".into(),
            });
        };
        validate_cap_hash(cap).map_err(|err| match err {
            ArtifactError::CounterInvalid { reason } => ArtifactError::LifecycleRejected { reason },
            other => other,
        })?;
        if reservation.payload != cap {
            return Err(ArtifactError::LifecycleRejected {
                reason: "cap_hash does not match reservation payload".into(),
            });
        }
        let Some(run_id) = request.run_id.as_deref().filter(|id| !id.is_empty()) else {
            return Err(ArtifactError::LifecycleRejected {
                reason: "owner-fenced run settle requires run_id".into(),
            });
        };
        crate::contract::policy::secure_artifact::validate_run_id(run_id)?;
        if !reservation_target_matches_run(&reservation.target, run_id) {
            return Err(ArtifactError::LifecycleRejected {
                reason: "run_id does not match reservation target".into(),
            });
        }
    }

    let reserved_release = reservation.max_bytes;
    counter.reserved_bytes = counter.reserved_bytes.saturating_sub(reserved_release);

    if let Some(total) = checked_apply_total(counter.total_bytes, request.settled_delta) {
        counter.total_bytes = total;
    } else {
        return Err(ArtifactError::LifecycleRejected {
            reason: "settled_delta overflows total_bytes".into(),
        });
    }

    let final_state = if abort && request.final_state.is_empty() {
        "aborted".to_string()
    } else {
        request.final_state.clone()
    };

    let for_create = reservation.action == ReservationAction::Create;
    if reservation.action == ReservationAction::Delete {
        counter.push_delete_settled(DeleteSettledRecord {
            op_id: reservation.op_id.clone(),
            kind: ReservationKind::Protect,
            action: ReservationAction::Delete,
            settled_delta: request.settled_delta,
            final_state: final_state.clone(),
            settled_at: request.settled_at.clone(),
        });
    } else {
        prune_settled_if_needed(&mut counter)?;
        counter.settled.push(SettledRecord {
            op_id: reservation.op_id.clone(),
            kind: reservation.kind,
            action: reservation.action,
            settled_delta: request.settled_delta,
            final_state: final_state.clone(),
            settled_at: request.settled_at.clone(),
            owner_fenced: reservation.owner_fenced,
            run_id: if reservation.owner_fenced == Some(true) {
                request.run_id.clone()
            } else {
                None
            },
            cap_hash: if reservation.owner_fenced == Some(true) {
                request.cap_hash.clone()
            } else {
                None
            },
        });
    }

    store_counter(root, lock, &counter, for_create)?;
    Ok(SettleOutcome::Settled {
        settled_delta: request.settled_delta,
        final_state,
    })
}

fn idempotent_settled_lookup(
    counter: &CapacityCounter,
    request: &SettleRequest,
) -> ArtifactResult<Option<SettleOutcome>> {
    if let Some(tomb) = counter
        .delete_settled
        .iter()
        .find(|t| t.op_id == request.op_id)
    {
        return Ok(Some(SettleOutcome::IdempotentRetry {
            settled_delta: tomb.settled_delta,
            final_state: tomb.final_state.clone(),
        }));
    }

    let Some(tomb) = counter.settled.iter().find(|t| t.op_id == request.op_id) else {
        return Ok(None);
    };

    if tomb.owner_fenced == Some(true) {
        let Some(cap) = request.cap_hash.as_deref() else {
            return Err(ArtifactError::LifecycleRejected {
                reason: "owner-fenced retry requires cap_hash".into(),
            });
        };
        if tomb.cap_hash.as_deref() != Some(cap) {
            return Err(ArtifactError::LifecycleRejected {
                reason: "cap_hash does not match settled tomb".into(),
            });
        }
        let Some(run_id) = request.run_id.as_deref().filter(|id| !id.is_empty()) else {
            return Err(ArtifactError::LifecycleRejected {
                reason: "owner-fenced retry requires run_id".into(),
            });
        };
        if tomb.run_id.as_deref() != Some(run_id) {
            return Err(ArtifactError::LifecycleRejected {
                reason: "run_id does not match settled tomb".into(),
            });
        }
    }

    Ok(Some(SettleOutcome::IdempotentRetry {
        settled_delta: tomb.settled_delta,
        final_state: tomb.final_state.clone(),
    }))
}

fn prune_settled_if_needed(counter: &mut CapacityCounter) -> ArtifactResult<()> {
    if counter.settled.len() < MAX_SETTLED {
        return Ok(());
    }
    Err(ArtifactError::LifecycleRejected {
        reason: format!("settled tombs full at {MAX_SETTLED}; reclaim aged tombs first"),
    })
}

/// Reclaim reservations whose `created_at` age exceeds [`RESERVATION_RECLAIM_AGE`]
/// and whose lease (if any) is not alive. Also drops aged settled tombs.
pub fn reclaim_stale_locked(
    counter: &mut CapacityCounter,
    leases: &dyn LeaseView,
    now: SystemTime,
) -> ArtifactResult<usize> {
    let mut reclaimed = 0usize;
    let mut kept = Vec::with_capacity(counter.reservations.len());
    for reservation in counter.reservations.drain(..) {
        let age = age_of_rfc3339(&reservation.created_at, now)?;
        let lease_blocks =
            reservation.kind == ReservationKind::Run && leases.lease_alive(&reservation.target);
        if age > RESERVATION_RECLAIM_AGE && !lease_blocks {
            counter.reserved_bytes = counter.reserved_bytes.saturating_sub(reservation.max_bytes);
            reclaimed += 1;
        } else {
            kept.push(reservation);
        }
    }
    counter.reservations = kept;

    counter
        .settled
        .retain(|tomb| match age_of_rfc3339(&tomb.settled_at, now) {
            Ok(age) => age <= SETTLED_TOMB_MAX_AGE,
            // Malformed tombs are not trusted as "still fresh".
            Err(_) => false,
        });
    Ok(reclaimed)
}

/// Persist after an in-memory reclaim (caller already holds the lock).
pub fn persist_reclaim(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    counter: &CapacityCounter,
) -> ArtifactResult<()> {
    store_counter(root, lock, counter, true)
}

pub fn signed_delta(before_len: u64, after_len: u64) -> ArtifactResult<i64> {
    signed_length_delta(before_len, after_len).ok_or_else(|| ArtifactError::LifecycleRejected {
        reason: "length delta overflow".into(),
    })
}

fn checked_apply_total(total: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        total.checked_add(delta as u64)
    } else {
        total.checked_sub(delta.unsigned_abs())
    }
}

/// Exact path match: `runs/<run_id>` or `runs/<run_id>/...`.
fn reservation_target_matches_run(target: &str, run_id: &str) -> bool {
    let prefix = format!("runs/{run_id}");
    target == prefix || target.starts_with(&(prefix + "/"))
}

fn age_of_rfc3339(stamp: &str, now: SystemTime) -> ArtifactResult<Duration> {
    // Accept exactly `YYYY-MM-DDTHH:MM:SSZ`.
    let Some(trimmed) = stamp.strip_suffix('Z') else {
        return Err(ArtifactError::LifecycleRejected {
            reason: format!("timestamp must end with Z: {stamp}"),
        });
    };
    if trimmed.contains('Z') || stamp.matches('Z').count() != 1 {
        return Err(ArtifactError::LifecycleRejected {
            reason: format!("timestamp must contain exactly one Z: {stamp}"),
        });
    }
    let parts: Vec<_> = trimmed.split('T').collect();
    if parts.len() != 2 {
        return Err(ArtifactError::LifecycleRejected {
            reason: format!("bad created_at/settled_at {stamp}"),
        });
    }
    let (date, time) = (parts[0], parts[1]);
    let dp: Vec<_> = date.split('-').collect();
    let tp: Vec<_> = time.split(':').collect();
    if dp.len() != 3 || tp.len() != 3 {
        return Err(ArtifactError::LifecycleRejected {
            reason: format!("bad timestamp {stamp}"),
        });
    }
    let year: i32 = dp[0]
        .parse()
        .map_err(|_| ArtifactError::LifecycleRejected {
            reason: format!("bad year in {stamp}"),
        })?;
    let month: u32 = dp[1]
        .parse()
        .map_err(|_| ArtifactError::LifecycleRejected {
            reason: format!("bad month in {stamp}"),
        })?;
    let day: u32 = dp[2]
        .parse()
        .map_err(|_| ArtifactError::LifecycleRejected {
            reason: format!("bad day in {stamp}"),
        })?;
    let hour: u32 = tp[0]
        .parse()
        .map_err(|_| ArtifactError::LifecycleRejected {
            reason: format!("bad hour in {stamp}"),
        })?;
    let min: u32 = tp[1]
        .parse()
        .map_err(|_| ArtifactError::LifecycleRejected {
            reason: format!("bad minute in {stamp}"),
        })?;
    let sec: u32 = tp[2]
        .parse()
        .map_err(|_| ArtifactError::LifecycleRejected {
            reason: format!("bad second in {stamp}"),
        })?;
    if hour > 23 || min > 59 || sec > 60 {
        return Err(ArtifactError::LifecycleRejected {
            reason: format!("out-of-range time in {stamp}"),
        });
    }
    let days = days_from_civil(year, month, day)?;
    let secs = days
        .checked_mul(86400)
        .and_then(|d| d.checked_add(i64::from(hour) * 3600))
        .and_then(|d| d.checked_add(i64::from(min) * 60))
        .and_then(|d| d.checked_add(i64::from(sec)))
        .ok_or_else(|| ArtifactError::LifecycleRejected {
            reason: format!("timestamp overflow {stamp}"),
        })?;
    if secs < 0 {
        return Err(ArtifactError::LifecycleRejected {
            reason: format!("pre-epoch timestamp rejected: {stamp}"),
        });
    }
    let then = UNIX_EPOCH + Duration::from_secs(secs as u64);
    Ok(now.duration_since(then).unwrap_or(Duration::ZERO))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> ArtifactResult<i64> {
    const DIM: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&month) {
        return Err(ArtifactError::LifecycleRejected {
            reason: "invalid date".into(),
        });
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = if month == 2 && leap {
        29
    } else {
        DIM[(month - 1) as usize]
    };
    if day == 0 || day > max_day {
        return Err(ArtifactError::LifecycleRejected {
            reason: "invalid date".into(),
        });
    }
    // Howard Hinnant civil-from-days inverse (UTC days since 1970-01-01).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Ok(era as i64 * 146097 + doe as i64 - 719468)
}

/// Map an admission deny into a stable reason string for callers/tests.
pub fn deny_reason(deny: &AdmissionDeny) -> String {
    format!("{deny:?}")
}

const _: () = {
    assert!(RESERVATION_RECLAIM_AGE.as_secs() == 3600);
};
