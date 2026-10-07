use std::sync::Arc;

use mst2_codec::metapage::{Entry, EntryKind};
use sea_orm::{Database, PaginatorTrait};
use sea_orm_migration::MigratorTrait;

use super::*;
use crate::{
    ceres::snapshot::retention_dag::MetadataDagBuilder,
    jupiter::{
        migration::Migrator,
        tests::{TestSchemaGuard, test_db_config},
    },
};

pub(super) fn prepared(children: usize) -> PreparedNativeMetadataRetention {
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    let mut entries = Vec::new();
    for index in 0..children {
        let file = [Entry::file(
            EntryKind::Regular,
            b"file",
            3,
            [index as u8; 32],
        )];
        let child = Page::build(&file).unwrap();
        builder.add_directory(&child, &file).unwrap();
        entries.push(Entry::dir(
            format!("dir-{index:03}").as_bytes(),
            page_id(&child),
        ));
    }
    let root = Page::build(&entries).unwrap();
    builder.add_directory(&root, &entries).unwrap();
    PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page_id(&root)).unwrap()),
        "/",
    )
}

pub(super) async fn fixture() -> (DatabaseConnection, DatabaseConnection, TestSchemaGuard) {
    let temp = tempfile::tempdir().unwrap();
    let (config, schema) = test_db_config(temp.path()).await;
    let first = Database::connect(config.db_url.clone()).await.unwrap();
    Migrator::up(&first, None).await.unwrap();
    let second = Database::connect(config.db_url).await.unwrap();
    (first, second, schema)
}

async fn scalar<C: ConnectionTrait>(db: &C, sql: &str) -> i64 {
    db.query_one_raw(statement(sql, []))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

async fn install(
    repository: &PostgresMetadataInstallRepository,
    capability: &ValidatedLegacyInstallCapability,
    prepared: &PreparedNativeMetadataRetention,
) {
    for batch in prepared.dag().payloads().chunks(64) {
        repository
            .install_pages_validated(capability, batch)
            .await
            .unwrap();
    }
}

async fn assert_guard(db: &DatabaseConnection, sql: &str) {
    let error = db.execute_unprepared(sql).await.expect_err(sql);
    assert!(
        error.to_string().contains("install capability")
            || error.to_string().contains("registered metadata"),
        "{error}"
    );
}

#[tokio::test]
async fn install_capability_many_batches_concurrent_mint_and_fresh_repository_finalize() {
    let (first, second, _schema) = fixture().await;
    let a = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let b = PostgresMetadataInstallRepository::new(second.clone())
        .await
        .unwrap();
    let pages = prepared(130);
    assert_eq!(pages.dag().payloads().len(), 133);
    assert_eq!(pages.dag().edges().len(), 132);
    let root = pages
        .dag()
        .payloads()
        .iter()
        .find(|page| page.id == pages.dag().root())
        .unwrap();
    let (
        Page::Branch {
            prefix,
            terminal,
            children,
        },
        count,
    ) = Page::decode(&root.bytes).unwrap()
    else {
        panic!("130 directory entries must use a canonical radix branch");
    };
    assert_eq!(prefix, b"dir-");
    assert!(terminal.is_none());
    assert_eq!(count, 130);
    assert_eq!(
        children
            .iter()
            .map(|child| (child.label, child.subtree_entries))
            .collect::<Vec<_>>(),
        [(b'0', 100), (b'1', 30)]
    );
    for child in children {
        let payload = pages
            .dag()
            .payloads()
            .iter()
            .find(|page| page.id == child.child_page_id)
            .unwrap();
        let (Page::Leaf { entries }, count) = Page::decode(&payload.bytes).unwrap() else {
            panic!("each root partition must be one canonical radix leaf");
        };
        assert_eq!(count, child.subtree_entries);
        assert_eq!(entries.len() as u64, count);
        for entry in entries {
            assert_eq!(entry.kind, EntryKind::Directory);
            assert_eq!(entry.name[4], child.label);
            assert!(
                pages
                    .dag()
                    .payloads()
                    .iter()
                    .any(|page| page.id == entry.child_root)
            );
        }
    }
    assert_eq!(
        pages
            .dag()
            .payloads()
            .chunks(64)
            .map(|batch| batch.len())
            .collect::<Vec<_>>(),
        [64, 64, 5]
    );
    let intent = a.begin_intent("many", &pages).await.unwrap();
    let (left, right) = tokio::join!(
        a.mint_legacy_install_capability(&intent),
        b.mint_legacy_install_capability(&intent)
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        1
    );
    for (index, batch) in pages.dag().payloads().chunks(64).enumerate() {
        if index == 0 {
            a.install_pages_validated(&left, batch).await.unwrap();
        } else {
            b.install_pages_validated(&right, batch).await.unwrap();
        }
    }
    drop(left);
    drop(right);
    drop(a);
    drop(b);
    let restarted = PostgresMetadataInstallRepository::new(second.clone())
        .await
        .unwrap();
    let fresh = restarted
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    install(&restarted, &fresh, &pages).await;
    let receipt = restarted.finalize(&intent).await.unwrap();
    assert_eq!(receipt.metadata_root(), pages.dag().root());
    assert_eq!(receipt.payload_bytes(), pages.dag().payload_bytes());
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NULL"
        )
        .await,
        133
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_retention_node WHERE state='LIVE'"
        )
        .await,
        133
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_retention_edge").await,
        132
    );
    let before = scalar(
        &first,
        "SELECT sum(incoming_refs)::bigint FROM mst2_retention_node",
    )
    .await;
    install(&restarted, &fresh, &pages).await;
    assert_eq!(restarted.finalize(&intent).await.unwrap(), receipt);
    assert_eq!(
        scalar(
            &first,
            "SELECT sum(incoming_refs)::bigint FROM mst2_retention_node"
        )
        .await,
        before
    );
}

#[tokio::test]
async fn install_capability_freezes_every_identity_member_anchor_and_truncate_path() {
    let (first, _second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = repository.begin_intent("freeze", &pages).await.unwrap();
    let other = repository
        .begin_intent("unregistered", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    let before = mst2_metadata_prepare::Entity::find_by_id(intent.prepare_id().to_owned())
        .one(&first)
        .await
        .unwrap()
        .unwrap();
    let member_count_before =
        scalar(&first, "SELECT count(*) FROM mst2_metadata_prepare_page").await;
    let seal_count_before = scalar(&first, "SELECT count(*) FROM mst2_metadata_install_seal").await;
    let restrict_error = first
        .execute_unprepared("TRUNCATE mst2_metadata_prepare_page")
        .await
        .expect_err("plain truncate must preserve foreign-key protection");
    assert!(
        restrict_error
            .to_string()
            .contains("cannot truncate a table referenced in a foreign key constraint"),
        "{restrict_error}"
    );
    assert_eq!(
        first.execute_raw(statement(
            "UPDATE mst2_metadata_prepare SET verification_revision=verification_revision WHERE prepare_id=$1",
            [intent.prepare_id().into()],
        )).await.unwrap().rows_affected(),
        1
    );
    for mutation in [
        "operation_id='changed'",
        "manifest_digest=decode(repeat('01',32),'hex')",
        "canonical_plan=canonical_plan||decode('ff','hex')",
        "source_domain='other'",
        "tagged_root_tree_oid='sha1:bad'",
        "scope='/changed'",
        "schema_version=3",
        "metadata_codec=2",
        "materialization_policy=2",
        "fs_semantics=2",
        "access_projection=2",
        "verification_revision=verification_revision+1",
        "projection_revision=2",
        "metadata_root=decode(repeat('01',32),'hex')",
        "node_count=node_count+1",
        "edge_count=edge_count+1",
        "total_bytes=total_bytes+1",
        "created_at=created_at+interval '1 second'",
    ] {
        assert_guard(
            &first,
            &format!(
                "UPDATE mst2_metadata_prepare SET {mutation} WHERE prepare_id='{}'",
                intent.prepare_id()
            ),
        )
        .await;
    }
    for sql in [
        format!(
            "DELETE FROM mst2_metadata_prepare WHERE prepare_id='{}'",
            intent.prepare_id()
        ),
        format!(
            "UPDATE mst2_metadata_prepare_page SET expected_size=expected_size+1 WHERE prepare_id='{}'",
            intent.prepare_id()
        ),
        format!(
            "UPDATE mst2_metadata_prepare_page SET generation=1 WHERE prepare_id='{}'",
            intent.prepare_id()
        ),
        format!(
            "UPDATE mst2_metadata_prepare_page SET prepare_id='{}' WHERE prepare_id='{}'",
            other.prepare_id(),
            intent.prepare_id()
        ),
        format!(
            "UPDATE mst2_metadata_prepare_page SET prepare_id='{}' WHERE prepare_id='{}'",
            intent.prepare_id(),
            other.prepare_id()
        ),
        format!(
            "DELETE FROM mst2_metadata_prepare_page WHERE prepare_id='{}'",
            intent.prepare_id()
        ),
        format!(
            "INSERT INTO mst2_metadata_prepare_page(prepare_id,page_id,expected_size) VALUES('{}',decode(repeat('ff',32),'hex'),64)",
            intent.prepare_id()
        ),
        "UPDATE mst2_metadata_install_seal SET members_digest=decode(repeat('ff',32),'hex')".into(),
        "DELETE FROM mst2_metadata_install_seal".into(),
        "TRUNCATE mst2_metadata_install_seal".into(),
        "TRUNCATE mst2_metadata_prepare_page CASCADE".into(),
        "TRUNCATE mst2_metadata_prepare CASCADE".into(),
    ] {
        assert_guard(&first, &sql).await;
    }
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_prepare_page").await,
        member_count_before
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        seal_count_before
    );
    assert_eq!(
        mst2_metadata_prepare::Entity::find_by_id(intent.prepare_id().to_owned())
            .one(&first)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        mst2_metadata_prepare_page::Entity::find()
            .filter(mst2_metadata_prepare_page::Column::PrepareId.eq(intent.prepare_id()))
            .count(&first)
            .await
            .unwrap(),
        3
    );
    install(&repository, &cap, &pages).await;
    repository.finalize(&intent).await.unwrap();
}

#[tokio::test]
async fn install_capability_unregistered_corruption_and_seal_conflict_cannot_mint() {
    let (first, _second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = repository.begin_intent("corrupt", &pages).await.unwrap();
    first
        .execute_unprepared("UPDATE mst2_metadata_prepare SET projection_revision=2")
        .await
        .unwrap();
    assert!(
        repository
            .mint_legacy_install_capability(&intent)
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        0
    );
    first.execute_unprepared("UPDATE mst2_metadata_prepare SET projection_revision=1,canonical_plan=canonical_plan||decode('ff','hex')").await.unwrap();
    assert!(
        repository
            .mint_legacy_install_capability(&intent)
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        0
    );
    let mapping = repository
        .begin_intent("mapping-corrupt", &pages)
        .await
        .unwrap();
    first.execute_raw(statement(
        "UPDATE mst2_metadata_prepare_page SET expected_size=expected_size+1 WHERE prepare_id=$1",
        [mapping.prepare_id().into()],
    )).await.unwrap();
    assert!(
        repository
            .mint_legacy_install_capability(&mapping)
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        0
    );
    first.execute_raw(statement(
        "UPDATE mst2_metadata_prepare_page SET expected_size=expected_size-1 WHERE prepare_id=$1",
        [mapping.prepare_id().into()],
    )).await.unwrap();
    let intent = repository.begin_intent("conflict", &pages).await.unwrap();
    let sql=format!("INSERT INTO mst2_metadata_install_seal(prepare_id,operation_id,manifest_digest,members_digest,primary_scope,install_seal)
        SELECT prepare_id,operation_id,manifest_digest,decode(repeat('00',32),'hex'),convert_to('[]','UTF8'),decode(repeat('00',32),'hex')
        FROM mst2_metadata_prepare WHERE prepare_id='{}'",intent.prepare_id());
    assert_guard(&first, &sql).await;
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    install(&repository, &cap, &pages).await;
}

#[tokio::test]
async fn install_capability_bounded_invalid_batches_are_atomic() {
    let (first, _second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(70);
    let intent = repository.begin_intent("invalid", &pages).await.unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    let payload = &pages.dag().payloads()[0];
    assert!(repository.install_pages_validated(&cap, &[]).await.is_err());
    assert!(
        repository
            .install_pages_validated(&cap, &pages.dag().payloads()[..65])
            .await
            .is_err()
    );
    assert!(
        repository
            .install_pages_validated(&cap, &[payload.clone(), payload.clone()])
            .await
            .is_err()
    );
    let mut wrong = payload.clone();
    wrong.size += 1;
    assert!(
        repository
            .install_pages_validated(&cap, &[payload.clone(), wrong])
            .await
            .is_err()
    );
    let mut wrong = payload.clone();
    wrong.id = [255; 32];
    assert!(
        repository
            .install_pages_validated(&cap, &[wrong])
            .await
            .is_err()
    );
    let mut wrong = payload.clone();
    wrong.bytes = vec![0; PAGE_MAX_BYTES + 1];
    wrong.size = wrong.bytes.len() as u64;
    wrong.id = page_id(&wrong.bytes);
    assert!(
        repository
            .install_pages_validated(&cap, &[wrong])
            .await
            .is_err()
    );
    let mut wrong = payload.clone();
    wrong.bytes = vec![0; HEADER_LEN];
    wrong.size = wrong.bytes.len() as u64;
    wrong.id = page_id(&wrong.bytes);
    assert!(
        repository
            .install_pages_validated(&cap, &[wrong])
            .await
            .is_err()
    );
    let stranger = prepared(71);
    let stranger = stranger
        .dag()
        .payloads()
        .iter()
        .find(|p| {
            !pages
                .dag()
                .payloads()
                .iter()
                .any(|member| member.id == p.id)
        })
        .unwrap();
    assert!(
        repository
            .install_pages_validated(&cap, std::slice::from_ref(stranger))
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    let mut wrong = payload.bytes.clone();
    let last = wrong.len() - 1;
    wrong[last] ^= 1;
    first.execute_raw(statement("INSERT INTO mst2_metadata_payload(page_id,metadata_codec,byte_size,payload) VALUES($1,1,$2,$3)",
        [payload.id.to_vec().into(),(payload.size as i32).into(),wrong.into()])).await.unwrap();
    assert!(
        repository
            .install_pages_validated(&cap, &pages.dag().payloads()[..64])
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        1
    );
}

#[tokio::test]
async fn install_capability_raw_committed_missing_bytes_does_not_repair_and_retirement_rejects() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = repository.begin_intent("promoted", &pages).await.unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    second.execute_raw(statement("UPDATE mst2_metadata_prepare SET state='COMMITTED',committed_at=clock_timestamp() WHERE prepare_id=$1",
        [intent.prepare_id().into()])).await.unwrap();
    assert!(
        repository
            .install_pages_validated(&cap, pages.dag().payloads())
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    assert!(
        repository
            .mint_legacy_install_capability(&intent)
            .await
            .is_err()
    );
    let intent = repository.begin_intent("retired", &pages).await.unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    install(&repository, &cap, &pages).await;
    repository.finalize(&intent).await.unwrap();
    second.execute_raw(statement("UPDATE mst2_metadata_prepare SET coverage_retired_at=clock_timestamp() WHERE prepare_id=$1",
        [intent.prepare_id().into()])).await.unwrap();
    assert!(
        repository
            .install_pages_validated(&cap, pages.dag().payloads())
            .await
            .is_err()
    );
    assert!(
        repository
            .mint_legacy_install_capability(&intent)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn install_capability_cross_primary_schema_repeatable_read_and_temp_spoof_refuse() {
    let (first, second, _schema) = fixture().await;
    let (alien, _unused, _alien_schema) = fixture().await;
    let a = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let b = PostgresMetadataInstallRepository::new(alien.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = a.begin_intent("scope", &pages).await.unwrap();
    let cap = a.mint_legacy_install_capability(&intent).await.unwrap();
    let alien_intent = b.begin_intent("scope", &pages).await.unwrap();
    assert_ne!(alien_intent.prepare_id(), intent.prepare_id());
    let copied_prepare = first.query_one_raw(statement(
        "SELECT row_to_json(p)::text AS copied FROM mst2_metadata_prepare p WHERE prepare_id=$1",
        [intent.prepare_id().into()],
    )).await.unwrap().unwrap().try_get::<String>("", "copied").unwrap();
    let copied_members = first.query_one_raw(statement(
        "SELECT json_agg(m)::text AS copied FROM mst2_metadata_prepare_page m WHERE prepare_id=$1",
        [intent.prepare_id().into()],
    )).await.unwrap().unwrap().try_get::<String>("", "copied").unwrap();
    alien
        .execute_raw(statement(
            "DELETE FROM mst2_metadata_prepare_page WHERE prepare_id=$1",
            [alien_intent.prepare_id().into()],
        ))
        .await
        .unwrap();
    alien
        .execute_raw(statement(
            "DELETE FROM mst2_metadata_prepare WHERE prepare_id=$1",
            [alien_intent.prepare_id().into()],
        ))
        .await
        .unwrap();
    alien.execute_raw(statement("INSERT INTO mst2_metadata_prepare SELECT * FROM json_populate_record(NULL::mst2_metadata_prepare,$1::json)",
        [copied_prepare.into()])).await.unwrap();
    alien.execute_raw(statement("INSERT INTO mst2_metadata_prepare_page SELECT * FROM json_populate_recordset(NULL::mst2_metadata_prepare_page,$1::json)",
        [copied_members.into()])).await.unwrap();
    let own_scope_cap = b.mint_legacy_install_capability(&intent).await.unwrap();
    assert!(
        b.install_pages_validated(&cap, pages.dag().payloads())
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&alien, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    install(&b, &own_scope_cap, &pages).await;
    let txn = second
        .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
        .await
        .unwrap();
    assert!(a.capability_barrier(&txn).await.is_err());
    assert!(
        txn.execute_unprepared("UPDATE mst2_metadata_prepare SET scope='/rr'")
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    let txn = second.begin().await.unwrap();
    let schema = a.storage_scope.schema.clone();
    txn.execute_unprepared("SET LOCAL search_path=pg_catalog")
        .await
        .unwrap();
    assert!(a.capability_barrier(&txn).await.is_err());
    let sql = format!(
        "UPDATE \"{}\".mst2_metadata_prepare SET scope='/wrongpath'",
        schema.replace('"', "\"\"")
    );
    assert!(txn.execute_unprepared(&sql).await.is_err());
    txn.rollback().await.unwrap();
    let txn = second.begin().await.unwrap();
    txn.execute_unprepared(
        "CREATE TEMP TABLE mst2_metadata_install_seal(prepare_id text);
        CREATE TEMP TABLE mst2_metadata_storage_scope(singleton integer,storage_uuid text)",
    )
    .await
    .unwrap();
    let raw_sql = format!(
        "UPDATE \"{}\".mst2_metadata_prepare SET scope='/raw-temp-spoof'",
        schema.replace('"', "\"\"")
    );
    assert!(
        txn.execute_unprepared(&raw_sql)
            .await
            .unwrap_err()
            .to_string()
            .contains("registered metadata")
    );
    txn.rollback().await.unwrap();
    let txn = second.begin().await.unwrap();
    txn.execute_unprepared(
        "CREATE TEMP TABLE mst2_metadata_install_seal(prepare_id text);
        CREATE TEMP TABLE mst2_metadata_storage_scope(singleton integer,storage_uuid text)",
    )
    .await
    .unwrap();
    a.capability_barrier(&txn).await.unwrap();
    assert_eq!(
        scalar(&txn, "SELECT count(*) FROM mst2_metadata_install_seal").await,
        1
    );
    assert!(
        txn.execute_unprepared("UPDATE mst2_metadata_prepare SET scope='/spoof'")
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    install(&a, &cap, &pages).await;
}

#[tokio::test]
async fn install_capability_reuses_bound_generic_bytes_without_adopting_qualified_generation() {
    let (first, _second, _schema) = fixture().await;
    let generic = generations::PostgresMetadataGenerationRepository::new(first.clone())
        .await
        .unwrap();
    let legacy = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let bound = generic.begin_intent("bound-generic", &pages).await.unwrap();
    generic
        .install_pages(&bound, pages.dag().payloads())
        .await
        .unwrap();
    generic.finalize(&bound).await.unwrap();
    let intent = legacy.begin_intent("legacy-share", &pages).await.unwrap();
    let cap = legacy
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    install(&legacy, &cap, &pages).await;
    legacy.finalize(&intent).await.unwrap();
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NOT NULL"
        )
        .await,
        3
    );
    assert_eq!(
        scalar(
            &first,
            "SELECT count(*) FROM mst2_metadata_prepare_page WHERE generation IS NULL"
        )
        .await,
        3
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
    let (qualified_db, _other, _q_schema) = fixture().await;
    let qualified =
        generations::qualified::PostgresQualifiedMetadataRepository::new(qualified_db.clone())
            .await
            .unwrap();
    let q_legacy = PostgresMetadataInstallRepository::new(qualified_db.clone())
        .await
        .unwrap();
    let bound = qualified.begin_intent("qualified", &pages).await.unwrap();
    qualified
        .install_pages(&bound, pages.dag().payloads())
        .await
        .unwrap();
    let q_state = domain_state_for_test(&qualified_db).await;
    let error = q_legacy
        .begin_intent("generic-refusal", &pages)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("qualified mapping cannot cross domain/current/state fence"),
        "{error}"
    );
    assert_eq!(domain_state_for_test(&qualified_db).await, q_state);
    assert_eq!(
        scalar(&qualified_db, "SELECT count(*) FROM mst2_metadata_payload").await,
        3
    );
    assert_eq!(
        scalar(&qualified_db, "SELECT count(*) FROM mst2_retention_node").await,
        0
    );

    let (legacy_db, _other, _legacy_schema) = fixture().await;
    let legacy = PostgresMetadataInstallRepository::new(legacy_db.clone())
        .await
        .unwrap();
    let qualified =
        generations::qualified::PostgresQualifiedMetadataRepository::new(legacy_db.clone())
            .await
            .unwrap();
    let intent = legacy.begin_intent("generic-first", &pages).await.unwrap();
    let cap = legacy
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    let generic_state = domain_state_for_test(&legacy_db).await;
    let error = qualified
        .begin_intent("qualified-refusal", &pages)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("metadata lifetime has durable or ambiguous coverage"),
        "{error}"
    );
    assert_eq!(domain_state_for_test(&legacy_db).await, generic_state);
    install(&legacy, &cap, &pages).await;
    assert_eq!(
        legacy.finalize(&intent).await.unwrap().metadata_root(),
        pages.dag().root()
    );
    assert_eq!(
        scalar(
            &legacy_db,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NULL"
        )
        .await,
        3
    );
    assert_eq!(
        scalar(
            &legacy_db,
            "SELECT count(*) FROM mst2_retention_node WHERE state='LIVE'"
        )
        .await,
        3
    );
    assert_eq!(
        scalar(&legacy_db, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        0
    );
    assert_eq!(
        scalar(&legacy_db, "SELECT count(*) FROM mst2_metadata_current").await,
        0
    );
    assert_eq!(
        scalar(&legacy_db, "SELECT count(*) FROM mst2_metadata_graph_node").await,
        0
    );
}

async fn domain_state_for_test(db: &DatabaseConnection) -> Vec<(String, Vec<String>)> {
    let mut state = Vec::new();
    for table in [
        "mst2_metadata_prepare",
        "mst2_metadata_prepare_page",
        "mst2_metadata_install_seal",
        "mst2_metadata_payload",
        "mst2_metadata_current",
        "mst2_metadata_lifetime",
        "mst2_metadata_graph_node",
        "mst2_metadata_graph_edge",
        "mst2_metadata_graph_root",
        "mst2_metadata_gc_op",
        "mst2_retention_node",
        "mst2_retention_edge",
        "mst2_retention_root",
        "mst2_retention_gc_op",
        "mst2_snapshot_context",
        "mst2_snapshot_lease",
    ] {
        let rows = db
            .query_all_raw(statement(
                &format!("SELECT to_jsonb(t)::text FROM {table} t ORDER BY 1"),
                [],
            ))
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.try_get_by_index(0).unwrap())
            .collect();
        state.push((table.into(), rows));
    }
    state
}

#[tokio::test]
async fn install_capability_batch_fault_rolls_back_and_raw_writer_waits_before_row_lock() {
    let (first, second, _schema) = fixture().await;
    let repository = PostgresMetadataInstallRepository::new(first.clone())
        .await
        .unwrap();
    let pages = prepared(2);
    let intent = repository
        .begin_intent("batch-fault", &pages)
        .await
        .unwrap();
    let cap = repository
        .mint_legacy_install_capability(&intent)
        .await
        .unwrap();
    let failed = hex::encode(pages.dag().payloads()[1].id);
    first.execute_unprepared(&format!("CREATE FUNCTION test_install_batch_fault() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN IF NEW.page_id=decode('{failed}','hex') THEN RAISE EXCEPTION 'injected batch fault'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER test_install_batch_fault AFTER INSERT ON mst2_metadata_payload FOR EACH ROW EXECUTE FUNCTION test_install_batch_fault()")).await.unwrap();
    assert!(
        repository
            .install_pages_validated(&cap, pages.dag().payloads())
            .await
            .is_err()
    );
    assert_eq!(
        scalar(&first, "SELECT count(*) FROM mst2_metadata_payload").await,
        0
    );
    first.execute_unprepared("DROP TRIGGER test_install_batch_fault ON mst2_metadata_payload;DROP FUNCTION test_install_batch_fault()").await.unwrap();
    let held = first
        .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
        .await
        .unwrap();
    repository.capability_barrier(&held).await.unwrap();
    let worker = second
        .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
        .await
        .unwrap();
    let pid = scalar(&worker, "SELECT pg_backend_pid()::bigint").await;
    let id = intent.prepare_id().to_owned();
    let blocked = tokio::spawn(async move {
        let result = worker
            .execute_raw(statement(
                "UPDATE mst2_metadata_prepare SET scope='/blocked' WHERE prepare_id=$1",
                [id.into()],
            ))
            .await;
        worker.rollback().await.unwrap();
        result
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    loop {
        let waiting=held.query_one_raw(statement("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND NOT granted
            AND classid=$1::integer::oid AND objid=hashtext(current_schema())::oid
            AND database=(SELECT oid FROM pg_database WHERE datname=current_database())) AS waiting",[RETENTION_LOCK_KEY.into()])).await.unwrap().unwrap().try_get::<bool>("","waiting").unwrap();
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "real writer must wait before frozen-row proof"
        );
        tokio::task::yield_now().await;
    }
    let row_locked=held.query_one_raw(statement("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND locktype='tuple' AND relation='mst2_metadata_prepare'::regclass) AS row_locked",[(pid as i32).into()])).await.unwrap().unwrap().try_get::<bool>("","row_locked").unwrap();
    assert!(
        !row_locked,
        "statement retention barrier precedes prepare row locking"
    );
    held.query_one_raw(statement(
        "SELECT prepare_id FROM mst2_metadata_prepare WHERE prepare_id=$1 FOR UPDATE",
        [intent.prepare_id().into()],
    ))
    .await
    .unwrap()
    .unwrap();
    held.commit().await.unwrap();
    assert!(
        blocked
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("registered metadata")
    );
    install(&repository, &cap, &pages).await;
}

#[path = "native_metadata_install_missing_tests.rs"]
mod missing;
