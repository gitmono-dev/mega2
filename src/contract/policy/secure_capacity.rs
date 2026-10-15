//! UN-54: the single freeze point for restricted-root capacity formula and
//! constants. Admission (UN-58), hard write ceilings at write time (UN-59), and
//! producer mapping (UN-60) all consume these values; they do not redefine them.

use crate::contract::policy::secure_counter::{
    COUNTERS_DELETE_HEADROOM, MAX_COUNTERS_BYTES, MAX_CREATE_RESERVATIONS, MAX_DELETE_SETTLED,
    MAX_SETTLED,
};

/// Canonical peak-capacity formula (byte-identical with the plan's「性能与容量摘要」).
pub const CAPACITY_FORMULA: &str = concat!(
    "5 MiB × (N + A) + 2 MiB × (K + P + 1 current + 1 temporary)",
    " + 256 KiB × R + 64 KiB 固定元数据"
);

/// Run retention batches (N).
pub const N: usize = 20;
/// Baseline versions retained, excluding protected (K).
pub const K: usize = 10;
/// Active-run ceiling (A).
pub const A: usize = 2;
/// Protected-manifest entry ceiling (P).
pub const P_MAX: usize = 20;
/// Sweep-report retention (R).
pub const R: usize = 100;
/// Directory entry ceiling examined by admission / reconcile (D).
pub const D: usize = 1000;

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;

/// Fixed metadata budget (locks / counter / protected manifest), counted in the peak.
pub const FIXED_METADATA_BYTES: u64 = 64 * KIB;
/// Promote admission always charges this quantum (never the raw byte length).
pub const PROMOTE_ADMISSION_BYTES: u64 = 2 * MIB;

/// Per-type write ceilings (enforcement is UN-59; values freeze here).
pub const MAX_CANDIDATE_OR_BASELINE_BYTES: u64 = 2 * MIB;
pub const MAX_SANITIZED_REPORT_BYTES: u64 = MIB;
pub const MAX_RESTRICTED_DIFF_BYTES: u64 = 4 * MIB;
pub const MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES: u64 = 256 * KIB;

/// Peak capacity for a concrete (N, A, K, P, R) substitution of [`CAPACITY_FORMULA`].
pub fn peak_capacity_bytes(n: usize, a: usize, k: usize, p: usize, r: usize) -> u64 {
    let run_slots = (n as u64).saturating_add(a as u64);
    let runs = 5 * MIB * run_slots;
    // K + P + 1 current + 1 temporary — parentheses keep the quantum outside the sum.
    let version_slots = (k as u64).saturating_add(p as u64).saturating_add(2);
    let versions = 2 * MIB * version_slots;
    let reports = 256 * KIB * (r as u64);
    runs.saturating_add(versions)
        .saturating_add(reports)
        .saturating_add(FIXED_METADATA_BYTES)
}

/// Peak capacity under the frozen default parameters (P = [`P_MAX`]).
pub fn peak_capacity_default() -> u64 {
    peak_capacity_bytes(N, A, K, P_MAX, R)
}

/// Promote admission charge: always the 2 MiB quantum, regardless of payload size.
pub fn promote_admission_bytes(_payload_len: u64) -> u64 {
    PROMOTE_ADMISSION_BYTES
}

/// Bytes already spoken for by a saturated version plane (K + P + current + temporary).
pub fn saturated_version_plane_bytes(k: usize, p: usize) -> u64 {
    2 * MIB * (k as u64).saturating_add(p as u64).saturating_add(2)
}

/// Per-type write class (ceiling values freeze here; enforcement is UN-59).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteClass {
    CandidateOrBaseline,
    SanitizedReport,
    RestrictedDiff,
    SweepReportOrEvidence,
}

impl WriteClass {
    pub const fn name(self) -> &'static str {
        match self {
            Self::CandidateOrBaseline => "candidate/baseline",
            Self::SanitizedReport => "sanitized-report",
            Self::RestrictedDiff => "restricted-diff",
            Self::SweepReportOrEvidence => "sweep-report/evidence",
        }
    }

    pub const fn limit_bytes(self) -> usize {
        match self {
            Self::CandidateOrBaseline => MAX_CANDIDATE_OR_BASELINE_BYTES as usize,
            Self::SanitizedReport => MAX_SANITIZED_REPORT_BYTES as usize,
            Self::RestrictedDiff => MAX_RESTRICTED_DIFF_BYTES as usize,
            Self::SweepReportOrEvidence => MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES as usize,
        }
    }
}

/// Fail-closed size check predicate used by UN-59 write sites.
///
/// Returns `Err((bytes, limit))` when over the ceiling so call sites can map
/// into their own error type without a module cycle.
pub fn hard_cap_violation(class: WriteClass, bytes: usize) -> Result<(), (usize, usize)> {
    let limit = class.limit_bytes();
    if bytes > limit {
        Err((bytes, limit))
    } else {
        Ok(())
    }
}

/// Infer a write class from a bare output name (best-effort for legacy callers).
pub fn infer_write_class(name: &str) -> WriteClass {
    let lower = name.to_ascii_lowercase();
    if lower.contains("diff") {
        WriteClass::RestrictedDiff
    } else if lower.contains("evidence") || lower.contains("killswitch") {
        WriteClass::SweepReportOrEvidence
    } else if lower.contains("sanitized") || lower == "report.json" {
        WriteClass::SanitizedReport
    } else {
        WriteClass::CandidateOrBaseline
    }
}

const _: () = {
    assert!(N == 20);
    assert!(K == 10);
    assert!(A == 2);
    assert!(P_MAX == 20);
    assert!(R == 100);
    assert!(D == 1000);
    assert!(FIXED_METADATA_BYTES == 64 * 1024);
    assert!(PROMOTE_ADMISSION_BYTES == 2 * 1024 * 1024);
    assert!(MAX_CANDIDATE_OR_BASELINE_BYTES == 2 * 1024 * 1024);
    assert!(MAX_SANITIZED_REPORT_BYTES == 1024 * 1024);
    assert!(MAX_RESTRICTED_DIFF_BYTES == 4 * 1024 * 1024);
    assert!(MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES == 256 * 1024);
    // Ledger bounds owned by UN-51; registered here so the policy card cannot drift.
    assert!(MAX_CREATE_RESERVATIONS == 64);
    assert!(MAX_SETTLED == 64);
    assert!(MAX_DELETE_SETTLED == 16);
    assert!(MAX_COUNTERS_BYTES == 48 * 1024);
    assert!(COUNTERS_DELETE_HEADROOM == 8 * 1024);
};

#[cfg(test)]
mod capacity_policy {
    //! UN-54: capacity formula and frozen constants must not drift from the plan's
    //! canonical string or from the values already enforced by sweep / counter.

    use crate::contract::policy::{
        secure_capacity::{
            A, CAPACITY_FORMULA, D, FIXED_METADATA_BYTES, K, MAX_CANDIDATE_OR_BASELINE_BYTES,
            MAX_RESTRICTED_DIFF_BYTES, MAX_SANITIZED_REPORT_BYTES,
            MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES, MIB, N, P_MAX, PROMOTE_ADMISSION_BYTES, R,
            peak_capacity_bytes, peak_capacity_default, promote_admission_bytes,
            saturated_version_plane_bytes,
        },
        secure_counter::{
            COUNTERS_DELETE_HEADROOM, MAX_COUNTERS_BYTES, MAX_COUNTERS_BYTES_FOR_CREATE,
            MAX_CREATE_RESERVATIONS, MAX_DELETE_SETTLED, MAX_RECONCILE_DIR_ENTRIES, MAX_SETTLED,
        },
        secure_sweep::{
            MAX_BASELINE_VERSIONS, MAX_REPORT_BYTES, MAX_RUN_BATCHES, MAX_SWEEP_REPORTS,
        },
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
}
