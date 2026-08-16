//! UN-60: the single freeze point mapping each restricted-root write producer
//! onto reservation parameters. Lifecycle (UN-57) and admission (UN-58) look
//! the row up; they do not redefine it.

use crate::contract::policy::{
    secure_capacity::{
        FIXED_METADATA_BYTES, MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES, MIB, PROMOTE_ADMISSION_BYTES,
    },
    secure_counter::{ReservationAction, ReservationKind},
};

/// Named write producers that may reserve capacity under a restricted root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Producer {
    BootstrapCandidate,
    Compare,
    Promote,
    SweepReport,
    KillSwitchEvidence,
    Protect,
    Unprotect,
}

/// Whether protect-family recovery treats "digest present" as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectExpectedState {
    /// `protect` / action=create: digest must exist when settled.
    DigestPresent,
    /// `unprotect` / action=delete: digest must be absent when settled.
    DigestAbsent,
}

/// One frozen row of the producer → reservation parameter table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerMapping {
    pub producer: Producer,
    pub kind: ReservationKind,
    pub action: ReservationAction,
    /// Maximum bytes reserved up front (`0` for delete / unprotect).
    pub max_bytes: u64,
    /// Root-relative target template (placeholders are documentary).
    pub target: &'static str,
    /// Human-readable expected end state after a successful settle.
    pub expected_state: &'static str,
    /// Bytes added to `reserved_bytes` at reserve time (`0` for delete).
    pub reserved_delta: i64,
    /// How `settled_delta` is derived once the write finishes.
    pub settled_delta_rule: SettledDeltaRule,
    /// Which later card / mode settles this producer.
    pub settler: &'static str,
}

/// Settled-delta accounting rule frozen per producer row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettledDeltaRule {
    /// Charge the bytes actually written into runs/total (or reports/versions).
    ActualBytesWritten,
    /// Signed `after_len − before_len` (protect / unprotect); never negated twice.
    SignedLengthDelta,
}

/// The complete frozen table (order matches the plan card).
pub const PRODUCER_MAPPINGS: &[ProducerMapping] = &[
    ProducerMapping {
        producer: Producer::BootstrapCandidate,
        kind: ReservationKind::Run,
        action: ReservationAction::Create,
        max_bytes: 3 * MIB,
        target: "runs/<run-id>/",
        expected_state: "run directory fully written",
        reserved_delta: (3 * MIB) as i64,
        settled_delta_rule: SettledDeltaRule::ActualBytesWritten,
        settler: "mode-builtin (settle on write)",
    },
    ProducerMapping {
        producer: Producer::Compare,
        kind: ReservationKind::Run,
        action: ReservationAction::Create,
        max_bytes: 5 * MIB,
        target: "runs/<run-id>/",
        expected_state: "run directory fully written",
        reserved_delta: (5 * MIB) as i64,
        settled_delta_rule: SettledDeltaRule::ActualBytesWritten,
        settler: "mode-builtin (settle on write)",
    },
    ProducerMapping {
        producer: Producer::Promote,
        kind: ReservationKind::Promotion,
        action: ReservationAction::Create,
        max_bytes: PROMOTE_ADMISSION_BYTES,
        target: "baselines/<digest>.json + pointer",
        expected_state: "version file present and pointer switched",
        reserved_delta: PROMOTE_ADMISSION_BYTES as i64,
        settled_delta_rule: SettledDeltaRule::ActualBytesWritten,
        settler: "UN-35",
    },
    ProducerMapping {
        producer: Producer::SweepReport,
        kind: ReservationKind::Report,
        action: ReservationAction::Create,
        max_bytes: MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES,
        target: "sweep-reports/<run-id>.json",
        expected_state: "report present",
        reserved_delta: MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES as i64,
        settled_delta_rule: SettledDeltaRule::ActualBytesWritten,
        settler: "UN-59 (sweep holds maintenance lock)",
    },
    ProducerMapping {
        producer: Producer::KillSwitchEvidence,
        kind: ReservationKind::Run,
        action: ReservationAction::Create,
        max_bytes: MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES,
        target: "runs/<run-id>/killswitch-evidence.json",
        expected_state: "evidence written and settled",
        reserved_delta: MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES as i64,
        settled_delta_rule: SettledDeltaRule::ActualBytesWritten,
        settler: "UN-52 run-commit/run-abort",
    },
    ProducerMapping {
        producer: Producer::Protect,
        kind: ReservationKind::Protect,
        action: ReservationAction::Create,
        max_bytes: FIXED_METADATA_BYTES,
        target: "baselines/protected.json",
        expected_state: "digest present",
        reserved_delta: FIXED_METADATA_BYTES as i64,
        settled_delta_rule: SettledDeltaRule::SignedLengthDelta,
        settler: "UN-55 mode-builtin",
    },
    ProducerMapping {
        producer: Producer::Unprotect,
        kind: ReservationKind::Protect,
        action: ReservationAction::Delete,
        max_bytes: 0,
        target: "baselines/protected.json",
        expected_state: "digest absent",
        reserved_delta: 0,
        settled_delta_rule: SettledDeltaRule::SignedLengthDelta,
        settler: "UN-55 mode-builtin",
    },
];

/// Look up the frozen mapping for `producer`.
pub fn mapping_for(producer: Producer) -> &'static ProducerMapping {
    PRODUCER_MAPPINGS
        .iter()
        .find(|row| row.producer == producer)
        .expect("every Producer variant has a frozen mapping row")
}

/// Protect-family recovery expected state (opposite for create vs delete).
pub fn protect_expected_state(producer: Producer) -> Option<ProtectExpectedState> {
    match producer {
        Producer::Protect => Some(ProtectExpectedState::DigestPresent),
        Producer::Unprotect => Some(ProtectExpectedState::DigestAbsent),
        _ => None,
    }
}

/// Whether a observed digest presence matches the producer's recovery rule.
pub fn protect_recovery_satisfied(producer: Producer, digest_present: bool) -> bool {
    match protect_expected_state(producer) {
        Some(ProtectExpectedState::DigestPresent) => digest_present,
        Some(ProtectExpectedState::DigestAbsent) => !digest_present,
        None => false,
    }
}

/// Signed length delta used by protect / unprotect settlement.
pub fn signed_length_delta(before_len: u64, after_len: u64) -> Option<i64> {
    i64::try_from(after_len)
        .ok()
        .zip(i64::try_from(before_len).ok())
        .and_then(|(after, before)| after.checked_sub(before))
}

const _: () = {
    assert!(PRODUCER_MAPPINGS.len() == 7);
};
