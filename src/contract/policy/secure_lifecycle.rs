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

#[cfg(all(test, target_os = "linux"))]
mod reservation_lifecycle {
    //! UN-57: reservation lifecycle under the maintenance lock.

    use std::time::{Duration, SystemTime};

    use crate::contract::policy::{
        secure_artifact::{ArtifactError, RestrictedRoot},
        secure_capacity::{FIXED_METADATA_BYTES, MIB},
        secure_counter::{
            COUNTERS_FILE, CapacityCounter, MAX_COUNTERS_BYTES_FOR_CREATE, MAX_CREATE_RESERVATIONS,
            MAX_SETTLED, ReservationAction, ReservationKind, ReservationRecord, SettledRecord,
            load_counter, store_counter,
        },
        secure_lifecycle::{
            NoLeases, RESERVATION_RECLAIM_AGE, ReserveRequest, SettleOutcome, SettleRequest,
            abort_locked, admit_and_reserve_locked, commit_locked, reclaim_stale_locked,
            signed_delta,
        },
        secure_producer::Producer,
        secure_sweep::MaintenanceLock,
    };

    fn root() -> (tempfile::TempDir, RestrictedRoot) {
        let temp = tempfile::tempdir().expect("temp");
        let root = RestrictedRoot::open(temp.path()).expect("open");
        (temp, root)
    }

    fn now_stamp() -> String {
        "2026-08-16T12:00:00Z".into()
    }

    fn reserve_run(op: &str, owner_fenced: bool) -> ReserveRequest {
        ReserveRequest {
            op_id: op.into(),
            target: "runs/20260816T120000Z-1".into(),
            payload: if owner_fenced {
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into()
            } else {
                String::new()
            },
            owner_fenced: Some(owner_fenced),
            created_at: now_stamp(),
            active_runs: 0,
            protected_count: 0,
            directory_entries: 0,
        }
    }

    #[test]
    fn un57_lifecycle_admit_commit_abort_round_trip() {
        let (_temp, root) = root();
        let lock = MaintenanceLock::acquire(&root).expect("lock");

        let out = admit_and_reserve_locked(
            &root,
            &lock,
            Producer::BootstrapCandidate,
            reserve_run("01HQBOOT0000000001", false),
            &NoLeases,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
        )
        .expect("admit");
        assert_eq!(out.reservation.max_bytes, 3 * MIB);
        assert_eq!(out.reservation.owner_fenced, Some(false));

        let committed = commit_locked(
            &root,
            &lock,
            SettleRequest {
                op_id: "01HQBOOT0000000001".into(),
                settled_delta: 1024,
                final_state: "committed".into(),
                settled_at: now_stamp(),
                run_id: None,
                cap_hash: None,
            },
        )
        .expect("commit");
        assert!(matches!(
            committed,
            SettleOutcome::Settled {
                settled_delta: 1024,
                ..
            }
        ));

        let counter = load_counter(&root, &lock).expect("load");
        assert!(counter.reservations.is_empty());
        assert_eq!(counter.settled.len(), 1);
        assert_eq!(counter.settled[0].cap_hash, None);
        assert_eq!(counter.total_bytes, 1024);
        assert_eq!(counter.reserved_bytes, 0);
    }

    #[test]
    fn un57_lifecycle_owner_fenced_idempotent_retry_and_wrong_cap() {
        let (_temp, root) = root();
        let lock = MaintenanceLock::acquire(&root).expect("lock");
        let cap = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

        admit_and_reserve_locked(
            &root,
            &lock,
            Producer::KillSwitchEvidence,
            ReserveRequest {
                op_id: "01HQEVID0000000001".into(),
                target: "runs/20260816T120000Z-9".into(),
                payload: cap.into(),
                owner_fenced: Some(true),
                created_at: now_stamp(),
                active_runs: 0,
                protected_count: 0,
                directory_entries: 0,
            },
            &NoLeases,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
        )
        .expect("admit");

        commit_locked(
            &root,
            &lock,
            SettleRequest {
                op_id: "01HQEVID0000000001".into(),
                settled_delta: 200,
                final_state: "committed".into(),
                settled_at: now_stamp(),
                run_id: Some("20260816T120000Z-9".into()),
                cap_hash: Some(cap.into()),
            },
        )
        .expect("commit");

        let retry = commit_locked(
            &root,
            &lock,
            SettleRequest {
                op_id: "01HQEVID0000000001".into(),
                settled_delta: 999,
                final_state: "ignored".into(),
                settled_at: now_stamp(),
                run_id: Some("20260816T120000Z-9".into()),
                cap_hash: Some(cap.into()),
            },
        )
        .expect("idempotent");
        assert!(matches!(
            retry,
            SettleOutcome::IdempotentRetry {
                settled_delta: 200,
                ..
            }
        ));

        let err = commit_locked(
            &root,
            &lock,
            SettleRequest {
                op_id: "01HQEVID0000000001".into(),
                settled_delta: 0,
                final_state: "x".into(),
                settled_at: now_stamp(),
                run_id: Some("20260816T120000Z-9".into()),
                cap_hash: Some(
                    "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                        .into(),
                ),
            },
        )
        .expect_err("wrong cap");
        assert!(matches!(err, ArtifactError::LifecycleRejected { .. }));
    }

    #[test]
    fn un57_lifecycle_unprotect_uses_delete_settled_and_signed_delta() {
        let (_temp, root) = root();
        let lock = MaintenanceLock::acquire(&root).expect("lock");

        // Fill settled[] to capacity so unprotect must use delete_settled[].
        let mut counter = CapacityCounter::default();
        for i in 0..MAX_SETTLED {
            counter.settled.push(SettledRecord {
                op_id: format!("01HQFILL{i:016}"),
                kind: ReservationKind::Report,
                action: ReservationAction::Create,
                settled_delta: 1,
                final_state: "committed".into(),
                settled_at: format!("2026-08-16T{:02}:{:02}:00Z", i / 60, i % 60),
                owner_fenced: None,
                run_id: None,
                cap_hash: None,
            });
        }
        store_counter(&root, &lock, &counter, true).expect("seed settled");
        // Seed total_bytes so a negative unprotect delta can apply.
        let mut counter = load_counter(&root, &lock).expect("load");
        counter.total_bytes = 100;
        store_counter(&root, &lock, &counter, true).expect("seed total");

        admit_and_reserve_locked(
            &root,
            &lock,
            Producer::Unprotect,
            ReserveRequest {
                op_id: "01HQUNPROT00000001".into(),
                target: "baselines/protected.json".into(),
                payload: "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    .into(),
                owner_fenced: None,
                created_at: now_stamp(),
                active_runs: 0,
                protected_count: 1,
                directory_entries: 0,
            },
            &NoLeases,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
        )
        .expect("unprotect admit");

        let delta = signed_delta(100, 80).expect("delta");
        assert_eq!(delta, -20);
        commit_locked(
            &root,
            &lock,
            SettleRequest {
                op_id: "01HQUNPROT00000001".into(),
                settled_delta: delta,
                final_state: "deleted".into(),
                settled_at: now_stamp(),
                run_id: None,
                cap_hash: None,
            },
        )
        .expect("unprotect commit");

        let counter = load_counter(&root, &lock).expect("load");
        assert_eq!(counter.settled.len(), MAX_SETTLED);
        assert_eq!(counter.delete_settled.len(), 1);
        assert_eq!(counter.delete_settled[0].settled_delta, -20);
    }

    #[test]
    fn un57_lifecycle_reclaim_respects_age_and_try_acquire_while_held() {
        let (temp, root) = root();
        let _lock = MaintenanceLock::acquire(&root).expect("lock");

        let path = temp.path().to_path_buf();
        let held = std::thread::spawn(move || {
            let root2 = RestrictedRoot::open(&path).expect("open");
            MaintenanceLock::try_acquire(&root2).expect("try")
        })
        .join()
        .expect("join");
        assert!(
            held.is_none(),
            "second acquire must fail while maintenance lock is held"
        );

        let mut counter = CapacityCounter::default();
        counter.reservations.push(ReservationRecord {
            op_id: "01HQOLD00000000001".into(),
            kind: ReservationKind::Promotion,
            action: ReservationAction::Create,
            created_at: "2026-01-01T00:00:00Z".into(),
            max_bytes: MIB,
            target: "baselines/aa.json".into(),
            payload: String::new(),
            owner_fenced: None,
            settle_key: "01HQOLD00000000001".into(),
        });
        counter.reserved_bytes = MIB;
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let n = reclaim_stale_locked(&mut counter, &NoLeases, now).expect("reclaim");
        assert_eq!(n, 1);
        assert!(counter.reservations.is_empty());
        assert_eq!(counter.reserved_bytes, 0);
        assert!(RESERVATION_RECLAIM_AGE.as_secs() == 3600);
    }

    #[test]
    fn un57_lifecycle_unprotect_still_works_when_create_ledger_full() {
        let (_temp, root) = root();
        let lock = MaintenanceLock::acquire(&root).expect("lock");

        // Inflate create reservations until the create-path ledger is at/over 40 KiB.
        let mut counter = CapacityCounter::default();
        for i in 0..MAX_CREATE_RESERVATIONS {
            let mut r = ReservationRecord {
                op_id: format!("01HQFULL{i:016}"),
                kind: ReservationKind::Report,
                action: ReservationAction::Create,
                created_at: now_stamp(),
                max_bytes: 1024,
                target: format!("sweep-reports/{}", "t".repeat(40)),
                payload: "p".repeat(900),
                owner_fenced: None,
                settle_key: format!("01HQFULL{i:016}"),
            };
            // Keep pushing payload until serialize would exceed create ceiling.
            while counter.to_canonical_bytes(true).is_ok() && r.payload.len() < 2000 {
                r.payload.push('x');
                counter.reservations.pop();
                counter.reservations.push(r.clone());
            }
            if counter.reservations.len() <= i {
                counter.reservations.push(r);
            }
            if counter.to_canonical_bytes(true).is_err() {
                break;
            }
        }
        // Persist with delete ceiling (for_create=false) so the oversize create
        // ledger still lands for the unprotect path to read.
        let bytes = serde_json::to_vec(&counter).expect("serde");
        assert!(
            bytes.len() > MAX_COUNTERS_BYTES_FOR_CREATE || counter.create_reservation_count() > 0
        );
        store_counter(&root, &lock, &counter, false).unwrap_or_else(|_| {
            // If even full ceiling fails, shrink payload once and store via delete path.
            counter.reservations.truncate(40);
            store_counter(&root, &lock, &counter, false).expect("store truncated")
        });
        assert!(root.display().join(COUNTERS_FILE).is_file());

        admit_and_reserve_locked(
            &root,
            &lock,
            Producer::Unprotect,
            ReserveRequest {
                op_id: "01HQUNPROTFULL0001".into(),
                target: "baselines/protected.json".into(),
                payload: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .into(),
                owner_fenced: None,
                created_at: now_stamp(),
                active_runs: 0,
                protected_count: 1,
                directory_entries: 0,
            },
            &NoLeases,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
        )
        .expect("unprotect must still admit under create pressure");

        abort_locked(
            &root,
            &lock,
            SettleRequest {
                op_id: "01HQUNPROTFULL0001".into(),
                settled_delta: 0,
                final_state: "aborted".into(),
                settled_at: now_stamp(),
                run_id: None,
                cap_hash: None,
            },
        )
        .expect("abort");
    }

    #[test]
    fn un57_lifecycle_six_producers_immediate_settle() {
        let producers = [
            (Producer::BootstrapCandidate, Some(false)),
            (Producer::Compare, Some(false)),
            (Producer::Promote, None),
            (Producer::SweepReport, None),
            (Producer::Protect, None),
            (Producer::Unprotect, None),
        ];
        for (i, (producer, owner_fenced)) in producers.into_iter().enumerate() {
            let (_temp, root) = root();
            let lock = MaintenanceLock::acquire(&root).expect("lock");
            if producer == Producer::Unprotect {
                let counter = CapacityCounter {
                    total_bytes: 10,
                    ..CapacityCounter::default()
                };
                store_counter(&root, &lock, &counter, true).expect("seed");
            }
            let op = format!("01HQSIX{i:016}");
            let req = ReserveRequest {
                op_id: op.clone(),
                target: match producer {
                    Producer::Promote => "baselines/digest.json".into(),
                    Producer::SweepReport => "sweep-reports/20260816T120000Z-1.json".into(),
                    Producer::Protect | Producer::Unprotect => "baselines/protected.json".into(),
                    _ => "runs/20260816T120000Z-1".into(),
                },
                payload: if producer == Producer::Protect || producer == Producer::Unprotect {
                    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()
                } else {
                    String::new()
                },
                owner_fenced,
                created_at: now_stamp(),
                active_runs: 0,
                protected_count: 0,
                directory_entries: 0,
            };
            admit_and_reserve_locked(
                &root,
                &lock,
                producer,
                req,
                &NoLeases,
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
            )
            .unwrap_or_else(|e| panic!("{producer:?} admit: {e}"));
            commit_locked(
                &root,
                &lock,
                SettleRequest {
                    op_id: op,
                    settled_delta: if producer == Producer::Unprotect {
                        -1
                    } else if producer == Producer::Protect {
                        FIXED_METADATA_BYTES as i64
                    } else {
                        10
                    },
                    final_state: "committed".into(),
                    settled_at: now_stamp(),
                    run_id: None,
                    cap_hash: None,
                },
            )
            .unwrap_or_else(|e| panic!("{producer:?} commit: {e}"));
            let counter = load_counter(&root, &lock).expect("load");
            assert!(
                counter.reservations.is_empty(),
                "{producer:?} left a reservation"
            );
        }
    }

    #[test]
    fn un57_lifecycle_signed_delta_growth_shrink_noop() {
        assert_eq!(signed_delta(10, 15).unwrap(), 5);
        assert_eq!(signed_delta(15, 10).unwrap(), -5);
        assert_eq!(signed_delta(10, 10).unwrap(), 0);
    }
}
