//! UN-60: producer mapping rows must match the plan card byte-for-byte on the
//! parameters later cards will look up.

use crate::contract::policy::{
    secure_capacity::{
        FIXED_METADATA_BYTES, MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES, MIB, PROMOTE_ADMISSION_BYTES,
    },
    secure_counter::{ReservationAction, ReservationKind},
    secure_producer::{
        PRODUCER_MAPPINGS, Producer, ProtectExpectedState, SettledDeltaRule, mapping_for,
        protect_expected_state, protect_recovery_satisfied, signed_length_delta,
    },
};

#[test]
fn un60_mapping_table_has_seven_frozen_rows() {
    assert_eq!(PRODUCER_MAPPINGS.len(), 7);
    let producers: Vec<_> = PRODUCER_MAPPINGS.iter().map(|r| r.producer).collect();
    assert_eq!(
        producers,
        vec![
            Producer::BootstrapCandidate,
            Producer::Compare,
            Producer::Promote,
            Producer::SweepReport,
            Producer::KillSwitchEvidence,
            Producer::Protect,
            Producer::Unprotect,
        ]
    );
}

#[test]
fn un60_mapping_bootstrap_candidate_row() {
    let row = mapping_for(Producer::BootstrapCandidate);
    assert_eq!(row.kind, ReservationKind::Run);
    assert_eq!(row.action, ReservationAction::Create);
    assert_eq!(row.max_bytes, 3 * MIB);
    assert_eq!(row.target, "runs/<run-id>/");
    assert_eq!(row.reserved_delta, (3 * MIB) as i64);
    assert_eq!(row.settled_delta_rule, SettledDeltaRule::ActualBytesWritten);
}

#[test]
fn un60_mapping_compare_row() {
    let row = mapping_for(Producer::Compare);
    assert_eq!(row.kind, ReservationKind::Run);
    assert_eq!(row.action, ReservationAction::Create);
    assert_eq!(row.max_bytes, 5 * MIB);
    assert_eq!(row.target, "runs/<run-id>/");
    assert_eq!(row.reserved_delta, (5 * MIB) as i64);
}

#[test]
fn un60_mapping_promote_row() {
    let row = mapping_for(Producer::Promote);
    assert_eq!(row.kind, ReservationKind::Promotion);
    assert_eq!(row.action, ReservationAction::Create);
    assert_eq!(row.max_bytes, PROMOTE_ADMISSION_BYTES);
    assert_eq!(row.max_bytes, 2 * MIB);
    assert_eq!(row.target, "baselines/<digest>.json + pointer");
    assert_eq!(row.settler, "UN-35");
}

#[test]
fn un60_mapping_sweep_report_row() {
    let row = mapping_for(Producer::SweepReport);
    assert_eq!(row.kind, ReservationKind::Report);
    assert_eq!(row.action, ReservationAction::Create);
    assert_eq!(row.max_bytes, MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES);
    assert_eq!(row.target, "sweep-reports/<run-id>.json");
    assert!(row.settler.contains("UN-59"));
}

#[test]
fn un60_mapping_killswitch_evidence_row() {
    let row = mapping_for(Producer::KillSwitchEvidence);
    assert_eq!(row.kind, ReservationKind::Run);
    assert_eq!(row.action, ReservationAction::Create);
    assert_eq!(row.max_bytes, MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES);
    assert_eq!(row.target, "runs/<run-id>/killswitch-evidence.json");
    assert!(row.settler.contains("UN-52"));
}

#[test]
fn un60_mapping_protect_row() {
    let row = mapping_for(Producer::Protect);
    assert_eq!(row.kind, ReservationKind::Protect);
    assert_eq!(row.action, ReservationAction::Create);
    assert_eq!(row.max_bytes, FIXED_METADATA_BYTES);
    assert_eq!(row.target, "baselines/protected.json");
    assert_eq!(row.reserved_delta, FIXED_METADATA_BYTES as i64);
    assert_eq!(row.settled_delta_rule, SettledDeltaRule::SignedLengthDelta);
    assert_eq!(
        protect_expected_state(Producer::Protect),
        Some(ProtectExpectedState::DigestPresent)
    );
}

#[test]
fn un60_mapping_unprotect_row() {
    let row = mapping_for(Producer::Unprotect);
    assert_eq!(row.kind, ReservationKind::Protect);
    assert_eq!(row.action, ReservationAction::Delete);
    assert_eq!(row.max_bytes, 0);
    assert_eq!(row.reserved_delta, 0);
    assert_eq!(row.target, "baselines/protected.json");
    assert_eq!(row.settled_delta_rule, SettledDeltaRule::SignedLengthDelta);
    assert_eq!(
        protect_expected_state(Producer::Unprotect),
        Some(ProtectExpectedState::DigestAbsent)
    );
}

#[test]
fn un60_mapping_protect_unprotect_recovery_are_opposites() {
    assert!(protect_recovery_satisfied(Producer::Protect, true));
    assert!(!protect_recovery_satisfied(Producer::Protect, false));
    assert!(protect_recovery_satisfied(Producer::Unprotect, false));
    assert!(!protect_recovery_satisfied(Producer::Unprotect, true));
    assert!(!protect_recovery_satisfied(Producer::Compare, true));

    // Same signed formula for both actions; never negate twice.
    assert_eq!(signed_length_delta(10, 15), Some(5));
    assert_eq!(signed_length_delta(15, 10), Some(-5));
    assert_eq!(signed_length_delta(10, 10), Some(0));
}
