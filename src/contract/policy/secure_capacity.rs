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
