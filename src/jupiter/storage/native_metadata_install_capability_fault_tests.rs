use super::*;

async fn count(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(statement(sql, []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn mint_fault(fault: Fault) {
    let (direct, recovery, _schema, url) = fixture().await;
    let proxy = PgCommitFaultProxy::start(&url, fault).await;
    let mut options = sea_orm::ConnectOptions::new(proxy.url.clone());
    options.max_connections(1).min_connections(1);
    let repository =
        PostgresMetadataInstallRepository::new(Database::connect(options).await.unwrap())
            .await
            .unwrap();
    let pages = prepared("/");
    let intent = repository
        .begin_intent("cap-mint-fault", &pages)
        .await
        .unwrap();
    proxy.armed.store(true, Ordering::SeqCst);
    let error = tokio::time::timeout(
        Duration::from_secs(15),
        repository.mint_legacy_install_capability(&intent),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(matches!(
        error,
        MetadataInstallError::CommitUncertain {
            phase: MetadataCommitPhase::Intent,
            ..
        }
    ));
    proxy.wait_for_fault().await;
    assert_eq!(
        count(&direct, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    assert_eq!(
        count(&direct, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        if matches!(fault, Fault::AfterCommit) {
            1
        } else {
            0
        }
    );
    assert_eq!(
        proxy.commit_observed.load(Ordering::SeqCst),
        matches!(fault, Fault::AfterCommit)
    );
    let restarted = PostgresMetadataInstallRepository::new(recovery.clone())
        .await
        .unwrap();
    let cap = restarted
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    assert_eq!(
        count(&direct, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        1
    );
    restarted
        .install_pages_validated(&cap, pages.dag().payloads())
        .await
        .unwrap();
    restarted.finalize(&intent).await.unwrap();
}

async fn batch_fault(fault: Fault) {
    let (direct, recovery, _schema, url) = fixture().await;
    let proxy = PgCommitFaultProxy::start(&url, fault).await;
    let mut options = sea_orm::ConnectOptions::new(proxy.url.clone());
    options.max_connections(1).min_connections(1);
    let repository =
        PostgresMetadataInstallRepository::new(Database::connect(options).await.unwrap())
            .await
            .unwrap();
    let pages = prepared("/");
    let intent = repository
        .begin_intent("cap-batch-fault", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    proxy.armed.store(true, Ordering::SeqCst);
    let error = tokio::time::timeout(
        Duration::from_secs(15),
        repository.install_pages_validated(&cap, pages.dag().payloads()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(matches!(
        error,
        MetadataInstallError::CommitUncertain {
            phase: MetadataCommitPhase::Payload,
            ..
        }
    ));
    proxy.wait_for_fault().await;
    assert_eq!(
        count(&direct, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        1
    );
    assert_eq!(
        count(&direct, "SELECT count(*) FROM mst2_metadata_payload").await,
        if matches!(fault, Fault::AfterCommit) {
            pages.dag().payloads().len() as i64
        } else {
            0
        }
    );
    let restarted = PostgresMetadataInstallRepository::new(recovery.clone())
        .await
        .unwrap();
    let fresh = restarted
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    restarted
        .install_pages_validated(&fresh, pages.dag().payloads())
        .await
        .unwrap();
    let receipt = restarted.finalize(&intent).await.unwrap();
    assert_eq!(receipt.metadata_root(), pages.dag().root());
    assert_eq!(
        count(
            &direct,
            "SELECT count(*) FROM mst2_retention_root WHERE root_kind='prepare'"
        )
        .await,
        pages.dag().payloads().len() as i64
    );
    restarted
        .install_pages_validated(&fresh, pages.dag().payloads())
        .await
        .unwrap();
    assert_eq!(restarted.finalize(&intent).await.unwrap(), receipt);
}

#[tokio::test]
async fn install_capability_real_registration_commit_response_loss_recovers_fresh_proof() {
    mint_fault(Fault::AfterCommit).await;
}

#[tokio::test]
async fn install_capability_real_registration_rollback_leaves_no_freeze_or_capability() {
    mint_fault(Fault::BeforeCommit).await;
}

#[tokio::test]
async fn install_capability_real_batch_commit_response_loss_preserves_exact_bytes() {
    batch_fault(Fault::AfterCommit).await;
}

#[tokio::test]
async fn install_capability_real_batch_connection_loss_rolls_back_without_false_success() {
    batch_fault(Fault::BeforeCommit).await;
}

#[tokio::test]
async fn install_capability_failed_registration_trigger_rolls_back_freeze_atomically() {
    let (direct, recovery, _schema, _url) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(direct.clone())
        .await
        .unwrap();
    let pages = prepared("/");
    let intent = repository
        .begin_intent("cap-register-rollback", &pages)
        .await
        .unwrap();
    direct.execute_unprepared("CREATE FUNCTION test_cap_registration_fault() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'registration fault after production guard'; END $$;
        CREATE TRIGGER test_cap_registration_fault AFTER INSERT ON mst2_metadata_install_seal FOR EACH ROW
        EXECUTE FUNCTION test_cap_registration_fault()").await.unwrap();
    assert!(
        repository
            .mint_legacy_install_capability(&intent)
            .await
            .is_err()
    );
    assert_eq!(
        count(&direct, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        0
    );
    recovery
        .execute_unprepared("UPDATE mst2_metadata_prepare_page SET expected_size=expected_size")
        .await
        .unwrap();
    direct
        .execute_unprepared(
            "DROP TRIGGER test_cap_registration_fault ON mst2_metadata_install_seal;
        DROP FUNCTION test_cap_registration_fault()",
        )
        .await
        .unwrap();
    let restarted = PostgresMetadataInstallRepository::new(recovery)
        .await
        .unwrap();
    let cap = restarted
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    restarted
        .install_pages_validated(&cap, pages.dag().payloads())
        .await
        .unwrap();
}
