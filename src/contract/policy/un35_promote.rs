//! UN-35: baseline promotion CAS under the maintenance lock.

use std::{
    sync::{Arc, Barrier},
    thread,
    time::{Duration, SystemTime},
};

use crate::contract::policy::{
    baseline_pointer::read_current_pointer,
    baseline_promotion::{
        PROMOTION_FENCING_EXIT_CODE, PromoteFence, PromoteOutcome, PromoteRequest, content_digest,
        promote, promote_locked, promote_locked_with_between_hook, resolve_promote_fence,
    },
    secure_artifact::{ArtifactError, RestrictedRoot},
    secure_counter::load_counter,
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

fn req<'a>(candidate: &'a [u8], digest: &'a str, fence: PromoteFence) -> PromoteRequest<'a> {
    PromoteRequest {
        candidate,
        expect_digest: digest,
        fence,
        now: now(),
        directory_entries: 0,
    }
}

#[test]
fn un35_promote_expect_flags_are_exclusive_and_required() {
    assert!(matches!(
        resolve_promote_fence(false, None),
        Err(ArtifactError::PromotionRejected { .. })
    ));
    assert!(matches!(
        resolve_promote_fence(
            true,
            Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        ),
        Err(ArtifactError::PromotionRejected { .. })
    ));
    assert_eq!(
        resolve_promote_fence(true, None).unwrap(),
        PromoteFence::ExpectNoCurrent
    );
}

#[test]
fn un35_promote_integrity_mismatch_writes_nothing() {
    let (_temp, root) = root();
    let candidate = br#"{"ok":true}"#;
    let err = promote(
        &root,
        req(
            candidate,
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            PromoteFence::ExpectNoCurrent,
        ),
    )
    .expect_err("integrity");
    assert!(matches!(err, ArtifactError::PromotionRejected { .. }));
    assert!(read_current_pointer(&root).unwrap().is_none());
}

#[test]
fn un35_promote_first_then_already_current_skips_fencing() {
    let (_temp, root) = root();
    let candidate = br#"{"schema":1,"roles":[]}"#;
    let digest = content_digest(candidate);

    let out = promote(
        &root,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("first");
    assert_eq!(
        out,
        PromoteOutcome::Promoted {
            digest: digest.clone()
        }
    );

    let out = promote(
        &root,
        req(
            candidate,
            &digest,
            PromoteFence::ExpectCurrentDigest(
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into(),
            ),
        ),
    )
    .expect("already-current");
    assert_eq!(out, PromoteOutcome::AlreadyCurrent { digest });
}

#[test]
fn un35_promote_fencing_mismatch_is_exit_code_three_and_zero_writes() {
    let (_temp, root) = root();
    let first = br#"{"v":1}"#;
    let d1 = content_digest(first);
    promote(&root, req(first, &d1, PromoteFence::ExpectNoCurrent)).expect("seed");

    let second = br#"{"v":2}"#;
    let d2 = content_digest(second);
    let err = promote(&root, req(second, &d2, PromoteFence::ExpectNoCurrent)).expect_err("fencing");
    match err {
        ArtifactError::PromotionFencing { code, .. } => {
            assert_eq!(code, PROMOTION_FENCING_EXIT_CODE);
        }
        other => panic!("expected fencing: {other}"),
    }
    assert_eq!(
        read_current_pointer(&root).unwrap().unwrap().digest,
        d1,
        "pointer must remain the winner"
    );
}

#[test]
fn un35_promote_concurrent_fencing_only_one_wins() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(2));

    let make = |body: &'static [u8]| {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            let root = RestrictedRoot::open(&path).expect("open");
            let digest = content_digest(body);
            barrier.wait();
            promote(
                &root,
                PromoteRequest {
                    candidate: body,
                    expect_digest: &digest,
                    fence: PromoteFence::ExpectNoCurrent,
                    now: now(),
                    directory_entries: 0,
                },
            )
        })
    };

    let a = make(br#"{"who":"a"}"#);
    let b = make(br#"{"who":"b"}"#);
    let ra = a.join().expect("join a");
    let rb = b.join().expect("join b");

    let wins = [&ra, &rb]
        .iter()
        .filter(|r| matches!(r, Ok(PromoteOutcome::Promoted { .. })))
        .count();
    let fences = [&ra, &rb]
        .iter()
        .filter(|r| matches!(r, Err(ArtifactError::PromotionFencing { code, .. }) if *code == 3))
        .count();
    assert_eq!(wins, 1, "exactly one promote must succeed: {ra:?} {rb:?}");
    assert_eq!(fences, 1, "loser must be fencing exit 3: {ra:?} {rb:?}");

    let root = RestrictedRoot::open(&path).expect("open");
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let counter = load_counter(&root, &lock).expect("counter");
    assert!(counter.reservations.is_empty());
    assert_eq!(counter.settled.len(), 1);
}

#[test]
fn un35_promote_holds_lock_so_concurrent_sweep_cannot_dangle_current() {
    let (_temp, root) = root();
    let candidate = br#"{"keep":true}"#;
    let digest = content_digest(candidate);
    let lock = MaintenanceLock::acquire(&root).expect("lock");

    let root_path = root.display().to_path_buf();
    let outcome = promote_locked_with_between_hook(
        &root,
        &lock,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
        &|| {
            let root2 = RestrictedRoot::open(&root_path).expect("open");
            assert!(
                MaintenanceLock::try_acquire(&root2).expect("try").is_none(),
                "sweep/admission must not enter while promote holds the lock"
            );
        },
    )
    .expect("promote");
    assert!(matches!(outcome, PromoteOutcome::Promoted { .. }));

    drop(lock);
    let lock = MaintenanceLock::acquire(&root).expect("lock again");
    let swept = sweep(&root, &lock, &NoReservations, now()).expect("sweep");
    assert!(swept.report_path.is_some());
    assert_eq!(read_current_pointer(&root).unwrap().unwrap().digest, digest);
}

#[test]
fn un35_promote_settles_reservation_immediately() {
    let (_temp, root) = root();
    let candidate = br#"{"n":1}"#;
    let digest = content_digest(candidate);
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    promote_locked(
        &root,
        &lock,
        req(candidate, &digest, PromoteFence::ExpectNoCurrent),
    )
    .expect("promote");
    let counter = load_counter(&root, &lock).expect("load");
    assert!(counter.reservations.is_empty());
    assert_eq!(counter.settled.len(), 1);
    assert_eq!(
        counter.settled[0].kind,
        crate::contract::policy::secure_counter::ReservationKind::Promotion
    );
}
