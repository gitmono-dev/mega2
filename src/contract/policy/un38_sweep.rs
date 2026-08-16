//! UN-38: the retention sweep, tested from the direction that matters.
//!
//! The failure worth guarding against is over-deletion — the sweep removes
//! files from a directory an operator keeps evidence in. So the interesting
//! cases are the ones where something *should not* be removed: a run still in
//! use, a version the current pointer names, a version on the protection
//! manifest, a manifest that cannot be trusted.
//!
//! Each of those is paired with the deletion that does happen under the same
//! conditions, because a sweep that deletes nothing would satisfy every
//! "must not delete" assertion on its own.

use std::{
    fs,
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, SystemTime},
};

use crate::contract::policy::{
    secure_artifact::{BASELINES_DIR, POINTER_NAME, RUNS_DIR, RestrictedRoot},
    secure_sweep::{
        ACTIVE_RUN_MAX_AGE, LEASE_LOCK, MAINTENANCE_LOCK, MAX_BASELINE_VERSIONS, MAX_RUN_BATCHES,
        MaintenanceLock, NoReservations, PROTECTED_MANIFEST, ReservationView, SweepOutcome,
        TEMP_MIN_AGE, sweep,
    },
};

fn root() -> (tempfile::TempDir, RestrictedRoot) {
    let temp = tempfile::tempdir().expect("temp dir");
    let root = RestrictedRoot::open(temp.path()).expect("open the restricted root");
    (temp, root)
}

/// A run id with a controllable ordinal, so "oldest" is a fact of the fixture
/// rather than of how fast the test ran.
fn run_id(ordinal: usize) -> String {
    format!("20260815T10{:04}Z-{ordinal}", ordinal % 10000)
}

fn make_run(temp: &Path, id: &str, age: Duration) -> std::path::PathBuf {
    let dir = temp.join(RUNS_DIR).join(id);
    fs::create_dir_all(&dir).expect("create the run directory");
    fs::write(dir.join("report.json"), b"{}").expect("write a run output");
    set_age(&dir, age);
    dir
}

fn make_version(temp: &Path, hex: &str, age: Duration) -> std::path::PathBuf {
    let dir = temp.join(BASELINES_DIR);
    fs::create_dir_all(&dir).expect("create baselines");
    let path = dir.join(format!("{hex}.json"));
    fs::write(&path, b"{\"baseline\":true}").expect("write a version");
    set_age(&path, age);
    path
}

fn digest(nibble: char) -> String {
    std::iter::repeat_n(nibble, 64).collect()
}

/// Backdate an entry so age thresholds can be exercised without waiting.
fn set_age(path: &Path, age: Duration) {
    set_mtime(path, SystemTime::now() - age);
}

/// Stamp an exact mtime.
///
/// Needed on its own for the tie-break case: `now() - age` is evaluated per
/// call, so "the same age" applied to several entries actually produces
/// distinct nanosecond timestamps and would order them by creation instead.
fn set_mtime(path: &Path, when: SystemTime) {
    let file = fs::File::open(path).expect("open for utime");
    file.set_times(fs::FileTimes::new().set_modified(when))
        .expect("set mtime");
}

fn now() -> SystemTime {
    SystemTime::now()
}

fn sweep_with(root: &RestrictedRoot, reservations: &dyn ReservationView) -> SweepOutcome {
    let lock = MaintenanceLock::acquire(root).expect("take the maintenance lock");
    sweep(root, &lock, reservations, now()).expect("sweep")
}

/// Under the limit, nothing is removed.
///
/// The first thing to establish: the sweep is bounded by a retention count, not
/// by age alone, so a directory that has not reached the limit is left entirely
/// alone however old its contents are.
#[test]
fn un38_a_directory_under_the_limit_is_left_alone() {
    let (temp, root) = root();
    for i in 0..MAX_RUN_BATCHES {
        make_run(temp.path(), &run_id(i), Duration::from_secs(90 * 24 * 3600));
    }

    let outcome = sweep_with(&root, &NoReservations);
    assert!(outcome.removed_runs.is_empty(), "{outcome:?}");
    assert_eq!(
        fs::read_dir(temp.path().join(RUNS_DIR))
            .expect("list runs")
            .count(),
        MAX_RUN_BATCHES
    );
}

/// Over the limit, the oldest go, and exactly the excess.
#[test]
fn un38_only_the_excess_oldest_runs_are_removed() {
    let (temp, root) = root();
    // Ages descend with the index, so run 0 is the oldest.
    for i in 0..MAX_RUN_BATCHES + 3 {
        let age = Duration::from_secs((MAX_RUN_BATCHES + 3 - i) as u64 * 86_400);
        make_run(temp.path(), &run_id(i), age);
    }

    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(
        outcome.removed_runs,
        vec![run_id(0), run_id(1), run_id(2)],
        "exactly the three oldest, oldest first"
    );
    assert_eq!(
        fs::read_dir(temp.path().join(RUNS_DIR))
            .expect("list runs")
            .count(),
        MAX_RUN_BATCHES
    );
    for i in 0..3 {
        assert!(!temp.path().join(RUNS_DIR).join(run_id(i)).exists());
    }
}

/// Equal mtimes are broken by name, not by directory order.
///
/// Which file survives must not depend on the order a directory happens to be
/// listed in — that is not a decision anybody made, and it makes the same
/// fixture behave differently on two filesystems.
#[test]
fn un38_equal_ages_are_broken_by_name() {
    let (temp, root) = root();
    // One timestamp, stamped onto all six after they exist: `now() - age`
    // evaluated per call would differ by nanoseconds and quietly order them by
    // creation, which is exactly what this test is about.
    let same = SystemTime::now() - Duration::from_secs(86_400);
    // Created in a deliberately unhelpful order.
    for i in [5usize, 1, 4, 0, 3, 2] {
        make_run(temp.path(), &run_id(i), Duration::from_secs(86_400));
    }
    for i in [5usize, 1, 4, 0, 3, 2] {
        set_mtime(&temp.path().join(RUNS_DIR).join(run_id(i)), same);
    }
    for i in 6..MAX_RUN_BATCHES + 2 {
        make_run(temp.path(), &run_id(i), Duration::from_secs(60));
    }

    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(
        outcome.removed_runs,
        vec![run_id(0), run_id(1)],
        "with equal mtimes the lexicographically smallest ids go first"
    );
}

/// A run whose lease is held is skipped, however old it is.
#[test]
fn un38_a_run_holding_its_lease_is_skipped() {
    let (temp, root) = root();
    for i in 0..MAX_RUN_BATCHES + 2 {
        let age = Duration::from_secs((MAX_RUN_BATCHES + 2 - i) as u64 * 86_400);
        make_run(temp.path(), &run_id(i), age);
    }

    // Hold the oldest run's lease for the duration of the sweep. The age is
    // re-stamped afterwards because creating a file inside a directory updates
    // that directory's mtime — which would make the run the newest, not the
    // oldest, and the test would pass for the wrong reason.
    let lease_path = temp.path().join(RUNS_DIR).join(run_id(0)).join(LEASE_LOCK);
    let lease = fs::File::create(&lease_path).expect("create the lease file");
    set_age(
        &temp.path().join(RUNS_DIR).join(run_id(0)),
        Duration::from_secs((MAX_RUN_BATCHES + 2) as u64 * 86_400),
    );
    // SAFETY: `lease` is an open descriptor owned by this test.
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "the test must actually hold the lease"
    );

    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(
        outcome.skipped_active_runs,
        vec![run_id(0)],
        "a held lease must be reported as a skip, not silently ignored"
    );
    assert!(
        temp.path().join(RUNS_DIR).join(run_id(0)).exists(),
        "the leased run must survive"
    );
    assert_eq!(
        outcome.removed_runs,
        vec![run_id(1)],
        "the next-oldest is collected instead"
    );

    drop(lease);

    // The control: once the lease is gone, the same run is collectable. This is
    // what makes the skip above a property of the lease rather than of the run.
    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(outcome.removed_runs, vec![run_id(0)]);
    assert!(!temp.path().join(RUNS_DIR).join(run_id(0)).exists());
}

/// A stale lease file with nobody holding it protects nothing.
///
/// The lock is released by the kernel when the owning process dies, so a
/// crashed run stops being protected without anyone tidying up. A lease file
/// that merely *exists* must not be mistaken for a held one.
#[test]
fn un38_a_lease_file_nobody_holds_does_not_protect() {
    let (temp, root) = root();
    for i in 0..MAX_RUN_BATCHES + 1 {
        let age = Duration::from_secs((MAX_RUN_BATCHES + 1 - i) as u64 * 86_400);
        make_run(temp.path(), &run_id(i), age);
    }
    fs::write(
        temp.path().join(RUNS_DIR).join(run_id(0)).join(LEASE_LOCK),
        b"",
    )
    .expect("leave a lease file behind");
    // Same reason as above: writing into the directory bumped its mtime.
    set_age(
        &temp.path().join(RUNS_DIR).join(run_id(0)),
        Duration::from_secs((MAX_RUN_BATCHES + 1) as u64 * 86_400),
    );

    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(outcome.removed_runs, vec![run_id(0)]);
}

struct Reserved(&'static str);

impl ReservationView for Reserved {
    fn has_unsettled_reservation(&self, run_id: &str) -> bool {
        run_id == self.0
    }
}

/// An unsettled reservation protects a young run, and stops protecting an old
/// one.
///
/// The age bound is the point: without it, one abandoned reservation would
/// protect a directory forever, and the retention limit would quietly stop
/// applying.
#[test]
fn un38_an_unsettled_reservation_protects_only_within_the_age_window() {
    let (temp, root) = root();
    let protected: &'static str = Box::leak(run_id(0).into_boxed_str());

    // Run 0 is the oldest by mtime but still inside the activity window.
    make_run(temp.path(), protected, ACTIVE_RUN_MAX_AGE / 2);
    for i in 1..MAX_RUN_BATCHES + 2 {
        make_run(temp.path(), &run_id(i), Duration::from_secs(1));
    }
    // Make run 0 the oldest without leaving the window.
    set_age(
        &temp.path().join(RUNS_DIR).join(protected),
        ACTIVE_RUN_MAX_AGE / 2,
    );

    let outcome = sweep_with(&root, &Reserved(protected));
    assert_eq!(
        outcome.skipped_active_runs,
        vec![protected.to_string()],
        "a reservation inside the window must protect"
    );
    assert!(temp.path().join(RUNS_DIR).join(protected).exists());

    // Push it past the window: the same reservation no longer protects it.
    set_age(
        &temp.path().join(RUNS_DIR).join(protected),
        ACTIVE_RUN_MAX_AGE + Duration::from_secs(60),
    );
    let outcome = sweep_with(&root, &Reserved(protected));
    assert!(
        outcome.removed_runs.contains(&protected.to_string()),
        "an abandoned reservation must not protect forever: {outcome:?}"
    );
}

/// Crash debris is removed once it is old enough, and not before.
#[test]
fn un38_temporary_files_are_removed_only_after_the_age_threshold() {
    let (temp, root) = root();
    let runs = temp.path().join(RUNS_DIR);
    fs::create_dir_all(&runs).expect("create runs");

    let young = runs.join(".tmp-young");
    let old = runs.join(".tmp-old");
    fs::write(&young, b"partial").expect("write");
    fs::write(&old, b"partial").expect("write");
    set_age(&young, TEMP_MIN_AGE / 2);
    set_age(&old, TEMP_MIN_AGE + Duration::from_secs(60));

    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(
        outcome.removed_temp_files,
        vec![".tmp-old".to_string()],
        "a young temporary may still belong to a writer mid-sequence"
    );
    assert!(young.exists());
    assert!(!old.exists());
}

/// A name the writer never produces is not swept.
///
/// A sweep that deletes whatever it does not recognise is a delete primitive
/// with a directory listing for input.
#[test]
fn un38_entries_that_are_not_run_directories_are_left_alone() {
    let (temp, root) = root();
    let runs = temp.path().join(RUNS_DIR);
    for i in 0..MAX_RUN_BATCHES + 2 {
        let age = Duration::from_secs((MAX_RUN_BATCHES + 2 - i) as u64 * 86_400);
        make_run(temp.path(), &run_id(i), age);
    }
    let stranger = runs.join("operator-notes");
    fs::create_dir(&stranger).expect("create a directory nobody here made");
    set_age(&stranger, Duration::from_secs(365 * 86_400));

    let outcome = sweep_with(&root, &NoReservations);
    assert!(
        stranger.exists(),
        "an unrecognised directory must survive: {outcome:?}"
    );
    assert!(!outcome.removed_runs.contains(&"operator-notes".to_string()));
}

/// Versions past the limit are removed oldest first.
#[test]
fn un38_only_the_excess_oldest_versions_are_removed() {
    let (temp, root) = root();
    let hexes: Vec<String> = "0123456789abc".chars().map(digest).collect();
    for (i, hex) in hexes.iter().enumerate() {
        let age = Duration::from_secs((hexes.len() - i) as u64 * 86_400);
        make_version(temp.path(), hex, age);
    }

    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(
        outcome.removed_versions.len(),
        hexes.len() - MAX_BASELINE_VERSIONS
    );
    assert_eq!(
        outcome.removed_versions,
        hexes[..hexes.len() - MAX_BASELINE_VERSIONS]
            .iter()
            .map(|hex| format!("{hex}.json"))
            .collect::<Vec<_>>()
    );
}

/// The version the current pointer names is never removed.
#[test]
fn un38_the_current_version_is_never_removed() {
    let (temp, root) = root();
    let hexes: Vec<String> = "0123456789abc".chars().map(digest).collect();
    for (i, hex) in hexes.iter().enumerate() {
        let age = Duration::from_secs((hexes.len() - i) as u64 * 86_400);
        make_version(temp.path(), hex, age);
    }
    // Pin the oldest — the one that would otherwise go first.
    fs::write(
        temp.path().join(BASELINES_DIR).join(POINTER_NAME),
        format!("{{\"digest\":\"sha256:{}\"}}", hexes[0]),
    )
    .expect("write the pointer");

    let outcome = sweep_with(&root, &NoReservations);
    assert!(
        temp.path()
            .join(BASELINES_DIR)
            .join(format!("{}.json", hexes[0]))
            .exists(),
        "the pinned version must survive: {outcome:?}"
    );
    assert!(
        !outcome
            .removed_versions
            .contains(&format!("{}.json", hexes[0]))
    );
    // Protected versions do not count toward the limit, so exactly the excess
    // of the *unprotected* ones is removed.
    assert_eq!(
        outcome.removed_versions.len(),
        hexes.len() - 1 - MAX_BASELINE_VERSIONS
    );
}

/// A manifested version is exempt and does not count toward the limit.
///
/// If protection consumed a retention slot, protecting a version would silently
/// push another one out — the operator's action would delete something.
#[test]
fn un38_manifested_versions_are_exempt_and_do_not_consume_the_limit() {
    let (temp, root) = root();
    let hexes: Vec<String> = "0123456789ab".chars().map(digest).collect();
    for (i, hex) in hexes.iter().enumerate() {
        let age = Duration::from_secs((hexes.len() - i) as u64 * 86_400);
        make_version(temp.path(), hex, age);
    }
    fs::write(
        temp.path().join(BASELINES_DIR).join(PROTECTED_MANIFEST),
        format!(
            "{{\"schema_version\":1,\"protected\":[\"sha256:{}\",\"sha256:{}\"]}}",
            hexes[0], hexes[1]
        ),
    )
    .expect("write the manifest");

    let outcome = sweep_with(&root, &NoReservations);
    for hex in &hexes[..2] {
        assert!(
            temp.path()
                .join(BASELINES_DIR)
                .join(format!("{hex}.json"))
                .exists(),
            "a manifested version must survive"
        );
    }
    assert_eq!(
        outcome.removed_versions.len(),
        hexes.len() - 2 - MAX_BASELINE_VERSIONS,
        "protection must not consume a retention slot"
    );
}

/// A manifest that cannot be trusted stops the sweep.
///
/// Deleting while unable to read what was protected is the one outcome worth
/// avoiding here, so every unparseable form is an error rather than a fallback
/// to "nothing is protected".
#[test]
fn un38_an_unreadable_manifest_stops_the_sweep() {
    let bad_manifests = [
        "not json at all",
        "{\"schema_version\":2,\"protected\":[]}",
        "{\"schema_version\":1,\"protected\":[\"sha256:short\"]}",
        "{\"schema_version\":1,\"protected\":[\"md5:0000\"]}",
        "{\"schema_version\":1,\"protected\":[],\"surprise\":true}",
        "{\"protected\":[]}",
    ];

    for body in bad_manifests {
        let (temp, root) = root();
        make_version(temp.path(), &digest('0'), Duration::from_secs(86_400));
        fs::write(
            temp.path().join(BASELINES_DIR).join(PROTECTED_MANIFEST),
            body,
        )
        .expect("write the manifest");

        let lock = MaintenanceLock::acquire(&root).expect("lock");
        assert!(
            sweep(&root, &lock, &NoReservations, now()).is_err(),
            "`{body}` must stop the sweep rather than be ignored"
        );
    }

    // The control: a well-formed manifest is accepted, so the refusals above
    // are about the content rather than about manifests being rejected.
    let (temp, root) = root();
    make_version(temp.path(), &digest('0'), Duration::from_secs(86_400));
    fs::write(
        temp.path().join(BASELINES_DIR).join(PROTECTED_MANIFEST),
        format!(
            "{{\"schema_version\":1,\"protected\":[\"sha256:{}\"]}}",
            digest('0')
        ),
    )
    .expect("write the manifest");
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    sweep(&root, &lock, &NoReservations, now()).expect("a valid manifest must be accepted");
}

/// An unreadable pointer stops the sweep too.
#[test]
fn un38_an_unreadable_pointer_stops_the_sweep() {
    let (temp, root) = root();
    make_version(temp.path(), &digest('0'), Duration::from_secs(86_400));
    fs::write(
        temp.path().join(BASELINES_DIR).join(POINTER_NAME),
        "{\"digest\":\"not-a-digest\"}",
    )
    .expect("write a pointer nobody can act on");

    let lock = MaintenanceLock::acquire(&root).expect("lock");
    assert!(
        sweep(&root, &lock, &NoReservations, now()).is_err(),
        "a pointer whose target cannot be identified must stop the sweep"
    );
}

/// No manifest means only the current version is protected.
#[test]
fn un38_a_missing_manifest_protects_only_the_current_version() {
    let (temp, root) = root();
    let hexes: Vec<String> = "0123456789ab".chars().map(digest).collect();
    for (i, hex) in hexes.iter().enumerate() {
        let age = Duration::from_secs((hexes.len() - i) as u64 * 86_400);
        make_version(temp.path(), hex, age);
    }
    fs::write(
        temp.path().join(BASELINES_DIR).join(POINTER_NAME),
        format!("{{\"digest\":\"sha256:{}\"}}", hexes[0]),
    )
    .expect("write the pointer");
    assert!(
        !temp
            .path()
            .join(BASELINES_DIR)
            .join(PROTECTED_MANIFEST)
            .exists(),
        "fixture: there is no manifest"
    );

    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(
        outcome.protected_versions,
        vec![format!("{}.json", hexes[0])],
        "an absent manifest is not an error — it simply protects nothing extra"
    );
}

/// The maintenance lock is exclusive, and a non-blocking caller learns that.
#[test]
fn un38_the_maintenance_lock_is_exclusive() {
    let (temp, root) = root();

    let held = MaintenanceLock::acquire(&root).expect("take the lock");
    let second = RestrictedRoot::open(temp.path()).expect("a second handle on the same root");
    assert!(
        MaintenanceLock::try_acquire(&second)
            .expect("probe the lock")
            .is_none(),
        "a second holder must not get the lock while it is held"
    );

    drop(held);
    assert!(
        MaintenanceLock::try_acquire(&second)
            .expect("probe the lock")
            .is_some(),
        "and must get it once it is released"
    );
    assert!(temp.path().join(MAINTENANCE_LOCK).exists());
}

/// A temp file inside a live run is not debris.
///
/// Age alone does not mean "the owner is defunct". Where the temp sits inside a
/// run, the run's lease answers that question exactly — and a run can be busy
/// for longer than the age threshold.
#[test]
fn un38_an_old_temporary_inside_a_live_run_is_kept() {
    let (temp, root) = root();
    let runs = temp.path().join(RUNS_DIR);
    fs::create_dir_all(&runs).expect("create runs");

    let debris = runs.join(".tmp-abandoned");
    fs::write(&debris, b"partial").expect("write");
    set_age(&debris, TEMP_MIN_AGE + Duration::from_secs(60));

    // Hold the lease at the level the temp lives at: the sweep must read that
    // as "somebody is working here".
    let lease = fs::File::create(runs.join(LEASE_LOCK)).expect("create the lease");
    // SAFETY: `lease` is an open descriptor owned by this test.
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let outcome = sweep_with(&root, &NoReservations);
    assert!(
        outcome.removed_temp_files.is_empty(),
        "a live owner's debris is not debris: {outcome:?}"
    );
    assert!(debris.exists());

    // The control: once the owner is gone, the same file is collected.
    drop(lease);
    fs::remove_file(runs.join(LEASE_LOCK)).expect("remove the lease file");
    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(
        outcome.removed_temp_files,
        vec![".tmp-abandoned".to_string()]
    );
}

/// An upper-case digest is refused rather than normalised.
///
/// Version files are named in the canonical lower-case form, so an upper-case
/// entry would build a name that matches no file — and a protection entry that
/// matches nothing is indistinguishable from no protection at all.
#[test]
fn un38_a_non_canonical_digest_is_refused() {
    let (temp, root) = root();
    make_version(temp.path(), &digest('a'), Duration::from_secs(86_400));
    fs::write(
        temp.path().join(BASELINES_DIR).join(PROTECTED_MANIFEST),
        format!(
            "{{\"schema_version\":1,\"protected\":[\"sha256:{}\"]}}",
            digest('A')
        ),
    )
    .expect("write the manifest");

    let lock = MaintenanceLock::acquire(&root).expect("lock");
    assert!(
        sweep(&root, &lock, &NoReservations, now()).is_err(),
        "an upper-case digest must stop the sweep rather than silently protect nothing"
    );
}

/// The pointer keeps working when its schema grows.
///
/// The pointer's full shape belongs to a later card. What the sweep needs is the
/// digest; refusing a pointer that has gained a field would turn this card into
/// a blocker for that one, and refusing one that has *lost* the digest is the
/// property that actually matters.
#[test]
fn un38_a_pointer_with_extra_fields_still_identifies_the_current_version() {
    let (temp, root) = root();
    let hexes: Vec<String> = "0123456789ab".chars().map(digest).collect();
    for (i, hex) in hexes.iter().enumerate() {
        let age = Duration::from_secs((hexes.len() - i) as u64 * 86_400);
        make_version(temp.path(), hex, age);
    }
    fs::write(
        temp.path().join(BASELINES_DIR).join(POINTER_NAME),
        format!(
            "{{\"schema_version\":7,\"digest\":\"sha256:{}\",\"promoted_at\":\"later\"}}",
            hexes[0]
        ),
    )
    .expect("write a richer pointer");

    let outcome = sweep_with(&root, &NoReservations);
    assert_eq!(
        outcome.protected_versions,
        vec![format!("{}.json", hexes[0])],
        "unknown fields must not stop the digest being found"
    );
    assert!(
        temp.path()
            .join(BASELINES_DIR)
            .join(format!("{}.json", hexes[0]))
            .exists()
    );
}

/// A file in `baselines/` that is not a version file is not a version.
///
/// Treating every file there as a retention candidate would make an operator's
/// note in the directory a casualty of the limit — the same reason an
/// unrecognised run directory is left alone.
#[test]
fn un38_unrecognised_baseline_files_are_left_alone() {
    let (temp, root) = root();
    let hexes: Vec<String> = "0123456789ab".chars().map(digest).collect();
    for (i, hex) in hexes.iter().enumerate() {
        let age = Duration::from_secs((hexes.len() - i) as u64 * 86_400);
        make_version(temp.path(), hex, age);
    }

    let stranger = temp.path().join(BASELINES_DIR).join("operator-notes.txt");
    fs::write(&stranger, b"why this baseline was approved").expect("write");
    set_age(&stranger, Duration::from_secs(365 * 86_400));
    let near_miss = temp.path().join(BASELINES_DIR).join("not-a-digest.json");
    fs::write(&near_miss, b"{}").expect("write");
    set_age(&near_miss, Duration::from_secs(365 * 86_400));

    let outcome = sweep_with(&root, &NoReservations);
    assert!(stranger.exists(), "a note must survive: {outcome:?}");
    assert!(
        near_miss.exists(),
        "so must a .json whose stem is not a digest"
    );
    assert_eq!(
        outcome.removed_versions.len(),
        hexes.len() - MAX_BASELINE_VERSIONS,
        "and neither may count toward the retention limit"
    );
}

/// A manifest that cannot be read stops the sweep *before* anything is removed.
///
/// Discovering the manifest is unreadable after the runs are already gone would
/// make "fail-closed" true only of the half that had not happened yet.
#[test]
fn un38_a_bad_manifest_stops_the_sweep_before_any_run_is_removed() {
    let (temp, root) = root();
    for i in 0..MAX_RUN_BATCHES + 3 {
        let age = Duration::from_secs((MAX_RUN_BATCHES + 3 - i) as u64 * 86_400);
        make_run(temp.path(), &run_id(i), age);
    }
    fs::create_dir_all(temp.path().join(BASELINES_DIR)).expect("create baselines");
    fs::write(
        temp.path().join(BASELINES_DIR).join(PROTECTED_MANIFEST),
        "{\"schema_version\":1,\"protected\":[\"sha256:nope\"]}",
    )
    .expect("write a manifest nobody can act on");

    let lock = MaintenanceLock::acquire(&root).expect("lock");
    assert!(sweep(&root, &lock, &NoReservations, now()).is_err());

    assert_eq!(
        fs::read_dir(temp.path().join(RUNS_DIR))
            .expect("list runs")
            .count(),
        MAX_RUN_BATCHES + 3,
        "no run may have been removed before the refusal"
    );
}
