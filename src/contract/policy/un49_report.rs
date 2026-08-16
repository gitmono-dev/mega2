//! UN-49: the sweep report, tested for the bounds that keep it from becoming
//! another unbounded pile under the restricted root.
//!
//! The interesting failures are not "did we write a file" — they are writing a
//! report that never ends (unbounded illegal names, unbounded entries) or
//! writing a truncated one when the byte ceiling is hit. Each bound is paired
//! with the write that does succeed under the same fixture, because a reporter
//! that writes nothing would pass every "must not grow forever" assertion.

use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::SystemTime};

use crate::contract::policy::{
    secure_artifact::{ArtifactError, BASELINES_DIR, RUNS_DIR, RestrictedRoot, SWEEP_REPORTS_DIR},
    secure_sweep::{
        MAX_ILLEGAL_NAMES_LISTED, MAX_REPORT_BYTES, MAX_REPORT_ENTRIES, MAX_RUN_BATCHES,
        MAX_SWEEP_REPORTS, MaintenanceLock, NoReservations, sweep, un49_report_bytes_for_test,
        un49_write_and_prune_for_test,
    },
};

fn root() -> (tempfile::TempDir, RestrictedRoot) {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = RestrictedRoot::open(temp.path()).expect("open the restricted root");
    (temp, root)
}

fn run_id(ordinal: usize) -> String {
    format!("20260815T10{:04}Z-{ordinal}", ordinal % 10000)
}

fn make_run(temp: &Path, id: &str) {
    let dir = temp.join(RUNS_DIR).join(id);
    fs::create_dir_all(&dir).expect("create the run directory");
    fs::write(dir.join("report.json"), b"{}").expect("write a run output");
}

fn set_mtime(path: &Path, when: SystemTime) {
    let file = fs::File::open(path).expect("open for utime");
    file.set_times(fs::FileTimes::new().set_modified(when))
        .expect("set mtime");
}

#[test]
fn un49_report_lands_at_0600_with_deleted_kept_and_illegal_summary() {
    let (temp, root) = root();
    // One over the retention limit so something is deleted and something kept.
    for i in 0..=MAX_RUN_BATCHES {
        make_run(temp.path(), &run_id(i));
    }
    fs::create_dir_all(temp.path().join(RUNS_DIR).join("not-a-run-id"))
        .expect("plant an illegal name");

    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let outcome = sweep(&root, &lock, &NoReservations, SystemTime::now()).expect("sweep");

    let report_run_id = outcome.report_run_id.expect("report run id");
    let report_path = temp
        .path()
        .join(SWEEP_REPORTS_DIR)
        .join(format!("{report_run_id}.json"));
    assert!(
        report_path.is_file(),
        "report must land under sweep-reports/"
    );
    let mode = fs::metadata(&report_path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "report must be 0600");

    let body: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).expect("read report")).expect("json");
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["run_id"], report_run_id);
    assert_eq!(body["illegal_names"]["total"], 1);
    assert_eq!(
        body["illegal_names"]["sample"][0],
        format!("{RUNS_DIR}/not-a-run-id")
    );

    let actions: Vec<&str> = body["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| e["action"].as_str().expect("action"))
        .collect();
    assert!(
        actions.contains(&"deleted"),
        "a run beyond N must be deleted"
    );
    assert!(actions.contains(&"kept"), "runs within N must be kept");
}

#[test]
fn un49_illegal_names_are_sampled_not_expanded() {
    let mut illegal = Vec::new();
    for i in 0..250 {
        illegal.push(format!("{RUNS_DIR}/weird-{i}"));
    }
    let bytes =
        un49_report_bytes_for_test(Vec::new(), illegal, "20260815T120000Z-0").expect("serialize");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["illegal_names"]["total"], 250);
    assert_eq!(
        body["illegal_names"]["sample"]
            .as_array()
            .expect("sample")
            .len(),
        MAX_ILLEGAL_NAMES_LISTED
    );
}

#[test]
fn un49_entry_cap_stops_appending_at_1000() {
    let mut entries = Vec::new();
    for i in 0..1200 {
        entries.push((
            format!("{RUNS_DIR}/r{i}"),
            "kept",
            "within run retention limit".to_string(),
        ));
    }
    let bytes =
        un49_report_bytes_for_test(entries, Vec::new(), "20260815T120000Z-1").expect("serialize");
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(
        body["entries"].as_array().expect("entries").len(),
        MAX_REPORT_ENTRIES
    );
}

#[test]
fn un49_byte_ceiling_refuses_rather_than_truncating() {
    // One oversized reason string is enough to blow the ceiling without needing
    // thousands of entries — the point is the refusal, not the shape that got
    // there. Sweep calls the same into_bytes gate before execute.
    let huge = "x".repeat(MAX_REPORT_BYTES);
    let err = un49_report_bytes_for_test(
        vec![(format!("{BASELINES_DIR}/note.json"), "kept", huge)],
        Vec::new(),
        "20260815T120000Z-2",
    )
    .expect_err("over-size must fail");
    match err {
        ArtifactError::ReportTooLarge { bytes, limit } => {
            assert!(bytes > limit);
            assert_eq!(limit, MAX_REPORT_BYTES);
        }
        other => panic!("expected ReportTooLarge, got {other}"),
    }
}

#[test]
fn un49_prune_leaves_illegal_json_names_alone() {
    let (temp, root) = root();
    let reports = temp.path().join(SWEEP_REPORTS_DIR);
    fs::create_dir_all(&reports).expect("create sweep-reports");
    fs::write(reports.join("notes.json"), b"do-not-delete").expect("illegal name");
    for i in 0..(MAX_SWEEP_REPORTS + 2) {
        let id = format!("20260815T12{:04}Z-{i}", i);
        fs::write(reports.join(format!("{id}.json")), b"{}").expect("seed");
    }
    un49_write_and_prune_for_test(&root, "20260815T129999Z-1", b"{}").expect("prune");
    assert!(
        reports.join("notes.json").is_file(),
        "illegal names under sweep-reports/ must never be pruned"
    );
    let _ = temp;
}

#[test]
fn un49_report_self_limit_keeps_newest_100_by_mtime_then_name() {
    let (temp, root) = root();
    let reports = temp.path().join(SWEEP_REPORTS_DIR);
    fs::create_dir_all(&reports).expect("create sweep-reports");

    let base = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for i in 0..(MAX_SWEEP_REPORTS + 5) {
        let id = format!("20260815T12{:04}Z-{i}", i);
        let path = reports.join(format!("{id}.json"));
        // Tiny valid-looking body; prune looks at names and mtimes, not schema.
        fs::write(&path, format!("{{\"n\":{i}}}").as_bytes()).expect("seed report");
        set_mtime(&path, base + std::time::Duration::from_secs(i as u64));
    }

    // Writing one more report triggers prune under the real write path.
    un49_write_and_prune_for_test(&root, "20260815T129999Z-999", b"{\"n\":999}")
        .expect("write and prune");

    let mut remaining: Vec<_> = fs::read_dir(&reports)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    remaining.sort();
    assert_eq!(remaining.len(), MAX_SWEEP_REPORTS);
    assert!(
        !remaining
            .iter()
            .any(|n| n.starts_with("20260815T120000Z-0")),
        "oldest reports must be removed first"
    );
    assert!(
        remaining.iter().any(|n| n == "20260815T129999Z-999.json"),
        "the newly written report must survive"
    );
}

#[test]
fn un49_protected_versions_are_recorded_as_skipped_protected() {
    let (temp, root) = root();
    let baselines = temp.path().join(BASELINES_DIR);
    fs::create_dir_all(&baselines).expect("baselines");
    let hex: String = std::iter::repeat_n('a', 64).collect();
    let name = format!("{hex}.json");
    fs::write(baselines.join(&name), b"{\"baseline\":true}").expect("version");
    fs::write(
        baselines.join("current.json"),
        format!(r#"{{"digest":"sha256:{hex}"}}"#).as_bytes(),
    )
    .expect("pointer");

    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let outcome = sweep(&root, &lock, &NoReservations, SystemTime::now()).expect("sweep");
    let report_path = temp.path().join(SWEEP_REPORTS_DIR).join(format!(
        "{}.json",
        outcome.report_run_id.expect("report id")
    ));
    let body: serde_json::Value =
        serde_json::from_slice(&fs::read(report_path).expect("read")).expect("json");
    let protected = body["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .find(|e| e["action"] == "skipped-protected")
        .expect("protected entry");
    assert_eq!(protected["path"], format!("{BASELINES_DIR}/{name}"));
}
