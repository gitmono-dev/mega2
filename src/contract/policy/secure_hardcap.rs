//! UN-59: admission wiring for run create and sweep-report writes.
//!
//! Hard-ceiling *values* and [`hard_cap_violation`] live in [`secure_capacity`]
//! (UN-54). This module is the write-path integration that calls UN-57 under
//! the maintenance lock and settles immediately.

use std::time::{SystemTime, UNIX_EPOCH};

use rand::RngExt;

use crate::contract::policy::{
    secure_artifact::{
        ArtifactError, ArtifactResult, RUNS_DIR, RestrictedRoot, RunDir, SWEEP_REPORTS_DIR,
        generate_run_id, write_exclusive_under,
    },
    secure_capacity::{WriteClass, hard_cap_violation},
    secure_lifecycle::{
        LeaseView, NoLeases, ReserveRequest, SettleRequest, abort_locked, admit_and_reserve_locked,
        commit_locked,
    },
    secure_producer::Producer,
    secure_sweep::MaintenanceLock,
};

/// A run directory claimed under an active reservation that must be settled.
#[derive(Debug)]
pub struct ReservedRun {
    pub run: RunDir,
    pub op_id: String,
    pub producer: Producer,
}

impl ReservedRun {
    /// Admit + claim a run directory under the held maintenance lock.
    ///
    /// `owner_fenced=false` for audit runs (`bootstrap-candidate` / `compare`).
    pub fn create(
        root: &RestrictedRoot,
        lock: &MaintenanceLock,
        producer: Producer,
        leases: &dyn LeaseView,
        now: SystemTime,
        active_runs: u64,
        directory_entries: usize,
    ) -> ArtifactResult<Self> {
        match producer {
            Producer::BootstrapCandidate | Producer::Compare => {}
            other => {
                return Err(ArtifactError::LifecycleRejected {
                    reason: format!("{other:?} is not an audit run-create producer"),
                });
            }
        }

        let mut last_err = None;
        for _ in 0..8 {
            let run_id = generate_run_id();
            let op_id = new_op_id();
            let target = format!("{RUNS_DIR}/{run_id}");
            match admit_and_reserve_locked(
                root,
                lock,
                producer,
                ReserveRequest {
                    op_id: op_id.clone(),
                    target,
                    payload: String::new(),
                    owner_fenced: Some(false),
                    created_at: rfc3339_utc(now),
                    active_runs,
                    protected_count: 0,
                    directory_entries,
                },
                leases,
                now,
            ) {
                Ok(_) => match RunDir::claim_exact(root, &run_id) {
                    Ok(run) => {
                        return Ok(Self {
                            run,
                            op_id,
                            producer,
                        });
                    }
                    Err(err) => {
                        let _ = abort_locked(
                            root,
                            lock,
                            SettleRequest {
                                op_id,
                                settled_delta: 0,
                                final_state: "aborted".into(),
                                settled_at: rfc3339_utc(now),
                                run_id: None,
                                cap_hash: None,
                            },
                        );
                        last_err = Some(err);
                    }
                },
                Err(err) => return Err(err),
            }
        }
        Err(last_err.unwrap_or(ArtifactError::RunIdExhausted { attempts: 8 }))
    }

    /// Commit the reservation after a successful run write sequence.
    pub fn commit(
        self,
        root: &RestrictedRoot,
        lock: &MaintenanceLock,
        settled_delta: i64,
        now: SystemTime,
    ) -> ArtifactResult<RunDir> {
        commit_locked(
            root,
            lock,
            SettleRequest {
                op_id: self.op_id,
                settled_delta,
                final_state: "committed".into(),
                settled_at: rfc3339_utc(now),
                run_id: None,
                cap_hash: None,
            },
        )?;
        Ok(self.run)
    }

    /// Abort the reservation after a failed run write sequence.
    pub fn abort(
        self,
        root: &RestrictedRoot,
        lock: &MaintenanceLock,
        now: SystemTime,
    ) -> ArtifactResult<()> {
        abort_locked(
            root,
            lock,
            SettleRequest {
                op_id: self.op_id,
                settled_delta: 0,
                final_state: "aborted".into(),
                settled_at: rfc3339_utc(now),
                run_id: None,
                cap_hash: None,
            },
        )?;
        Ok(())
    }
}

/// Reservation for a sweep report that must be settled after the write.
#[derive(Debug)]
pub struct AdmittedSweepReport {
    pub op_id: String,
    pub run_id: String,
}

/// Admit a sweep-report reservation **before** any retention deletion.
pub fn admit_sweep_report(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    run_id: &str,
    bytes_len: usize,
    now: SystemTime,
    directory_entries: usize,
) -> ArtifactResult<AdmittedSweepReport> {
    hard_cap_violation(WriteClass::SweepReportOrEvidence, bytes_len).map_err(
        |(bytes, limit)| ArtifactError::WriteTooLarge {
            class: WriteClass::SweepReportOrEvidence.name(),
            bytes,
            limit,
        },
    )?;
    let op_id = new_op_id();
    admit_and_reserve_locked(
        root,
        lock,
        Producer::SweepReport,
        ReserveRequest {
            op_id: op_id.clone(),
            target: format!("{SWEEP_REPORTS_DIR}/{run_id}.json"),
            payload: String::new(),
            owner_fenced: None,
            created_at: rfc3339_utc(now),
            active_runs: 0,
            protected_count: 0,
            directory_entries,
        },
        &NoLeases,
        now,
    )?;
    Ok(AdmittedSweepReport {
        op_id,
        run_id: run_id.to_string(),
    })
}

/// Abort an admitted sweep-report reservation without writing.
pub fn abort_admitted_sweep_report(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    admitted: AdmittedSweepReport,
    now: SystemTime,
) -> ArtifactResult<()> {
    abort_locked(
        root,
        lock,
        SettleRequest {
            op_id: admitted.op_id,
            settled_delta: 0,
            final_state: "aborted".into(),
            settled_at: rfc3339_utc(now),
            run_id: None,
            cap_hash: None,
        },
    )?;
    Ok(())
}

/// Write + prune + settle an already-admitted sweep report.
pub fn finish_sweep_report(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    admitted: AdmittedSweepReport,
    bytes: &[u8],
    now: SystemTime,
) -> ArtifactResult<String> {
    let file_name = format!("{}.json", admitted.run_id);
    match write_exclusive_under(root, SWEEP_REPORTS_DIR, &file_name, bytes) {
        Ok(()) => {
            // Retention bound is part of a successful report write (UN-49).
            let prune_res = crate::contract::policy::secure_sweep::prune_sweep_reports(root);
            commit_locked(
                root,
                lock,
                SettleRequest {
                    op_id: admitted.op_id,
                    settled_delta: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
                    final_state: "committed".into(),
                    settled_at: rfc3339_utc(now),
                    run_id: None,
                    cap_hash: None,
                },
            )?;
            prune_res?;
            Ok(root
                .display()
                .join(SWEEP_REPORTS_DIR)
                .join(file_name)
                .display()
                .to_string())
        }
        Err(err) => {
            let _ = abort_locked(
                root,
                lock,
                SettleRequest {
                    op_id: admitted.op_id,
                    settled_delta: 0,
                    final_state: "aborted".into(),
                    settled_at: rfc3339_utc(now),
                    run_id: None,
                    cap_hash: None,
                },
            );
            Err(err)
        }
    }
}

/// Admit, write, prune, and settle a sweep report (no intervening deletions).
pub fn persist_sweep_report_admitted(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    run_id: &str,
    bytes: &[u8],
    now: SystemTime,
    directory_entries: usize,
) -> ArtifactResult<String> {
    let admitted = admit_sweep_report(root, lock, run_id, bytes.len(), now, directory_entries)?;
    finish_sweep_report(root, lock, admitted, bytes, now)
}

pub fn new_op_id() -> String {
    let n: u128 = rand::rng().random();
    format!("01UN{n:026x}")
}

pub fn rfc3339_utc(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = secs / 86400;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(all(test, target_os = "linux"))]
mod write_hardcap {
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
}
