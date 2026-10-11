use sea_orm::DatabaseTransaction;

use super::*;

#[path = "snapshot_reader_retention_upgrade_tests.rs"]
mod upgrade;

async fn transaction(fixture: &Fixture) -> DatabaseTransaction {
    let schema = q_schema(fixture).await;
    let mono = fixture.state.storage.mono_storage();
    let txn = mono.get_connection().begin().await.unwrap();
    txn.execute_unprepared(&format!(
        "SET LOCAL search_path=\"{}\",pg_catalog,pg_temp",
        schema.replace('"', "\"\"")
    ))
    .await
    .unwrap();
    txn
}

async fn admission(txn: &DatabaseTransaction, fixture: &Fixture) -> (String, i64) {
    let reader = txn.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT operation_id::text,reader_issuance FROM mst2_metadata_begin_reader($1,$2,
           (SELECT instance_id FROM mst2_qualified_session_incarnation WHERE snapshot_id=$1 AND state='READY' LIMIT 1))",
        [fixture.snapshot.clone().into(),fixture.lease.clone().into()])).await.unwrap().unwrap();
    (
        reader.try_get("", "operation_id").unwrap(),
        reader.try_get("", "reader_issuance").unwrap(),
    )
}

async fn finish(txn: &DatabaseTransaction, ticket: &(String, i64)) {
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT mst2_metadata_finish_reader($1::uuid,$2::bigint)",
        [ticket.0.clone().into(), ticket.1.into()],
    ))
    .await
    .unwrap();
}

async fn prune(txn: &DatabaseTransaction, maximum: i32) -> i64 {
    txn.query_one_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT mst2_metadata_prune_readers($1)",
        [maximum.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get_by_index(0)
    .unwrap()
}

#[tokio::test]
async fn committed_metadata_requests_retire_owners_without_retiring_source_history() {
    // FIX-OX-23: test-only diagnostic headroom (explicit 3600 s lease) for this call site; the shared
    // fixture constructor and production files stay untouched.
    let fixture = Fixture::new_in_publication_mode_with_options(
        true,
        0,
        &[],
        false,
        true,
        super::super::FixtureOptions {
            lease_seconds: Some(3600),
            ..super::super::FixtureOptions::default()
        },
    )
    .await;
    let initial = q_count(
        &fixture,
        "SELECT high_water FROM {q}.mst2_metadata_reader_issuance",
    )
    .await;
    let certificate_count = q_count(
        &fixture,
        "SELECT count(*) FROM {q}.mst2_metadata_page_certificate",
    )
    .await;
    let source_count = q_count(
        &fixture,
        "SELECT count(*) FROM {q}.mst2_metadata_source_root_attestation",
    )
    .await;
    for _ in 0..96 {
        success_json(
            fixture
                .app
                .clone()
                .oneshot(fixture.request(
                    "POST",
                    "lookup",
                    Body::from(json!({"paths":["/file"]}).to_string()),
                ))
                .await
                .unwrap(),
        )
        .await;
    }
    assert_eq!(
        q_count(
            &fixture,
            "SELECT high_water FROM {q}.mst2_metadata_reader_issuance"
        )
        .await,
        initial + 96
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation"
        )
        .await,
        1
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='ACTIVE'"
        )
        .await,
        0
    );
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,0);
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_page_certificate"
        )
        .await,
        certificate_count
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_source_root_attestation"
        )
        .await,
        source_count
    );
    success_json(
        fixture
            .app
            .clone()
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn terminal_owner_survives_its_deferred_completion_and_prunes_in_a_later_transaction() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let txn = transaction(&fixture).await;
    let ticket = admission(&txn, &fixture).await;
    txn.commit().await.unwrap();
    for forbidden in [
        "DELETE FROM mst2_metadata_reader_operation",
        "UPDATE mst2_metadata_reader_operation SET state='EXPIRED'",
        "UPDATE mst2_metadata_reader_issuance SET high_water=0",
        "DELETE FROM mst2_metadata_reader_issuance",
        "TRUNCATE mst2_metadata_reader_issuance",
        "SELECT mst2_metadata_prune_readers(65)",
    ] {
        let txn = transaction(&fixture).await;
        assert!(txn.execute_unprepared(forbidden).await.is_err());
        txn.rollback().await.unwrap();
    }
    let txn = transaction(&fixture).await;
    finish(&txn, &ticket).await;
    assert_eq!(prune(&txn, 64).await, 0);
    txn.commit().await.unwrap();
    let txn = transaction(&fixture).await;
    // A terminal no-op must not enqueue another deferred row lookup.
    txn.execute_unprepared("UPDATE mst2_metadata_reader_operation SET state=state")
        .await
        .unwrap();
    assert_eq!(prune(&txn, 0).await, 0);
    assert_eq!(prune(&txn, 64).await, 1);
    txn.commit().await.unwrap();
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation"
        )
        .await,
        0
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT high_water FROM {q}.mst2_metadata_reader_issuance"
        )
        .await,
        ticket.1
    );
}

#[tokio::test]
async fn reader_pruning_respects_the_64_owner_budget_with_a_committed_backlog() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let initial = q_count(
        &fixture,
        "SELECT high_water FROM {q}.mst2_metadata_reader_issuance",
    )
    .await;
    let txn = transaction(&fixture).await;
    // Complete each owner before admitting the next. Their terminal transaction
    // fence retains the entire backlog until its deferred checks have committed.
    for _ in 0..65 {
        let ticket = admission(&txn, &fixture).await;
        finish(&txn, &ticket).await;
    }
    assert_eq!(prune(&txn, 64).await, 0);
    txn.commit().await.unwrap();
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='FINISHED'"
        )
        .await,
        65
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='ACTIVE'"
        )
        .await,
        0
    );
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,0);
    assert_eq!(
        q_count(
            &fixture,
            "SELECT high_water FROM {q}.mst2_metadata_reader_issuance"
        )
        .await,
        initial + 65
    );
    let txn = transaction(&fixture).await;
    assert_eq!(prune(&txn, 64).await, 64);
    txn.commit().await.unwrap();
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation"
        )
        .await,
        1
    );
    let txn = transaction(&fixture).await;
    assert_eq!(prune(&txn, 64).await, 1);
    txn.commit().await.unwrap();
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation"
        )
        .await,
        0
    );
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn stale_actual_reader_cannot_read_or_finish_a_reissued_uuid() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let admitted = Arc::new(Barrier::new(2));
    let resume = Arc::new(Barrier::new(2));
    let request = fixture.request(
        "POST",
        "metadata/pages",
        Body::from(
            json!({"encoding":"identity","items":[{"directory_path":"/nested"}]}).to_string(),
        ),
    );
    let app = fixture.app.clone();
    let pending = tokio::spawn(with_rooted_reader_barriers(
        admitted.clone(),
        resume.clone(),
        async move { app.oneshot(request).await.unwrap() },
    ));
    tokio::time::timeout(Duration::from_secs(60), admitted.wait())
        .await
        .unwrap();
    let txn = transaction(&fixture).await;
    let owner=txn.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT operation_id::text,reader_issuance,to_jsonb(r) AS owner FROM mst2_metadata_reader_operation r WHERE state='ACTIVE'"))
        .await.unwrap().unwrap();
    let old = (
        owner.try_get::<String>("", "operation_id").unwrap(),
        owner.try_get::<i64>("", "reader_issuance").unwrap(),
    );
    let old_row: Value = owner.try_get("", "owner").unwrap();
    let anchors:Value=txn.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT jsonb_agg(to_jsonb(a)) AS anchors FROM mst2_metadata_root_anchor a WHERE anchor_kind IN ('REQUEST','READER')"))
        .await.unwrap().unwrap().try_get("","anchors").unwrap();
    finish(&txn, &old).await;
    txn.commit().await.unwrap();
    let txn = transaction(&fixture).await;
    assert_eq!(prune(&txn, 64).await, 1);
    txn.commit().await.unwrap();
    let txn = transaction(&fixture).await;
    assert!(txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_reader_operation SELECT * FROM jsonb_populate_record(NULL::mst2_metadata_reader_operation,$1::jsonb)",
        [old_row.clone().into()])).await.is_err());
    txn.rollback().await.unwrap();
    let next = old.1 + 1;
    let txn = transaction(&fixture).await;
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_reader_operation SELECT * FROM jsonb_populate_record(NULL::mst2_metadata_reader_operation,
           $1::jsonb||jsonb_build_object('reader_issuance',$2::bigint))",[old_row.into(),next.into()])).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_root_anchor SELECT (jsonb_populate_record(NULL::mst2_metadata_root_anchor,
           value||jsonb_build_object('reader_issuance',$2::bigint))).* FROM jsonb_array_elements($1::jsonb)",
        [anchors.into(),next.into()])).await.unwrap();
    txn.commit().await.unwrap();
    let txn = transaction(&fixture).await;
    let source = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT a.attestation_id::text,a.root_page,a.root_generation,a.root_certificate_digest
         FROM mst2_metadata_source_root_attestation a JOIN mst2_metadata_reader_operation r
           ON r.root_page=a.root_page AND r.root_generation=a.root_generation
         WHERE r.operation_id=$1::uuid AND r.reader_issuance=$2 LIMIT 1",
            [old.0.clone().into(), next.into()],
        ))
        .await
        .unwrap()
        .unwrap();
    let source_id: String = source.try_get("", "attestation_id").unwrap();
    let source_root: Vec<u8> = source.try_get("", "root_page").unwrap();
    let source_generation: i64 = source.try_get("", "root_generation").unwrap();
    let source_certificate: Vec<u8> = source.try_get("", "root_certificate_digest").unwrap();
    assert!(txn.query_all_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT * FROM mst2_metadata_read_source_entries($1::uuid,$2::bigint,$3::uuid,$4,$5,$6,'[]'::jsonb)",
        [old.0.clone().into(),old.1.into(),source_id.clone().into(),source_root.clone().into(),source_generation.into(),source_certificate.clone().into()])).await.is_err());
    txn.rollback().await.unwrap();
    let txn = transaction(&fixture).await;
    assert!(txn.query_all_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT * FROM mst2_metadata_read_source_entries($1::uuid,$2::bigint,$3::uuid,$4,$5,$6,'[]'::jsonb)",
        [old.0.clone().into(),next.into(),source_id.into(),source_root.into(),source_generation.into(),source_certificate.into()])).await.unwrap().is_empty());
    txn.commit().await.unwrap();
    resume.wait().await;
    let response = tokio::time::timeout(Duration::from_secs(60), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(response.status().is_server_error());
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation WHERE state='ACTIVE'"
        )
        .await,
        1
    );
    assert_eq!(q_count(&fixture,"SELECT count(*) FROM {q}.mst2_metadata_root_anchor WHERE anchor_kind IN ('REQUEST','READER')").await,2);
    let txn = transaction(&fixture).await;
    finish(&txn, &(old.0, next)).await;
    txn.commit().await.unwrap();
    fixture.counts.assert(0, 0);
}

#[tokio::test]
async fn rolled_back_reader_issuance_is_unpublished_and_extreme_issuance_fails_closed() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let initial = q_count(
        &fixture,
        "SELECT high_water FROM {q}.mst2_metadata_reader_issuance",
    )
    .await;
    let txn = transaction(&fixture).await;
    let unpublished = admission(&txn, &fixture).await;
    txn.rollback().await.unwrap();
    assert_eq!(
        q_count(
            &fixture,
            "SELECT high_water FROM {q}.mst2_metadata_reader_issuance"
        )
        .await,
        initial
    );
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation"
        )
        .await,
        0
    );
    let txn = transaction(&fixture).await;
    let published = admission(&txn, &fixture).await;
    assert_eq!(published.1, unpublished.1);
    txn.commit().await.unwrap();
    let txn = transaction(&fixture).await;
    assert_eq!(
        txn.query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT mst2_metadata_next_reader_issuance(9223372036854775806)"
        ))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index::<i64>(0)
        .unwrap(),
        i64::MAX
    );
    assert!(
        txn.query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT mst2_metadata_next_reader_issuance(9223372036854775807)"
        ))
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        q_count(
            &fixture,
            "SELECT high_water FROM {q}.mst2_metadata_reader_issuance"
        )
        .await,
        published.1
    );
    let txn = transaction(&fixture).await;
    finish(&txn, &published).await;
    txn.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "capacity soak: run explicitly against dedicated PostgreSQL"]
async fn reader_history_remains_bounded_after_more_than_65536_committed_http_requests() {
    let fixture = Fixture::new_with_pg_config(true).await;
    for _ in 0..65_537 {
        success_json(
            fixture
                .app
                .clone()
                .oneshot(fixture.request(
                    "POST",
                    "lookup",
                    Body::from(json!({"paths":["/file"]}).to_string()),
                ))
                .await
                .unwrap(),
        )
        .await;
    }
    assert_eq!(
        q_count(
            &fixture,
            "SELECT count(*) FROM {q}.mst2_metadata_reader_operation"
        )
        .await,
        1
    );
    assert!(
        q_count(
            &fixture,
            "SELECT high_water FROM {q}.mst2_metadata_reader_issuance"
        )
        .await
            > 65_536
    );
    fixture.counts.assert(0, 0);
}
