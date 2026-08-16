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
