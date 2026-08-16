//! UN-59: write-time hard ceilings and admitted run / sweep-report paths.

use std::time::{Duration, SystemTime};

use crate::contract::policy::{
    secure_artifact::{ArtifactError, RestrictedRoot, RunDir, write_baseline_version},
    secure_capacity::{
        MAX_CANDIDATE_OR_BASELINE_BYTES, MAX_RESTRICTED_DIFF_BYTES, MAX_SANITIZED_REPORT_BYTES,
        MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES, WriteClass, hard_cap_violation, infer_write_class,
    },
    secure_counter::load_counter,
    secure_hardcap::{ReservedRun, persist_sweep_report_admitted},
    secure_lifecycle::NoLeases,
    secure_producer::Producer,
    secure_sweep::{MaintenanceLock, NoReservations, sweep},
};

fn root() -> (tempfile::TempDir, RestrictedRoot) {
    let temp = tempfile::tempdir().expect("temp");
    let root = RestrictedRoot::open(temp.path()).expect("open");
    (temp, root)
}

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000)
}

#[test]
fn un59_hardcap_rejects_each_class_over_ceiling() {
    let cases = [
        (
            WriteClass::CandidateOrBaseline,
            MAX_CANDIDATE_OR_BASELINE_BYTES as usize,
        ),
        (
            WriteClass::SanitizedReport,
            MAX_SANITIZED_REPORT_BYTES as usize,
        ),
        (
            WriteClass::RestrictedDiff,
            MAX_RESTRICTED_DIFF_BYTES as usize,
        ),
        (
            WriteClass::SweepReportOrEvidence,
            MAX_SWEEP_REPORT_OR_EVIDENCE_BYTES as usize,
        ),
    ];
    for (class, limit) in cases {
        assert!(hard_cap_violation(class, limit).is_ok());
        assert!(hard_cap_violation(class, limit + 1).is_err());
    }
    assert_eq!(infer_write_class("diff.json"), WriteClass::RestrictedDiff);
    assert_eq!(
        infer_write_class("report.json"),
        WriteClass::SanitizedReport
    );
    assert_eq!(
        infer_write_class("killswitch-evidence.json"),
        WriteClass::SweepReportOrEvidence
    );
}

#[test]
fn un59_hardcap_write_output_and_baseline_fail_closed() {
    let (_temp, root) = root();
    let run = RunDir::create(&root).expect("run");
    let over = vec![0u8; MAX_CANDIDATE_OR_BASELINE_BYTES as usize + 1];
    let err = run
        .write_output("candidate.json", &over)
        .expect_err("oversize");
    assert!(matches!(err, ArtifactError::WriteTooLarge { .. }));

    let err = write_baseline_version(&root, "deadbeef.json", &over).expect_err("oversize");
    assert!(matches!(err, ArtifactError::WriteTooLarge { .. }));

    // Boundary: exact ceiling is accepted.
    let exact = vec![0u8; MAX_SANITIZED_REPORT_BYTES as usize];
    run.write_output("report.json", &exact).expect("exact");
}

#[test]
fn un59_hardcap_reserved_run_admits_and_settles() {
    let (_temp, root) = root();
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let reserved = ReservedRun::create(
        &root,
        &lock,
        Producer::BootstrapCandidate,
        &NoLeases,
        now(),
        0,
        0,
    )
    .expect("admit+create");
    let op = reserved.op_id.clone();
    reserved
        .run
        .write_output("candidate.json", b"{\"ok\":true}")
        .expect("write");
    let run = reserved.commit(&root, &lock, 12, now()).expect("commit");
    assert!(!run.run_id().is_empty());

    let counter = load_counter(&root, &lock).expect("load");
    assert!(counter.reservations.is_empty());
    assert_eq!(counter.settled.len(), 1);
    assert_eq!(counter.settled[0].op_id, op);
    assert_eq!(counter.total_bytes, 12);
}

#[test]
fn un59_hardcap_sweep_report_path_admits_and_settles() {
    let (_temp, root) = root();
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    // Empty sweep still writes a report under admission.
    let outcome = sweep(&root, &lock, &NoReservations, now()).expect("sweep");
    assert!(outcome.report_path.is_some());
    assert!(outcome.report_run_id.is_some());

    let counter = load_counter(&root, &lock).expect("load");
    assert!(
        counter.reservations.is_empty(),
        "sweep report must settle immediately"
    );
    assert_eq!(counter.settled.len(), 1);
    assert_eq!(
        counter.settled[0].kind,
        crate::contract::policy::secure_counter::ReservationKind::Report
    );

    // Direct admitted write path also works.
    let path = persist_sweep_report_admitted(
        &root,
        &lock,
        "20260816T120000Z-99",
        br#"{"schema_version":1,"run_id":"20260816T120000Z-99","entries":[],"illegal_names":{"total":0,"sample":[]}}"#,
        now(),
        0,
    )
    .expect("direct");
    assert!(path.contains("sweep-reports"));
}
