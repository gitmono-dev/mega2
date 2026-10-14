//! UN-58: pure capacity admission judgment and the disk-pressure alert event.
//!
//! Wiring into writers / sweep is UN-59 (and promotion / evidence / protect
//! settlers). This module only answers whether a producer may proceed given a
//! snapshot of counter and directory state.

use tracing::warn;

use crate::contract::policy::{
    secure_capacity::{A, D, P_MAX, peak_capacity_default},
    secure_counter::{
        MAX_COUNTERS_BYTES_FOR_CREATE, MAX_CREATE_RESERVATIONS, MAX_SETTLED, ReservationAction,
        ReservationKind,
    },
    secure_producer::{Producer, mapping_for},
};

/// Structured log / alert event name frozen by the plan card.
pub const DISK_PRESSURE_EVENT: &str = "audit_retention_disk_pressure";

/// Settled tombs that create admission must leave free for settle / delete.
pub const SETTLED_CREATE_HEADROOM: usize = 8;

/// Create admission may not push `settled[]` past this projected occupancy.
pub const MAX_SETTLED_FOR_CREATE: usize = MAX_SETTLED - SETTLED_CREATE_HEADROOM;

/// Snapshot the caller assembled under the maintenance lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionSnapshot {
    pub total_bytes: u64,
    pub reserved_bytes: u64,
    /// Runs still counted as active (unsettled / young). Compared to [`A`].
    pub active_runs: u64,
    /// Digests currently listed in `protected.json`.
    pub protected_count: usize,
    /// Directory entries in the relevant listing (bounded walk, cap [`D`]).
    pub directory_entries: usize,
    /// Create-class reservations already in flight.
    pub create_reservations: usize,
    /// Settled tombs already present.
    pub settled_count: usize,
    /// Current `.counters.json` size under the create-admission ceiling.
    pub create_ledger_bytes: usize,
}

/// Why admission refused a producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDeny {
    DiskPressure {
        projected: u64,
        limit: u64,
        addition: u64,
    },
    ActiveRunLimit {
        active_runs: u64,
        limit: u64,
    },
    ProtectedLimit {
        protected_count: usize,
        limit: usize,
    },
    DirectoryEntryLimit {
        directory_entries: usize,
        limit: usize,
    },
    CreateReservationSlots {
        create_reservations: usize,
        limit: usize,
    },
    SettledSlotsReserved {
        settled_count: usize,
        limit: usize,
    },
    CreateLedgerHeadroom {
        create_ledger_bytes: usize,
        limit: usize,
    },
}

/// Outcome of a pure admission check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDecision {
    Allow,
    Deny(AdmissionDeny),
}

impl AdmissionDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Projected byte total if `producer` reserves its mapped `max_bytes`.
pub fn projected_bytes(snapshot: &AdmissionSnapshot, producer: Producer) -> u64 {
    let addition = mapping_for(producer).max_bytes;
    snapshot
        .total_bytes
        .saturating_add(snapshot.reserved_bytes)
        .saturating_add(addition)
}

/// Pure admission: no I/O, no lock acquisition, no counter mutation.
///
/// Delete / unprotect is always allowed on the logical quota axis (physical
/// ENOSPC remains an I/O failure elsewhere). Capacity overrun denials emit
/// [`DISK_PRESSURE_EVENT`].
pub fn admit(producer: Producer, snapshot: &AdmissionSnapshot) -> AdmissionDecision {
    let mapping = mapping_for(producer);

    if mapping.action == ReservationAction::Delete {
        return AdmissionDecision::Allow;
    }

    if snapshot.directory_entries > D {
        return AdmissionDecision::Deny(AdmissionDeny::DirectoryEntryLimit {
            directory_entries: snapshot.directory_entries,
            limit: D,
        });
    }

    if mapping.kind == ReservationKind::Run && snapshot.active_runs >= A as u64 {
        return AdmissionDecision::Deny(AdmissionDeny::ActiveRunLimit {
            active_runs: snapshot.active_runs,
            limit: A as u64,
        });
    }

    if producer == Producer::Protect && snapshot.protected_count >= P_MAX {
        return AdmissionDecision::Deny(AdmissionDeny::ProtectedLimit {
            protected_count: snapshot.protected_count,
            limit: P_MAX,
        });
    }

    if snapshot.create_reservations >= MAX_CREATE_RESERVATIONS {
        return AdmissionDecision::Deny(AdmissionDeny::CreateReservationSlots {
            create_reservations: snapshot.create_reservations,
            limit: MAX_CREATE_RESERVATIONS,
        });
    }

    if snapshot.settled_count >= MAX_SETTLED_FOR_CREATE {
        return AdmissionDecision::Deny(AdmissionDeny::SettledSlotsReserved {
            settled_count: snapshot.settled_count,
            limit: MAX_SETTLED_FOR_CREATE,
        });
    }

    if snapshot.create_ledger_bytes >= MAX_COUNTERS_BYTES_FOR_CREATE {
        return AdmissionDecision::Deny(AdmissionDeny::CreateLedgerHeadroom {
            create_ledger_bytes: snapshot.create_ledger_bytes,
            limit: MAX_COUNTERS_BYTES_FOR_CREATE,
        });
    }

    let addition = mapping.max_bytes;
    let projected = snapshot
        .total_bytes
        .saturating_add(snapshot.reserved_bytes)
        .saturating_add(addition);
    let limit = peak_capacity_default();
    if projected > limit {
        emit_disk_pressure(producer, projected, limit, addition);
        return AdmissionDecision::Deny(AdmissionDeny::DiskPressure {
            projected,
            limit,
            addition,
        });
    }

    AdmissionDecision::Allow
}

fn emit_disk_pressure(producer: Producer, projected: u64, limit: u64, addition: u64) {
    warn!(
        event = DISK_PRESSURE_EVENT,
        ?producer,
        projected,
        limit,
        addition,
        "restricted-root capacity admission refused"
    );
}

const _: () = {
    assert!(SETTLED_CREATE_HEADROOM == 8);
    assert!(MAX_SETTLED_FOR_CREATE == 56);
    assert!(A == 2);
    assert!(D == 1000);
    assert!(P_MAX == 20);
};

#[cfg(test)]
mod admission {
    //! UN-58: pure admission judgment — projected capacity, A/P/D gates, delete
    //! always allowed, and the frozen disk-pressure event name.

    use crate::contract::policy::{
        secure_admission::{
            AdmissionDecision, AdmissionDeny, AdmissionSnapshot, DISK_PRESSURE_EVENT,
            MAX_SETTLED_FOR_CREATE, SETTLED_CREATE_HEADROOM, admit, projected_bytes,
        },
        secure_capacity::{A, D, MIB, P_MAX, peak_capacity_default},
        secure_counter::MAX_COUNTERS_BYTES_FOR_CREATE,
        secure_producer::Producer,
    };

    fn empty_snapshot() -> AdmissionSnapshot {
        AdmissionSnapshot {
            total_bytes: 0,
            reserved_bytes: 0,
            active_runs: 0,
            protected_count: 0,
            directory_entries: 0,
            create_reservations: 0,
            settled_count: 0,
            create_ledger_bytes: 0,
        }
    }

    #[test]
    fn un58_admission_allows_just_under_peak() {
        let peak = peak_capacity_default();
        let addition = 3 * MIB; // bootstrap-candidate
        let snap = AdmissionSnapshot {
            total_bytes: peak - addition,
            reserved_bytes: 0,
            ..empty_snapshot()
        };
        assert_eq!(projected_bytes(&snap, Producer::BootstrapCandidate), peak);
        assert!(admit(Producer::BootstrapCandidate, &snap).is_allow());
    }

    #[test]
    fn un58_admission_rejects_single_run_that_would_exceed_peak() {
        let peak = peak_capacity_default();
        let snap = AdmissionSnapshot {
            total_bytes: peak - MIB, // 1 MiB free; compare wants 5 MiB
            reserved_bytes: 0,
            ..empty_snapshot()
        };
        match admit(Producer::Compare, &snap) {
            AdmissionDecision::Deny(AdmissionDeny::DiskPressure {
                projected,
                limit,
                addition,
            }) => {
                assert_eq!(limit, peak);
                assert_eq!(addition, 5 * MIB);
                assert!(projected > peak);
            }
            other => panic!("expected disk pressure deny, got {other:?}"),
        }
    }

    #[test]
    fn un58_admission_rejects_active_run_and_protected_and_directory_limits() {
        let mut snap = empty_snapshot();
        snap.active_runs = A as u64;
        assert!(matches!(
            admit(Producer::BootstrapCandidate, &snap),
            AdmissionDecision::Deny(AdmissionDeny::ActiveRunLimit { .. })
        ));
        // Non-run producers are not gated by A.
        assert!(admit(Producer::SweepReport, &snap).is_allow());

        snap = empty_snapshot();
        snap.protected_count = P_MAX;
        assert!(matches!(
            admit(Producer::Protect, &snap),
            AdmissionDecision::Deny(AdmissionDeny::ProtectedLimit { .. })
        ));

        snap = empty_snapshot();
        snap.directory_entries = D + 1;
        assert!(matches!(
            admit(Producer::Compare, &snap),
            AdmissionDecision::Deny(AdmissionDeny::DirectoryEntryLimit { .. })
        ));
    }

    #[test]
    fn un58_admission_unprotect_always_allowed_on_logical_quota() {
        let peak = peak_capacity_default();
        let snap = AdmissionSnapshot {
            total_bytes: peak,
            reserved_bytes: peak,
            active_runs: A as u64,
            protected_count: P_MAX,
            directory_entries: D + 50,
            create_reservations: 64,
            settled_count: 64,
            create_ledger_bytes: MAX_COUNTERS_BYTES_FOR_CREATE + 1,
        };
        assert!(admit(Producer::Unprotect, &snap).is_allow());
    }

    #[test]
    fn un58_admission_reserves_settled_and_create_ledger_headroom() {
        assert_eq!(SETTLED_CREATE_HEADROOM, 8);
        assert_eq!(MAX_SETTLED_FOR_CREATE, 56);

        let mut snap = empty_snapshot();
        snap.settled_count = MAX_SETTLED_FOR_CREATE; // 56: one more settle would project to 57
        assert!(matches!(
            admit(Producer::Promote, &snap),
            AdmissionDecision::Deny(AdmissionDeny::SettledSlotsReserved { .. })
        ));
        snap.settled_count = MAX_SETTLED_FOR_CREATE - 1;
        assert!(admit(Producer::Promote, &snap).is_allow());

        snap = empty_snapshot();
        snap.create_ledger_bytes = MAX_COUNTERS_BYTES_FOR_CREATE;
        assert!(matches!(
            admit(Producer::SweepReport, &snap),
            AdmissionDecision::Deny(AdmissionDeny::CreateLedgerHeadroom { .. })
        ));
    }

    #[test]
    fn un58_admission_disk_pressure_event_name_is_frozen() {
        assert_eq!(DISK_PRESSURE_EVENT, "audit_retention_disk_pressure");
    }
}
