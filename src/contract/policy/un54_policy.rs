//! UN-54: capacity formula and frozen constants must not drift from the plan's
//! canonical string or from the values already enforced by sweep / counter.

use crate::contract::policy::{
    secure_capacity::{
        A, CAPACITY_FORMULA, D, FIXED_METADATA_BYTES, K, MAX_CANDIDATE_OR_BASELINE_BYTES,
        MAX_RESTRICTED_DIFF_BYTES, MAX_SANITIZED_REPORT_BYTES, MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES,
        MIB, N, P_MAX, PROMOTE_ADMISSION_BYTES, R, peak_capacity_bytes, peak_capacity_default,
        promote_admission_bytes, saturated_version_plane_bytes,
    },
    secure_counter::{
        COUNTERS_DELETE_HEADROOM, MAX_COUNTERS_BYTES, MAX_COUNTERS_BYTES_FOR_CREATE,
        MAX_CREATE_RESERVATIONS, MAX_DELETE_SETTLED, MAX_RECONCILE_DIR_ENTRIES, MAX_SETTLED,
    },
    secure_sweep::{MAX_BASELINE_VERSIONS, MAX_REPORT_BYTES, MAX_RUN_BATCHES, MAX_SWEEP_REPORTS},
};

#[test]
fn un54_policy_formula_string_is_canonical() {
    assert_eq!(
        CAPACITY_FORMULA,
        "5 MiB × (N + A) + 2 MiB × (K + P + 1 current + 1 temporary) + 256 KiB × R + 64 KiB 固定元数据"
    );
}

#[test]
fn un54_policy_constants_match_sweep_and_counter() {
    assert_eq!(N, MAX_RUN_BATCHES);
    assert_eq!(K, MAX_BASELINE_VERSIONS);
    assert_eq!(R, MAX_SWEEP_REPORTS);
    assert_eq!(D, MAX_RECONCILE_DIR_ENTRIES);
    assert_eq!(
        MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES as usize,
        MAX_REPORT_BYTES
    );
    assert_eq!(MAX_CREATE_RESERVATIONS, 64);
    assert_eq!(MAX_SETTLED, 64);
    assert_eq!(MAX_DELETE_SETTLED, 16);
    assert_eq!(MAX_COUNTERS_BYTES, 48 * 1024);
    assert_eq!(COUNTERS_DELETE_HEADROOM, 8 * 1024);
    assert_eq!(MAX_COUNTERS_BYTES_FOR_CREATE, 40 * 1024);
    assert_eq!(A, 2);
    assert_eq!(P_MAX, 20);
    assert_eq!(FIXED_METADATA_BYTES, 64 * 1024);
    assert_eq!(MAX_CANDIDATE_OR_BASELINE_BYTES, 2 * MIB);
    assert_eq!(MAX_SANITIZED_REPORT_BYTES, MIB);
    assert_eq!(MAX_RESTRICTED_DIFF_BYTES, 4 * MIB);
}

#[test]
fn un54_policy_promote_admission_always_two_mib() {
    assert_eq!(promote_admission_bytes(0), PROMOTE_ADMISSION_BYTES);
    assert_eq!(promote_admission_bytes(1), PROMOTE_ADMISSION_BYTES);
    assert_eq!(promote_admission_bytes(MIB), PROMOTE_ADMISSION_BYTES);
    assert_eq!(promote_admission_bytes(2 * MIB), PROMOTE_ADMISSION_BYTES);
    assert_eq!(
        promote_admission_bytes(2 * MIB + 1),
        PROMOTE_ADMISSION_BYTES
    );
}

#[test]
fn un54_policy_peak_matches_formula_substitution() {
    let expected = 5 * MIB * (N + A) as u64
        + 2 * MIB * (K + P_MAX + 1 + 1) as u64
        + 256 * 1024 * R as u64
        + FIXED_METADATA_BYTES;
    assert_eq!(peak_capacity_default(), expected);
    assert_eq!(peak_capacity_bytes(N, A, K, P_MAX, R), expected);
}

#[test]
fn un54_policy_promotion_boundary_when_version_plane_saturated() {
    // Saturated version plane = K + P + current + temporary, each charged at 2 MiB.
    let versions = saturated_version_plane_bytes(K, P_MAX);
    assert_eq!(versions, 2 * MIB * (K + P_MAX + 2) as u64);

    let peak = peak_capacity_default();
    let base_without_versions = peak.saturating_sub(versions);
    // At the formula ceiling for versions, another promote charge still fits
    // only because the peak already budgets exactly that plane — exceeding the
    // plane (K+P+current+temp+1) would push projected versions over the term.
    let over_plane = versions.saturating_add(promote_admission_bytes(0));
    let projected = base_without_versions.saturating_add(over_plane);
    assert!(
        projected > peak,
        "a promote beyond K+P+current+temporary must exceed the frozen peak"
    );
}
