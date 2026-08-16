//! UN-51: capacity counter ledger bounds, tested where over-size would silently
//! corrupt the restricted root's accounting.

use crate::contract::policy::{
    secure_artifact::{ArtifactError, RestrictedRoot},
    secure_counter::{
        COUNTERS_FILE, CapacityCounter, DeleteSettledRecord, MAX_COUNTERS_BYTES,
        MAX_COUNTERS_BYTES_FOR_CREATE, MAX_CREATE_RESERVATIONS, MAX_DELETE_SETTLED, MAX_SETTLED,
        ReservationAction, ReservationKind, ReservationRecord, SettledRecord, load_counter,
        reconcile_counts_from_disk, store_counter,
    },
    secure_sweep::MaintenanceLock,
};

fn root() -> (tempfile::TempDir, RestrictedRoot) {
    let temp = tempfile::tempdir().expect("temp");
    let root = RestrictedRoot::open(temp.path()).expect("open");
    (temp, root)
}

fn create_reservation(i: usize) -> ReservationRecord {
    ReservationRecord {
        op_id: format!("01HQTEST{i:016}"),
        kind: ReservationKind::Report,
        action: ReservationAction::Create,
        created_at: "2026-08-16T00:00:00Z".into(),
        max_bytes: 1024,
        target: format!("sweep-reports/r{i}.json"),
        payload: String::new(),
        owner_fenced: None,
        settle_key: format!("01HQTEST{i:016}"),
    }
}

fn settled(i: usize) -> SettledRecord {
    SettledRecord {
        op_id: format!("01HQSET{i:016}"),
        kind: ReservationKind::Report,
        action: ReservationAction::Create,
        settled_delta: 1024,
        final_state: "committed".into(),
        settled_at: format!("2026-08-16T00:{i:02}:00Z"),
        owner_fenced: None,
        run_id: None,
        cap_hash: None,
    }
}

fn delete_settled(i: usize) -> DeleteSettledRecord {
    DeleteSettledRecord {
        op_id: format!("01HQDEL{i:016}"),
        kind: ReservationKind::Protect,
        action: ReservationAction::Delete,
        settled_delta: -1,
        final_state: "deleted".into(),
        settled_at: format!("2026-08-16T01:{i:02}:00Z"),
    }
}

#[test]
fn un51_ledger_rejects_unknown_fields_and_schema() {
    let err = CapacityCounter::from_canonical_bytes(
        br#"{"schema_version":1,"runs":0,"versions":0,"reports":0,"total_bytes":0,"reserved_bytes":0,"reservations":[],"settled":[],"delete_settled":[],"extra":true}"#,
    )
    .expect_err("extra field");
    assert!(matches!(err, ArtifactError::CounterInvalid { .. }));

    let err = CapacityCounter::from_canonical_bytes(
        br#"{"schema_version":2,"runs":0,"versions":0,"reports":0,"total_bytes":0,"reserved_bytes":0,"reservations":[],"settled":[],"delete_settled":[]}"#,
    )
    .expect_err("bad schema");
    assert!(matches!(err, ArtifactError::CounterInvalid { .. }));
}

#[test]
fn un51_ledger_create_reservations_bound_at_64() {
    let mut counter = CapacityCounter::default();
    for i in 0..MAX_CREATE_RESERVATIONS {
        counter.reservations.push(create_reservation(i));
    }
    counter
        .to_canonical_bytes(true)
        .expect("64 create reservations ok");
    counter.reservations.push(create_reservation(64));
    let err = counter.to_canonical_bytes(true).expect_err("65th");
    assert!(matches!(err, ArtifactError::CounterInvalid { .. }));
}

#[test]
fn un51_ledger_delete_action_does_not_consume_create_slot() {
    let mut counter = CapacityCounter::default();
    for i in 0..MAX_CREATE_RESERVATIONS {
        counter.reservations.push(create_reservation(i));
    }
    counter.reservations.push(ReservationRecord {
        op_id: "01HQDELDELETE00001".into(),
        kind: ReservationKind::Protect,
        action: ReservationAction::Delete,
        created_at: "2026-08-16T00:00:00Z".into(),
        max_bytes: 0,
        target: "baselines/aa.json".into(),
        payload: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
        owner_fenced: None,
        settle_key: "01HQDELDELETE00001".into(),
    });
    assert_eq!(counter.create_reservation_count(), MAX_CREATE_RESERVATIONS);
    counter
        .to_canonical_bytes(true)
        .expect("delete action is outside the create slot count");
}

#[test]
fn un51_ledger_settled_bound_at_64() {
    let mut counter = CapacityCounter::default();
    for i in 0..MAX_SETTLED {
        counter.settled.push(settled(i));
    }
    counter.to_canonical_bytes(true).expect("64 settled ok");
    counter.settled.push(settled(64));
    assert!(matches!(
        counter.to_canonical_bytes(true),
        Err(ArtifactError::CounterInvalid { .. })
    ));
}

#[test]
fn un51_ledger_delete_settled_evicts_oldest_at_17() {
    let mut counter = CapacityCounter::default();
    for i in 0..MAX_DELETE_SETTLED {
        counter.push_delete_settled(delete_settled(i));
    }
    assert_eq!(counter.delete_settled.len(), MAX_DELETE_SETTLED);
    counter.push_delete_settled(delete_settled(16));
    assert_eq!(counter.delete_settled.len(), MAX_DELETE_SETTLED);
    assert!(
        !counter
            .delete_settled
            .iter()
            .any(|d| d.op_id.ends_with("0000000000000000")),
        "oldest delete_settled must be evicted"
    );
    assert!(
        counter
            .delete_settled
            .iter()
            .any(|d| d.op_id.contains("0000000000000016") || d.settled_at.contains("01:16:")),
        "newest delete_settled must remain"
    );
}

#[test]
fn un51_ledger_owner_fenced_round_trip() {
    let mut counter = CapacityCounter::default();
    counter.reservations.push(ReservationRecord {
        op_id: "01HQRUNEVIDENCE0001".into(),
        kind: ReservationKind::Run,
        action: ReservationAction::Create,
        created_at: "2026-08-16T00:00:00Z".into(),
        max_bytes: 4096,
        target: "runs/20260816T000000Z-1".into(),
        payload: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        owner_fenced: Some(true),
        settle_key: "01HQRUNEVIDENCE0001".into(),
    });
    counter.settled.push(SettledRecord {
        op_id: "01HQRUNEVIDENCE0001".into(),
        kind: ReservationKind::Run,
        action: ReservationAction::Create,
        settled_delta: 4096,
        final_state: "committed".into(),
        settled_at: "2026-08-16T00:01:00Z".into(),
        owner_fenced: Some(true),
        run_id: Some("20260816T000000Z-1".into()),
        cap_hash: Some(
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        ),
    });
    counter.settled.push(SettledRecord {
        op_id: "01HQRUNAUDIT0000001".into(),
        kind: ReservationKind::Run,
        action: ReservationAction::Create,
        settled_delta: 0,
        final_state: "committed".into(),
        settled_at: "2026-08-16T00:02:00Z".into(),
        owner_fenced: Some(false),
        run_id: None,
        cap_hash: None,
    });

    let bytes = counter.to_canonical_bytes(true).expect("serialize");
    assert!(
        !String::from_utf8_lossy(&bytes).contains("run_cap="),
        "raw run_cap must never appear"
    );
    let round = CapacityCounter::from_canonical_bytes(&bytes).expect("parse");
    assert_eq!(round.reservations[0].owner_fenced, Some(true));
    assert!(round.settled[0].cap_hash.is_some());
    assert_eq!(round.settled[1].cap_hash, None);
}

#[test]
fn un51_ledger_rejects_malformed_owner_and_delete_shapes() {
    let mut counter = CapacityCounter::default();
    counter.settled.push(SettledRecord {
        op_id: "01HQPROTECT0000001".into(),
        kind: ReservationKind::Protect,
        action: ReservationAction::Create,
        settled_delta: 1,
        final_state: "committed".into(),
        settled_at: "2026-08-16T00:00:00Z".into(),
        owner_fenced: None,
        run_id: Some("20260816T000000Z-1".into()),
        cap_hash: None,
    });
    assert!(matches!(
        counter.to_canonical_bytes(true),
        Err(ArtifactError::CounterInvalid { .. })
    ));

    let mut counter = CapacityCounter::default();
    counter.settled.push(SettledRecord {
        op_id: "01HQRUNAUDIT0000001".into(),
        kind: ReservationKind::Run,
        action: ReservationAction::Create,
        settled_delta: 0,
        final_state: "committed".into(),
        settled_at: "2026-08-16T00:00:00Z".into(),
        owner_fenced: Some(false),
        run_id: Some("20260816T000000Z-1".into()),
        cap_hash: None,
    });
    assert!(matches!(
        counter.to_canonical_bytes(true),
        Err(ArtifactError::CounterInvalid { .. })
    ));

    let mut counter = CapacityCounter::default();
    counter.delete_settled.push(DeleteSettledRecord {
        op_id: "01HQBADDELETE00001".into(),
        kind: ReservationKind::Report,
        action: ReservationAction::Delete,
        settled_delta: -1,
        final_state: "deleted".into(),
        settled_at: "2026-08-16T00:00:00Z".into(),
    });
    assert!(matches!(
        counter.to_canonical_bytes(false),
        Err(ArtifactError::CounterInvalid { .. })
    ));
}

#[test]
fn un51_ledger_create_ceiling_leaves_delete_headroom() {
    let mut counter = CapacityCounter::default();
    // Inflate create-path payload until the create ceiling fails but the full
    // ceiling would still have room for a small delete_settled write.
    for i in 0..40 {
        let mut r = create_reservation(i);
        r.payload = "p".repeat(900);
        r.target = format!("sweep-reports/{}", "t".repeat(40));
        counter.reservations.push(r);
    }
    let create_err = counter
        .to_canonical_bytes(true)
        .expect_err("create path should hit 40 KiB ceiling first");
    match create_err {
        ArtifactError::CounterTooLarge { limit, .. } => {
            assert_eq!(limit, MAX_COUNTERS_BYTES_FOR_CREATE);
        }
        other => panic!("expected CounterTooLarge, got {other}"),
    }
    // Delete path still fits under the full 48 KiB ceiling.
    counter.push_delete_settled(delete_settled(0));
    let bytes = counter
        .to_canonical_bytes(false)
        .expect("delete headroom must still accept the ledger");
    assert!(bytes.len() <= MAX_COUNTERS_BYTES);
}

#[test]
fn un51_ledger_persists_under_lock_and_reconciles() {
    let (temp, root) = root();
    let lock = MaintenanceLock::acquire(&root).expect("lock");
    let counter = CapacityCounter {
        runs: 99,
        ..CapacityCounter::default()
    };
    store_counter(&root, &lock, &counter, true).expect("store");
    assert!(temp.path().join(COUNTERS_FILE).is_file());

    std::fs::create_dir_all(temp.path().join("runs").join("20260816T120000Z-1")).unwrap();
    std::fs::write(
        temp.path()
            .join("runs")
            .join("20260816T120000Z-1")
            .join("out.json"),
        b"{}",
    )
    .unwrap();

    let mut loaded = load_counter(&root, &lock).expect("load");
    assert_eq!(loaded.runs, 99);
    reconcile_counts_from_disk(&root, &lock, &mut loaded).expect("reconcile");
    assert_eq!(loaded.runs, 1);
    assert!(loaded.total_bytes >= 2);
}
