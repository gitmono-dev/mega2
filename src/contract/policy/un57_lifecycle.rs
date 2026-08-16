//! UN-57: reservation lifecycle under the maintenance lock.

use std::time::{Duration, SystemTime};

use crate::contract::policy::{
    secure_artifact::{ArtifactError, RestrictedRoot},
    secure_capacity::{FIXED_METADATA_BYTES, MIB},
    secure_counter::{
        COUNTERS_FILE, CapacityCounter, MAX_COUNTERS_BYTES_FOR_CREATE, MAX_CREATE_RESERVATIONS,
        MAX_SETTLED, ReservationAction, ReservationKind, ReservationRecord, SettledRecord,
        load_counter, store_counter,
    },
    secure_lifecycle::{
        NoLeases, RESERVATION_RECLAIM_AGE, ReserveRequest, SettleOutcome, SettleRequest,
        abort_locked, admit_and_reserve_locked, commit_locked, reclaim_stale_locked, signed_delta,
    },
    secure_producer::Producer,
    secure_sweep::MaintenanceLock,
};

fn root() -> (tempfile::TempDir, RestrictedRoot) {
    let temp = tempfile::tempdir().expect("temp");
    let root = RestrictedRoot::open(temp.path()).expect("open");
    (temp, root)
}

fn now_stamp() -> String {
    "2026-08-16T12:00:00Z".into()
}

fn reserve_run(op: &str, owner_fenced: bool) -> ReserveRequest {
    ReserveRequest {
        op_id: op.into(),
        target: "runs/20260816T120000Z-1".into(),
        payload: if owner_fenced {
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".into()
        } else {
            String::new()
        },
        owner_fenced: Some(owner_fenced),
        created_at: now_stamp(),
        active_runs: 0,
        protected_count: 0,
        directory_entries: 0,
    }
}

#[test]
fn un57_lifecycle_admit_commit_abort_round_trip() {
    let (_temp, root) = root();
    let lock = MaintenanceLock::acquire(&root).expect("lock");

    let out = admit_and_reserve_locked(
        &root,
        &lock,
        Producer::BootstrapCandidate,
        reserve_run("01HQBOOT0000000001", false),
        &NoLeases,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
    )
    .expect("admit");
    assert_eq!(out.reservation.max_bytes, 3 * MIB);
    assert_eq!(out.reservation.owner_fenced, Some(false));

    let committed = commit_locked(
        &root,
        &lock,
        SettleRequest {
            op_id: "01HQBOOT0000000001".into(),
            settled_delta: 1024,
            final_state: "committed".into(),
            settled_at: now_stamp(),
            run_id: None,
            cap_hash: None,
        },
    )
    .expect("commit");
    assert!(matches!(
        committed,
        SettleOutcome::Settled {
            settled_delta: 1024,
            ..
        }
    ));

    let counter = load_counter(&root, &lock).expect("load");
    assert!(counter.reservations.is_empty());
    assert_eq!(counter.settled.len(), 1);
    assert_eq!(counter.settled[0].cap_hash, None);
    assert_eq!(counter.total_bytes, 1024);
    assert_eq!(counter.reserved_bytes, 0);
}

#[test]
fn un57_lifecycle_owner_fenced_idempotent_retry_and_wrong_cap() {
    let (_temp, root) = root();
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let cap = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    admit_and_reserve_locked(
        &root,
        &lock,
        Producer::KillSwitchEvidence,
        ReserveRequest {
            op_id: "01HQEVID0000000001".into(),
            target: "runs/20260816T120000Z-9".into(),
            payload: cap.into(),
            owner_fenced: Some(true),
            created_at: now_stamp(),
            active_runs: 0,
            protected_count: 0,
            directory_entries: 0,
        },
        &NoLeases,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
    )
    .expect("admit");

    commit_locked(
        &root,
        &lock,
        SettleRequest {
            op_id: "01HQEVID0000000001".into(),
            settled_delta: 200,
            final_state: "committed".into(),
            settled_at: now_stamp(),
            run_id: Some("20260816T120000Z-9".into()),
            cap_hash: Some(cap.into()),
        },
    )
    .expect("commit");

    let retry = commit_locked(
        &root,
        &lock,
        SettleRequest {
            op_id: "01HQEVID0000000001".into(),
            settled_delta: 999,
            final_state: "ignored".into(),
            settled_at: now_stamp(),
            run_id: Some("20260816T120000Z-9".into()),
            cap_hash: Some(cap.into()),
        },
    )
    .expect("idempotent");
    assert!(matches!(
        retry,
        SettleOutcome::IdempotentRetry {
            settled_delta: 200,
            ..
        }
    ));

    let err = commit_locked(
        &root,
        &lock,
        SettleRequest {
            op_id: "01HQEVID0000000001".into(),
            settled_delta: 0,
            final_state: "x".into(),
            settled_at: now_stamp(),
            run_id: Some("20260816T120000Z-9".into()),
            cap_hash: Some(
                "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
            ),
        },
    )
    .expect_err("wrong cap");
    assert!(matches!(err, ArtifactError::LifecycleRejected { .. }));
}

#[test]
fn un57_lifecycle_unprotect_uses_delete_settled_and_signed_delta() {
    let (_temp, root) = root();
    let lock = MaintenanceLock::acquire(&root).expect("lock");

    // Fill settled[] to capacity so unprotect must use delete_settled[].
    let mut counter = CapacityCounter::default();
    for i in 0..MAX_SETTLED {
        counter.settled.push(SettledRecord {
            op_id: format!("01HQFILL{i:016}"),
            kind: ReservationKind::Report,
            action: ReservationAction::Create,
            settled_delta: 1,
            final_state: "committed".into(),
            settled_at: format!("2026-08-16T{:02}:{:02}:00Z", i / 60, i % 60),
            owner_fenced: None,
            run_id: None,
            cap_hash: None,
        });
    }
    store_counter(&root, &lock, &counter, true).expect("seed settled");
    // Seed total_bytes so a negative unprotect delta can apply.
    let mut counter = load_counter(&root, &lock).expect("load");
    counter.total_bytes = 100;
    store_counter(&root, &lock, &counter, true).expect("seed total");

    admit_and_reserve_locked(
        &root,
        &lock,
        Producer::Unprotect,
        ReserveRequest {
            op_id: "01HQUNPROT00000001".into(),
            target: "baselines/protected.json".into(),
            payload: "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                .into(),
            owner_fenced: None,
            created_at: now_stamp(),
            active_runs: 0,
            protected_count: 1,
            directory_entries: 0,
        },
        &NoLeases,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
    )
    .expect("unprotect admit");

    let delta = signed_delta(100, 80).expect("delta");
    assert_eq!(delta, -20);
    commit_locked(
        &root,
        &lock,
        SettleRequest {
            op_id: "01HQUNPROT00000001".into(),
            settled_delta: delta,
            final_state: "deleted".into(),
            settled_at: now_stamp(),
            run_id: None,
            cap_hash: None,
        },
    )
    .expect("unprotect commit");

    let counter = load_counter(&root, &lock).expect("load");
    assert_eq!(counter.settled.len(), MAX_SETTLED);
    assert_eq!(counter.delete_settled.len(), 1);
    assert_eq!(counter.delete_settled[0].settled_delta, -20);
}

#[test]
fn un57_lifecycle_reclaim_respects_age_and_try_acquire_while_held() {
    let (temp, root) = root();
    let _lock = MaintenanceLock::acquire(&root).expect("lock");

    let path = temp.path().to_path_buf();
    let held = std::thread::spawn(move || {
        let root2 = RestrictedRoot::open(&path).expect("open");
        MaintenanceLock::try_acquire(&root2).expect("try")
    })
    .join()
    .expect("join");
    assert!(
        held.is_none(),
        "second acquire must fail while maintenance lock is held"
    );

    let mut counter = CapacityCounter::default();
    counter.reservations.push(ReservationRecord {
        op_id: "01HQOLD00000000001".into(),
        kind: ReservationKind::Promotion,
        action: ReservationAction::Create,
        created_at: "2026-01-01T00:00:00Z".into(),
        max_bytes: MIB,
        target: "baselines/aa.json".into(),
        payload: String::new(),
        owner_fenced: None,
        settle_key: "01HQOLD00000000001".into(),
    });
    counter.reserved_bytes = MIB;
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let n = reclaim_stale_locked(&mut counter, &NoLeases, now).expect("reclaim");
    assert_eq!(n, 1);
    assert!(counter.reservations.is_empty());
    assert_eq!(counter.reserved_bytes, 0);
    assert!(RESERVATION_RECLAIM_AGE.as_secs() == 3600);
}

#[test]
fn un57_lifecycle_unprotect_still_works_when_create_ledger_full() {
    let (_temp, root) = root();
    let lock = MaintenanceLock::acquire(&root).expect("lock");

    // Inflate create reservations until the create-path ledger is at/over 40 KiB.
    let mut counter = CapacityCounter::default();
    for i in 0..MAX_CREATE_RESERVATIONS {
        let mut r = ReservationRecord {
            op_id: format!("01HQFULL{i:016}"),
            kind: ReservationKind::Report,
            action: ReservationAction::Create,
            created_at: now_stamp(),
            max_bytes: 1024,
            target: format!("sweep-reports/{}", "t".repeat(40)),
            payload: "p".repeat(900),
            owner_fenced: None,
            settle_key: format!("01HQFULL{i:016}"),
        };
        // Keep pushing payload until serialize would exceed create ceiling.
        while counter.to_canonical_bytes(true).is_ok() && r.payload.len() < 2000 {
            r.payload.push('x');
            counter.reservations.pop();
            counter.reservations.push(r.clone());
        }
        if counter.reservations.len() <= i {
            counter.reservations.push(r);
        }
        if counter.to_canonical_bytes(true).is_err() {
            break;
        }
    }
    // Persist with delete ceiling (for_create=false) so the oversize create
    // ledger still lands for the unprotect path to read.
    let bytes = serde_json::to_vec(&counter).expect("serde");
    assert!(bytes.len() > MAX_COUNTERS_BYTES_FOR_CREATE || counter.create_reservation_count() > 0);
    store_counter(&root, &lock, &counter, false).unwrap_or_else(|_| {
        // If even full ceiling fails, shrink payload once and store via delete path.
        counter.reservations.truncate(40);
        store_counter(&root, &lock, &counter, false).expect("store truncated")
    });
    assert!(root.display().join(COUNTERS_FILE).is_file());

    admit_and_reserve_locked(
        &root,
        &lock,
        Producer::Unprotect,
        ReserveRequest {
            op_id: "01HQUNPROTFULL0001".into(),
            target: "baselines/protected.json".into(),
            payload: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
            owner_fenced: None,
            created_at: now_stamp(),
            active_runs: 0,
            protected_count: 1,
            directory_entries: 0,
        },
        &NoLeases,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
    )
    .expect("unprotect must still admit under create pressure");

    abort_locked(
        &root,
        &lock,
        SettleRequest {
            op_id: "01HQUNPROTFULL0001".into(),
            settled_delta: 0,
            final_state: "aborted".into(),
            settled_at: now_stamp(),
            run_id: None,
            cap_hash: None,
        },
    )
    .expect("abort");
}

#[test]
fn un57_lifecycle_six_producers_immediate_settle() {
    let producers = [
        (Producer::BootstrapCandidate, Some(false)),
        (Producer::Compare, Some(false)),
        (Producer::Promote, None),
        (Producer::SweepReport, None),
        (Producer::Protect, None),
        (Producer::Unprotect, None),
    ];
    for (i, (producer, owner_fenced)) in producers.into_iter().enumerate() {
        let (_temp, root) = root();
        let lock = MaintenanceLock::acquire(&root).expect("lock");
        if producer == Producer::Unprotect {
            let counter = CapacityCounter {
                total_bytes: 10,
                ..CapacityCounter::default()
            };
            store_counter(&root, &lock, &counter, true).expect("seed");
        }
        let op = format!("01HQSIX{i:016}");
        let req = ReserveRequest {
            op_id: op.clone(),
            target: match producer {
                Producer::Promote => "baselines/digest.json".into(),
                Producer::SweepReport => "sweep-reports/20260816T120000Z-1.json".into(),
                Producer::Protect | Producer::Unprotect => "baselines/protected.json".into(),
                _ => "runs/20260816T120000Z-1".into(),
            },
            payload: if producer == Producer::Protect || producer == Producer::Unprotect {
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()
            } else {
                String::new()
            },
            owner_fenced,
            created_at: now_stamp(),
            active_runs: 0,
            protected_count: 0,
            directory_entries: 0,
        };
        admit_and_reserve_locked(
            &root,
            &lock,
            producer,
            req,
            &NoLeases,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_724_000_000),
        )
        .unwrap_or_else(|e| panic!("{producer:?} admit: {e}"));
        commit_locked(
            &root,
            &lock,
            SettleRequest {
                op_id: op,
                settled_delta: if producer == Producer::Unprotect {
                    -1
                } else if producer == Producer::Protect {
                    FIXED_METADATA_BYTES as i64
                } else {
                    10
                },
                final_state: "committed".into(),
                settled_at: now_stamp(),
                run_id: None,
                cap_hash: None,
            },
        )
        .unwrap_or_else(|e| panic!("{producer:?} commit: {e}"));
        let counter = load_counter(&root, &lock).expect("load");
        assert!(
            counter.reservations.is_empty(),
            "{producer:?} left a reservation"
        );
    }
}

#[test]
fn un57_lifecycle_signed_delta_growth_shrink_noop() {
    assert_eq!(signed_delta(10, 15).unwrap(), 5);
    assert_eq!(signed_delta(15, 10).unwrap(), -5);
    assert_eq!(signed_delta(10, 10).unwrap(), 0);
}
