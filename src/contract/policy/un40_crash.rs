//! UN-40: promotion crash-window end states and retry convergence.

use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use crate::contract::policy::{
    baseline_pointer::{
        baselines_dir_fragment, read_current_pointer, write_current_pointer,
        write_version_for_digest,
    },
    baseline_promotion::{
        ALREADY_CURRENT_MARKER, CrashWindow, PromoteFence, PromoteOutcome, PromoteRequest,
        content_digest, promote,
    },
    secure_artifact::{BASELINES_DIR, RestrictedRoot},
    secure_sweep::{MaintenanceLock, NoReservations, TEMP_MIN_AGE, sweep},
};

fn root() -> (tempfile::TempDir, RestrictedRoot) {
    let temp = tempfile::tempdir().expect("temp");
    let root = RestrictedRoot::open(temp.path()).expect("open");
    (temp, root)
}

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000)
}

fn req<'a>(candidate: &'a [u8], digest: &'a str, fence: PromoteFence) -> PromoteRequest<'a> {
    PromoteRequest {
        candidate,
        expect_digest: digest,
        fence,
        now: now(),
        directory_entries: 0,
    }
}

fn set_mtime(path: &Path, when: SystemTime) {
    let file = fs::File::open(path).expect("open for utime");
    file.set_times(fs::FileTimes::new().set_modified(when))
        .expect("set mtime");
}

#[test]
fn un40_crash_windows_are_named() {
    assert_eq!(CrashWindow::W1VersionTemp as u8, 0);
    assert_eq!(CrashWindow::W6FullyCommitted as u8, 5);
}

#[test]
fn un40_w1_version_temp_left_retry_promotes() {
    let (temp, root) = root();
    let baselines = temp.path().join(BASELINES_DIR);
    fs::create_dir_all(&baselines).expect("mkdir");
    let debris = baselines.join(".tmp-w1-version");
    fs::write(&debris, br#"partial"#).expect("tmp");

    let candidate = br#"{"w":1}"#;
    let digest = content_digest(candidate);
    let out = promote(
        &root,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("retry after W1");
    assert!(matches!(out, PromoteOutcome::Promoted { .. }));
    assert!(debris.exists(), "temps are UN-38's job, not promote's");
    assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
}

#[test]
fn un40_w2_version_present_retry_is_idempotent() {
    let (_temp, root) = root();
    let candidate = br#"{"w":2}"#;
    let digest = content_digest(candidate);
    write_version_for_digest(&root, &digest, candidate).expect("pre-plant version");

    let out = promote(
        &root,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("W2 retry");
    assert!(matches!(out, PromoteOutcome::Promoted { .. }));
    assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
}

#[test]
fn un40_w3_version_without_pointer_continues() {
    let (_temp, root) = root();
    let old = br#"{"old":true}"#;
    let old_d = content_digest(old);
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    write_version_for_digest(&root, &old_d, old).expect("old version");
    write_current_pointer(&root, &lock, &old_d).expect("old pointer");
    drop(lock);

    let candidate = br#"{"w":3}"#;
    let digest = content_digest(candidate);
    write_version_for_digest(&root, &digest, candidate).expect("new version only");

    let out = promote(
        &root,
        req(
            candidate,
            &digest,
            PromoteFence::ExpectCurrentDigest(old_d.clone()),
        ),
    )
    .expect("W3 continue");
    assert!(matches!(out, PromoteOutcome::Promoted { .. }));
    assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
}

#[test]
fn un40_w4_pointer_temp_left_retry_promotes() {
    let (temp, root) = root();
    let baselines = temp.path().join(BASELINES_DIR);
    fs::create_dir_all(&baselines).expect("mkdir");
    let debris = baselines.join(".tmp-w4-pointer");
    fs::write(&debris, br#"{"schema_version":1,"digest":"sha256:dead"}"#).expect("tmp");

    let candidate = br#"{"w":4}"#;
    let digest = content_digest(candidate);
    let out = promote(
        &root,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("W4");
    assert!(matches!(out, PromoteOutcome::Promoted { .. }));
    assert!(debris.exists());
}

#[test]
fn un40_w5_pointer_may_already_be_new_or_still_old() {
    let (_temp, root) = root();
    let candidate = br#"{"w":5}"#;
    let digest = content_digest(candidate);

    // New pointer durable (rename done): retry → already-current.
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    write_version_for_digest(&root, &digest, candidate).expect("version");
    write_current_pointer(&root, &lock, &digest).expect("pointer");
    drop(lock);

    let out = promote(
        &root,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("W5 already");
    assert_eq!(
        out,
        PromoteOutcome::AlreadyCurrent {
            digest: digest.clone()
        }
    );
    assert_eq!(out.already_current_marker(), Some(ALREADY_CURRENT_MARKER));
}

#[test]
fn un40_w6_fully_committed_is_already_current() {
    let (_temp, root) = root();
    let candidate = br#"{"w":6}"#;
    let digest = content_digest(candidate);
    promote(
        &root,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("first");

    let out = promote(
        &root,
        req(
            candidate,
            &digest,
            // Wrong fence on purpose — already-current must ignore it.
            PromoteFence::ExpectCurrentDigest(
                "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".into(),
            ),
        ),
    )
    .expect("W6");
    assert_eq!(out, PromoteOutcome::AlreadyCurrent { digest });
    assert_eq!(out.already_current_marker(), Some(ALREADY_CURRENT_MARKER));
}

#[test]
fn un40_already_current_settles_orphan_promotion_reservation() {
    use crate::contract::policy::{
        secure_counter::load_counter,
        secure_hardcap::{new_op_id, rfc3339_utc},
        secure_lifecycle::{NoLeases, ReserveRequest, admit_and_reserve_locked},
        secure_producer::Producer,
    };

    let (_temp, root) = root();
    let candidate = br#"{"orphan":true}"#;
    let digest = content_digest(candidate);
    let lock = MaintenanceLock::acquire(&root).expect("lock");

    // Durable promote without settle (crash between pointer and commit).
    write_version_for_digest(&root, &digest, candidate).expect("version");
    write_current_pointer(&root, &lock, &digest).expect("pointer");
    let version_name =
        crate::contract::policy::baseline_pointer::version_file_name(&digest).unwrap();
    admit_and_reserve_locked(
        &root,
        &lock,
        Producer::Promote,
        ReserveRequest {
            op_id: new_op_id(),
            target: format!("baselines/{version_name} + pointer"),
            payload: digest.clone(),
            owner_fenced: None,
            created_at: rfc3339_utc(now()),
            active_runs: 0,
            protected_count: 0,
            directory_entries: 0,
        },
        &NoLeases,
        now(),
    )
    .expect("orphan reserve");
    assert_eq!(load_counter(&root, &lock).unwrap().reservations.len(), 1);
    drop(lock);

    let out = promote(
        &root,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("settle orphan");
    assert!(matches!(out, PromoteOutcome::AlreadyCurrent { .. }));

    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let counter = load_counter(&root, &lock).expect("load");
    assert!(counter.reservations.is_empty());
    assert_eq!(counter.settled.len(), 1);
}

#[test]
fn un40_tmp_cleanup_delegates_to_sweep_age_threshold() {
    let (temp, root) = root();
    let baselines = temp.path().join(baselines_dir_fragment());
    fs::create_dir_all(&baselines).expect("mkdir");
    let young = baselines.join(".tmp-young");
    let old = baselines.join(".tmp-old");
    fs::write(&young, b"y").expect("young");
    fs::write(&old, b"o").expect("old");
    set_mtime(&young, SystemTime::now() - TEMP_MIN_AGE / 2);
    set_mtime(
        &old,
        SystemTime::now() - TEMP_MIN_AGE - Duration::from_secs(60),
    );

    // Seed a current so sweep has a protected set.
    let candidate = br#"{"keep":true}"#;
    let digest = content_digest(candidate);
    promote(
        &root,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("seed");

    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let outcome = sweep(&root, &lock, &NoReservations, SystemTime::now()).expect("sweep");
    assert!(outcome.removed_temp_files.contains(&".tmp-old".into()));
    assert!(!outcome.removed_temp_files.contains(&".tmp-young".into()));
    assert!(young.exists());
    assert!(!old.exists());
    assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
}
