use mst2_codec::descriptor::ServingDescriptor;

use super::*;
use crate::{
    ceres::snapshot::descriptor::BuiltDescriptor,
    jupiter::storage::{
        mst2_retention::GcClaim, native_snapshot_session::PostgresNativeSessionRepository,
    },
};

fn descriptor(pages: &PreparedNativeMetadataRetention, view: u8) -> BuiltDescriptor {
    let descriptor = ServingDescriptor {
        instance_uuid: [7; 16],
        namespace_view_id: [view; 32],
        scope: pages.scope().into(),
        metadata_root: pages.dag().root(),
    };
    BuiltDescriptor {
        instance_id: uuid::Uuid::from_bytes(descriptor.instance_uuid).to_string(),
        snapshot_id: format!("sha256:{}", hex::encode(descriptor.snapshot_id().unwrap())),
        metadata_root: format!("sha256:{}", hex::encode(descriptor.metadata_root)),
        descriptor,
    }
}

fn assert_preparing_work(
    work: &LegacyPayloadInstallWork,
    pages: &[MetadataPagePayload],
    existing: &BTreeSet<[u8; 32]>,
) {
    let omitted: Vec<_> = pages
        .iter()
        .filter(|page| existing.contains(&page.id))
        .collect();
    let requested_bytes = pages.iter().map(|page| page.size).sum::<u64>();
    let omitted_bytes = omitted.iter().map(|page| page.size).sum::<u64>();
    assert_eq!(work.requested_pages, pages.len() as u64);
    assert_eq!(work.payload_pages_omitted, omitted.len() as u64);
    assert_eq!(
        work.payload_pages_encoded,
        (pages.len() - omitted.len()) as u64
    );
    assert_eq!(work.requested_payload_bytes_validated, requested_bytes);
    assert_eq!(work.payload_bytes_omitted, omitted_bytes);
    assert_eq!(work.payload_bytes_encoded, requested_bytes - omitted_bytes);
    assert_eq!(work.committed_replay_pages, 0);
    assert_eq!(work.transactions, pages.chunks(64).len() as u64);
    assert_eq!(work.registration_queries, work.transactions);
    assert_eq!(work.requested_member_queries, work.transactions);
    assert_eq!(work.classification_batches, work.transactions);
    assert!(work.metadata_parameter_bytes > 0);
    let missing_batches = pages
        .chunks(64)
        .filter(|batch| batch.iter().any(|page| !existing.contains(&page.id)))
        .count() as u64;
    assert_eq!(work.insert_statements, missing_batches);
    assert_eq!(work.byte_comparison_queries, missing_batches);
    if missing_batches == 0 {
        assert_eq!(work.payload_parameter_bytes, 0);
    } else {
        assert!(work.payload_parameter_bytes >= 4 * work.payload_bytes_encoded);
    }
}

async fn install_missing(
    repository: &PostgresMetadataInstallRepository,
    cap: &ValidatedLegacyInstallCapability,
    pages: &PreparedNativeMetadataRetention,
) -> LegacyPayloadInstallWork {
    let mut work = LegacyPayloadInstallWork::default();
    for batch in pages.dag().payloads().chunks(64) {
        work.record(
            repository
                .install_missing_pages_validated(cap, batch)
                .await
                .unwrap(),
        );
    }
    work
}

async fn payload_modes(db: &DatabaseConnection) -> Vec<(String, String)> {
    db.query_all_raw(statement(
        "SELECT tgname,tgenabled::text AS mode FROM pg_catalog.pg_trigger
         WHERE tgrelid='mst2_metadata_payload'::regclass AND NOT tgisinternal ORDER BY tgname",
        [],
    ))
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        (
            row.try_get("", "tgname").unwrap(),
            row.try_get("", "mode").unwrap(),
        )
    })
    .collect()
}

enum PayloadDamage {
    Bytes(Vec<u8>),
    Remove,
    UnbindGeneration,
}

async fn damage_payload(
    db: &DatabaseConnection,
    page: &MetadataPagePayload,
    damage: PayloadDamage,
) {
    let modes = payload_modes(db).await;
    assert!(modes.iter().all(|(_, mode)| mode == "O"));
    assert!(
        modes
            .iter()
            .any(|(name, _)| name == "mst2_metadata_payload_fenced")
    );
    assert!(
        modes
            .iter()
            .any(|(name, _)| name == "mst2_metadata_payload_removed")
    );
    assert!(
        db.execute_raw(statement(
            "UPDATE mst2_metadata_payload SET payload=payload WHERE page_id=$1",
            [page.id.to_vec().into()],
        ))
        .await
        .is_err()
    );
    let txn = db.begin().await.unwrap();
    txn.execute_raw(statement(
        "SELECT pg_advisory_xact_lock($1,hashtext(current_schema()))",
        [RETENTION_LOCK_KEY.into()],
    ))
    .await
    .unwrap();
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_payload DISABLE TRIGGER mst2_metadata_payload_fenced",
    )
    .await
    .unwrap();
    let removed = matches!(&damage, PayloadDamage::Remove);
    if removed {
        txn.execute_unprepared(
            "ALTER TABLE mst2_metadata_payload DISABLE TRIGGER mst2_metadata_payload_removed",
        )
        .await
        .unwrap();
    }
    let result = match damage {
        PayloadDamage::Bytes(bytes) => {
            assert_eq!(bytes.len(), page.bytes.len());
            txn.execute_raw(statement(
                "UPDATE mst2_metadata_payload SET payload=$2 WHERE page_id=$1",
                [page.id.to_vec().into(), bytes.into()],
            ))
            .await
            .unwrap()
        }
        PayloadDamage::Remove => txn
            .execute_raw(statement(
                "DELETE FROM mst2_metadata_payload WHERE page_id=$1",
                [page.id.to_vec().into()],
            ))
            .await
            .unwrap(),
        PayloadDamage::UnbindGeneration => txn
            .execute_raw(statement(
                "UPDATE mst2_metadata_payload SET generation=NULL WHERE page_id=$1",
                [page.id.to_vec().into()],
            ))
            .await
            .unwrap(),
    };
    assert_eq!(result.rows_affected(), 1);
    if removed {
        txn.execute_unprepared(
            "ALTER TABLE mst2_metadata_payload ENABLE TRIGGER mst2_metadata_payload_removed",
        )
        .await
        .unwrap();
    }
    txn.execute_unprepared(
        "ALTER TABLE mst2_metadata_payload ENABLE TRIGGER mst2_metadata_payload_fenced",
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(payload_modes(db).await, modes);
    assert!(
        db.execute_unprepared("UPDATE mst2_metadata_payload SET payload=payload",)
            .await
            .is_err(),
        "fault injection must restore the production UPDATE fence"
    );
}

fn corrupt(page: &MetadataPagePayload) -> Vec<u8> {
    let mut bytes = page.bytes.clone();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    bytes
}

#[tokio::test]
async fn install_missing_actual_session_new_sid_mixed_reuse_and_restart_keep_complete_oracles() {
    let (first, second, _schema) = fixture().await;
    let sessions = PostgresNativeSessionRepository::new(first.clone());
    let old = prepared(130);
    let (old_receipt, cold) = sessions
        .install_with_work(&descriptor(&old, 1), &old)
        .await
        .unwrap();
    assert_preparing_work(&cold, old.dag().payloads(), &BTreeSet::new());
    assert_eq!(old_receipt.payload_bytes(), old.dag().payload_bytes());
    let existing: BTreeSet<_> = old.dag().payloads().iter().map(|page| page.id).collect();
    let new = prepared(131);
    assert_ne!(old.dag().root(), new.dag().root());
    assert!(
        new.dag()
            .payloads()
            .iter()
            .any(|page| existing.contains(&page.id))
    );
    assert!(
        new.dag()
            .payloads()
            .iter()
            .any(|page| !existing.contains(&page.id))
    );
    first.execute_unprepared(
        "CREATE FUNCTION test_missing_mixed_existing_trap() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF EXISTS(SELECT 1 FROM mst2_metadata_payload WHERE page_id=NEW.page_id) THEN
           RAISE EXCEPTION 'mixed reuse attempted existing payload INSERT'; END IF; RETURN NEW; END $$;
         CREATE TRIGGER test_missing_mixed_existing_trap BEFORE INSERT ON mst2_metadata_payload
         FOR EACH ROW EXECUTE FUNCTION test_missing_mixed_existing_trap()",
    ).await.unwrap();
    let old_state = domain_state_for_test(&first).await;
    assert!(first.execute_unprepared(
        "INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload)
         SELECT page_id,metadata_codec,byte_size,payload FROM mst2_metadata_payload ORDER BY page_id LIMIT 1
         ON CONFLICT(page_id) DO NOTHING",
    ).await.unwrap_err().to_string().contains("mixed reuse attempted existing payload INSERT"));
    assert_eq!(domain_state_for_test(&first).await, old_state);
    let (new_receipt, mixed) = sessions
        .install_with_work(&descriptor(&new, 2), &new)
        .await
        .unwrap();
    assert_preparing_work(&mixed, new.dag().payloads(), &existing);
    assert_ne!(
        old_receipt.intent().prepare_id(),
        new_receipt.intent().prepare_id()
    );
    let union: BTreeSet<_> = existing
        .iter()
        .copied()
        .chain(new.dag().payloads().iter().map(|page| page.id))
        .collect();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        union.len() as i64
    );
    first
        .execute_unprepared(
            "DROP TRIGGER test_missing_mixed_existing_trap ON mst2_metadata_payload;
         DROP FUNCTION test_missing_mixed_existing_trap()",
        )
        .await
        .unwrap();
    first
        .execute_unprepared(
            "CREATE FUNCTION test_missing_no_insert() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'full reuse attempted payload INSERT'; END $$;
         CREATE TRIGGER test_missing_no_insert BEFORE INSERT ON mst2_metadata_payload
         FOR EACH STATEMENT EXECUTE FUNCTION test_missing_no_insert()",
        )
        .await
        .unwrap();
    drop(sessions);
    let restarted = PostgresNativeSessionRepository::new(second.clone());
    let (reused_receipt, reused) = restarted
        .install_with_work(&descriptor(&new, 3), &new)
        .await
        .unwrap();
    assert_preparing_work(&reused, new.dag().payloads(), &union);
    assert_ne!(
        new_receipt.intent().prepare_id(),
        reused_receipt.intent().prepare_id()
    );
    assert_eq!(reused_receipt.metadata_root(), new.dag().root());
    let installer = PostgresMetadataInstallRepository::new(second)
        .await
        .unwrap();
    installer
        .restore_session_dag(old_receipt.intent().prepare_id())
        .await
        .unwrap();
    installer
        .restore_session_dag(new_receipt.intent().prepare_id())
        .await
        .unwrap();
    installer
        .restore_session_dag(reused_receipt.intent().prepare_id())
        .await
        .unwrap();
    let cap = installer
        .mint_legacy_install_capability(reused_receipt.intent())
        .await
        .unwrap();
    installer
        .install_missing_pages_validated(&cap, &new.dag().payloads()[..64])
        .await
        .unwrap();
    let wrapper_receipt = restarted.install(&descriptor(&new, 4), &new).await.unwrap();
    assert_ne!(
        wrapper_receipt.intent().prepare_id(),
        reused_receipt.intent().prepare_id()
    );
    assert_eq!(wrapper_receipt.metadata_root(), new.dag().root());
    installer
        .restore_session_dag(wrapper_receipt.intent().prepare_id())
        .await
        .unwrap();
    first.execute_unprepared(
        "DROP TRIGGER test_missing_no_insert ON mst2_metadata_payload; DROP FUNCTION test_missing_no_insert()",
    ).await.unwrap();
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE state='COMMITTED'"
        )
        .await,
        4
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_retention_node WHERE state='LIVE'"
        )
        .await,
        union.len() as i64
    );
}

#[tokio::test]
async fn install_missing_sql_statement_trap_proves_full_reuse_skips_conflict_insert() {
    let (first, _second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = repository
        .begin_intent("reuse-statement", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    install(&repository, &cap, &pages).await;
    first
        .execute_unprepared(
            "CREATE FUNCTION test_missing_insert_trap() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'payload statement trap fired'; END $$;
         CREATE TRIGGER test_missing_insert_trap BEFORE INSERT ON mst2_metadata_payload
         FOR EACH STATEMENT EXECUTE FUNCTION test_missing_insert_trap()",
        )
        .await
        .unwrap();
    let before = domain_state_for_test(&first).await;
    assert!(
        repository
            .install_pages_validated(&cap, pages.dag().payloads())
            .await
            .unwrap_err()
            .to_string()
            .contains("payload statement trap fired")
    );
    assert_eq!(domain_state_for_test(&first).await, before);
    let work = install_missing(&repository, &cap, &pages).await;
    let existing = pages.dag().payloads().iter().map(|page| page.id).collect();
    assert_preparing_work(&work, pages.dag().payloads(), &existing);
    assert_eq!(domain_state_for_test(&first).await, before);
    let receipt = repository.finalize(&intent).await.unwrap();
    let replay = repository
        .install_missing_pages_validated(&cap, pages.dag().payloads())
        .await
        .unwrap();
    assert_eq!(
        replay.committed_replay_pages,
        pages.dag().payloads().len() as u64
    );
    assert_eq!(replay.payload_pages_omitted, 0);
    assert_eq!(replay.payload_pages_encoded, replay.requested_pages);
    assert_eq!(replay.payload_bytes_encoded, pages.dag().payload_bytes());
    assert_eq!(replay.payload_bytes_omitted, 0);
    assert_eq!(replay.classification_batches, 0);
    assert_eq!(replay.insert_statements, 0);
    assert_eq!(replay.byte_comparison_queries, 1);
    assert!(replay.payload_parameter_bytes >= 2 * replay.payload_bytes_encoded);
    assert_eq!(repository.finalize(&intent).await.unwrap(), receipt);
    first.execute_unprepared(
        "DROP TRIGGER test_missing_insert_trap ON mst2_metadata_payload; DROP FUNCTION test_missing_insert_trap()",
    ).await.unwrap();
}

#[tokio::test]
async fn install_missing_invalid_batch_and_foreign_scope_cannot_write_any_member() {
    let (first, _second, _schema) = fixture().await;
    let (alien, _other, _alien_schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let foreign = PostgresMetadataInstallRepository::new(alien.clone())
        .await
        .unwrap();
    let pages = prepared(70);
    let intent = repository
        .begin_intent("missing-bounds", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    let member = pages.dag().payloads()[0].clone();
    let mut bad_size = member.clone();
    bad_size.size += 1;
    let mut bad_digest = member.clone();
    bad_digest.bytes = corrupt(&member);
    let malformed = MetadataPagePayload {
        id: page_id(&[0; HEADER_LEN]),
        size: HEADER_LEN as u64,
        bytes: vec![0; HEADER_LEN],
    };
    let too_big = MetadataPagePayload {
        id: page_id(&vec![0; PAGE_MAX_BYTES + 1]),
        size: (PAGE_MAX_BYTES + 1) as u64,
        bytes: vec![0; PAGE_MAX_BYTES + 1],
    };
    let other = prepared(71);
    let stranger = other
        .dag()
        .payloads()
        .iter()
        .find(|page| {
            !pages
                .dag()
                .payloads()
                .iter()
                .any(|member| member.id == page.id)
        })
        .unwrap()
        .clone();
    let before = domain_state_for_test(&first).await;
    for batch in [
        vec![],
        pages.dag().payloads()[..65].to_vec(),
        vec![member.clone(), member.clone()],
        vec![member.clone(), bad_size],
        vec![member.clone(), bad_digest],
        vec![member.clone(), malformed],
        vec![member.clone(), too_big],
        vec![member, stranger],
    ] {
        assert!(
            repository
                .install_missing_pages_validated(&cap, &batch)
                .await
                .is_err()
        );
        assert_eq!(domain_state_for_test(&first).await, before);
    }
    let foreign_before = domain_state_for_test(&alien).await;
    assert!(
        foreign
            .install_missing_pages_validated(&cap, &pages.dag().payloads()[..64])
            .await
            .is_err()
    );
    assert_eq!(domain_state_for_test(&alien).await, foreign_before);
    assert_eq!(domain_state_for_test(&first).await, before);
}

#[tokio::test]
async fn install_missing_actual_size_conflict_rejects_before_inserting_missing_siblings() {
    let (first, _second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = repository
        .begin_intent("actual-size", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    let page = &pages.dag().payloads()[0];
    let mut wrong = page.bytes.clone();
    wrong.push(0);
    assert!(wrong.len() <= PAGE_MAX_BYTES);
    first.execute_raw(statement(
        "INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload) VALUES($1,1,$2,$3)",
        [page.id.to_vec().into(), (wrong.len() as i32).into(), wrong.into()],
    )).await.unwrap();
    let before = domain_state_for_test(&first).await;
    assert!(
        repository
            .install_missing_pages_validated(&cap, pages.dag().payloads())
            .await
            .unwrap_err()
            .to_string()
            .contains("payload profile conflicts")
    );
    assert_eq!(domain_state_for_test(&first).await, before);
    assert!(repository.finalize(&intent).await.is_err());
    assert_eq!(domain_state_for_test(&first).await, before);
}

#[tokio::test]
async fn install_missing_actual_codec_check_and_after_insert_fault_roll_back_entire_batch() {
    let (first, _second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = repository
        .begin_intent("missing-atomic", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    repository
        .install_pages_validated(&cap, &pages.dag().payloads()[..1])
        .await
        .unwrap();
    let original_modes = payload_modes(&first).await;
    let before = domain_state_for_test(&first).await;
    let failed = hex::encode(pages.dag().payloads()[1].id);
    first.execute_unprepared(&format!(
        "CREATE FUNCTION test_missing_bad_codec() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.page_id=decode('{failed}','hex') THEN NEW.metadata_codec:=2; END IF; RETURN NEW; END $$;
         CREATE TRIGGER test_missing_bad_codec BEFORE INSERT ON mst2_metadata_payload
         FOR EACH ROW EXECUTE FUNCTION test_missing_bad_codec()"
    )).await.unwrap();
    assert!(
        repository
            .install_missing_pages_validated(&cap, pages.dag().payloads())
            .await
            .unwrap_err()
            .to_string()
            .contains("mst2_metadata_payload_metadata_codec_check")
    );
    assert_eq!(domain_state_for_test(&first).await, before);
    first.execute_unprepared(
        "DROP TRIGGER test_missing_bad_codec ON mst2_metadata_payload; DROP FUNCTION test_missing_bad_codec();
         CREATE FUNCTION test_missing_after_insert_fault() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'missing insert fault after production guards'; END $$;
         CREATE TRIGGER test_missing_after_insert_fault AFTER INSERT ON mst2_metadata_payload
         FOR EACH STATEMENT EXECUTE FUNCTION test_missing_after_insert_fault()",
    ).await.unwrap();
    assert!(
        repository
            .install_missing_pages_validated(&cap, pages.dag().payloads())
            .await
            .unwrap_err()
            .to_string()
            .contains("missing insert fault after production guards")
    );
    assert_eq!(domain_state_for_test(&first).await, before);
    first.execute_unprepared(
        "DROP TRIGGER test_missing_after_insert_fault ON mst2_metadata_payload; DROP FUNCTION test_missing_after_insert_fault()",
    ).await.unwrap();
    assert_eq!(payload_modes(&first).await, original_modes);
    let existing = pages.dag().payloads()[..1]
        .iter()
        .map(|page| page.id)
        .collect();
    let work = install_missing(&repository, &cap, &pages).await;
    assert_preparing_work(&work, pages.dag().payloads(), &existing);
    repository.finalize(&intent).await.unwrap();
}

#[tokio::test]
async fn install_missing_presence_hint_never_certifies_metadata_consistent_corrupt_body() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = repository
        .begin_intent("hint-not-proof", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    let page = &pages.dag().payloads()[0];
    first.execute_raw(statement(
        "INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload) VALUES($1,1,$2,$3)",
        [page.id.to_vec().into(), (page.size as i32).into(), corrupt(page).into()],
    )).await.unwrap();
    let poisoned = domain_state_for_test(&first).await;
    assert!(
        repository
            .install_pages_validated(&cap, pages.dag().payloads())
            .await
            .unwrap_err()
            .to_string()
            .contains("payload identity conflicts with stored bytes")
    );
    assert_eq!(domain_state_for_test(&first).await, poisoned);
    let work = install_missing(&repository, &cap, &pages).await;
    assert_preparing_work(&work, pages.dag().payloads(), &BTreeSet::from([page.id]));
    let before = domain_state_for_test(&first).await;
    assert!(repository.finalize(&intent).await.is_err());
    let restarted = PostgresMetadataInstallRepository::new(second)
        .await
        .unwrap();
    assert!(restarted.finalize(&intent).await.is_err());
    assert!(
        restarted
            .restore_session_dag(intent.prepare_id())
            .await
            .is_err()
    );
    assert_eq!(domain_state_for_test(&first).await, before);
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE state='COMMITTED'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_root").await,
        0
    );
}

#[tokio::test]
async fn install_missing_committed_replay_rejects_corrupt_and_missing_bytes_without_repair() {
    for remove in [false, true] {
        let (first, second, _schema) = fixture().await;
        let repository = PostgresMetadataInstallRepository::new(first.clone())
            .await
            .unwrap();
        let pages = prepared(2);
        let intent = repository
            .begin_intent("committed-no-repair", &pages)
            .await
            .unwrap();
        let cap = repository
            .mint_legacy_install_capability(&intent)
            .await
            .unwrap();
        install_missing(&repository, &cap, &pages).await;
        repository.finalize(&intent).await.unwrap();
        let page = &pages.dag().payloads()[0];
        let damage = if remove {
            PayloadDamage::Remove
        } else {
            PayloadDamage::Bytes(corrupt(page))
        };
        damage_payload(&first, page, damage).await;
        let before = domain_state_for_test(&first).await;
        assert!(
            repository
                .install_missing_pages_validated(&cap, pages.dag().payloads())
                .await
                .is_err()
        );
        assert!(repository.finalize(&intent).await.is_err());
        let restarted = PostgresMetadataInstallRepository::new(second)
            .await
            .unwrap();
        assert!(
            restarted
                .mint_legacy_install_capability(&intent)
                .await
                .is_err()
        );
        assert!(
            restarted
                .restore_session_dag(intent.prepare_id())
                .await
                .is_err()
        );
        assert_eq!(domain_state_for_test(&first).await, before);
        assert_eq!(
            scalar(&first, "SELECT count(*) FROM mst2_retention_root").await,
            pages.dag().payloads().len() as i64
        );
    }
}

#[tokio::test]
async fn install_missing_generic_generation_reuse_requires_exact_live_physical_binding() {
    let (first, second, _schema) = fixture().await;
    let generic = generations::PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let bound = generic
        .begin_intent("missing-bound-generic", &pages)
        .await
        .unwrap();
    generic
        .install_pages(&bound, pages.dag().payloads())
        .await
        .unwrap();
    generic.finalize(&bound).await.unwrap();
    let intent = repository
        .begin_intent("missing-share-generic", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    let existing = pages.dag().payloads().iter().map(|page| page.id).collect();
    let work = install_missing(&repository, &cap, &pages).await;
    assert_preparing_work(&work, pages.dag().payloads(), &existing);
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NOT NULL"
        )
        .await,
        pages.dag().payloads().len() as i64
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare_page WHERE generation IS NULL"
        )
        .await,
        pages.dag().payloads().len() as i64
    );
    let page = &pages.dag().payloads()[0];
    damage_payload(&first, page, PayloadDamage::UnbindGeneration).await;
    let before = domain_state_for_test(&first).await;
    let restarted = PostgresMetadataInstallRepository::new(second)
        .await
        .unwrap();
    assert!(
        restarted
            .install_missing_pages_validated(&cap, pages.dag().payloads())
            .await
            .is_err()
    );
    assert_eq!(domain_state_for_test(&first).await, before);
}

#[tokio::test]
async fn install_missing_qualified_owner_cannot_be_adopted_by_generic_preparation() {
    let (first, _second, _schema) = fixture().await;
    let qualified = generations::qualified::PostgresQualifiedMetadataRepository::new(first.clone())
        .await
        .unwrap();
    let legacy = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let bound = qualified
        .begin_intent("missing-qualified", &pages)
        .await
        .unwrap();
    qualified
        .install_pages(&bound, pages.dag().payloads())
        .await
        .unwrap();
    let before = domain_state_for_test(&first).await;
    assert!(
        legacy
            .begin_intent("missing-wrong-domain", &pages)
            .await
            .unwrap_err()
            .to_string()
            .contains("qualified mapping cannot cross domain/current/state fence")
    );
    let bound_intent = MetadataPrepareIntent {
        prepare_id: bound.prepare_id().into(),
        operation_id: bound.operation_id().into(),
        manifest_digest: bound.manifest_digest(),
    };
    assert!(
        legacy
            .mint_legacy_install_capability(&bound_intent)
            .await
            .is_err()
    );
    assert_eq!(domain_state_for_test(&first).await, before);
}

#[tokio::test]
async fn install_missing_postclassification_release_gc_and_tombstone_keep_finalization_fenced() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let graph = PostgresRetentionRepository::new(second);
    let pages = prepared(2);
    let old = repository
        .begin_intent("old-covered", &pages)
        .await
        .unwrap();
    let old_cap = repository
        .mint_legacy_install_capability(&old)
        .await
        .unwrap();
    install_missing(&repository, &old_cap, &pages).await;
    repository.finalize(&old).await.unwrap();
    let next = repository
        .begin_intent("after-classification", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&next)
        .await
        .unwrap();
    let work = install_missing(&repository, &cap, &pages).await;
    let existing = pages.dag().payloads().iter().map(|page| page.id).collect();
    assert_preparing_work(&work, pages.dag().payloads(), &existing);
    let root = node_id(&pages.dag().root());
    let lease = RetentionRoot::Lease("missing-classification-lease-root".into());
    let txn = first.begin().await.unwrap();
    PostgresRetentionRepository::acquire_existing_roots_in_txn(
        &txn,
        &root,
        std::slice::from_ref(&lease),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    graph
        .release_root(&RetentionRoot::Prepare(old.prepare_id().into()))
        .await
        .unwrap();
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='lease'"
        )
        .await,
        1
    );
    let protected = domain_state_for_test(&first).await;
    assert_eq!(
        graph
            .mark_deleting("missing-gc-protected", &root)
            .await
            .unwrap(),
        GcClaim::Unavailable
    );
    assert_eq!(domain_state_for_test(&first).await, protected);
    graph.release_root(&lease).await.unwrap();
    assert_eq!(
        graph.mark_deleting("missing-gc-root", &root).await.unwrap(),
        GcClaim::Marked
    );
    let before = domain_state_for_test(&first).await;
    assert!(repository.finalize(&next).await.is_err());
    assert!(
        repository
            .install_missing_pages_validated(&cap, pages.dag().payloads())
            .await
            .is_err()
    );
    assert_eq!(domain_state_for_test(&first).await, before);
    assert_eq!(graph.node(&root).await.unwrap().unwrap().state, "DELETING");
    let completed = graph.complete_gc("missing-gc-root").await.unwrap();
    assert!(!completed.replayed);
    assert!(graph.node(&root).await.unwrap().is_none());
    let tombstoned = domain_state_for_test(&first).await;
    assert!(
        repository
            .install_missing_pages_validated(&cap, pages.dag().payloads())
            .await
            .is_err()
    );
    assert!(repository.finalize(&next).await.is_err());
    assert_eq!(domain_state_for_test(&first).await, tombstoned);
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_root").await,
        0
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        pages.dag().payloads().len() as i64
    );
}
