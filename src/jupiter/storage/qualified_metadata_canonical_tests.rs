use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use super::*;
use crate::ceres::snapshot::{
    rooted_metadata_install::{RootedMetadataInstallPlan, RootedReuseRoot},
    rooted_metadata_projection::RootedReuseLookup,
};

fn encoded_entries(entries: &[Entry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|entry| {
                json!({"kind":1,"name":hex::encode(&entry.name),"size":u64::MAX,
            "content_id":hex::encode([17;32])})
            })
            .collect(),
    )
}

fn files(names: impl IntoIterator<Item = String>) -> Vec<Entry> {
    let mut entries: Vec<_> = names
        .into_iter()
        .map(|name| Entry::file(EntryKind::Regular, name.as_bytes(), u64::MAX, [17; 32]))
        .collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

#[tokio::test]
async fn database_canonical_builder_matches_pinned_codec_at_split_and_name_boundaries() {
    let (_config, _core, _namespace, q, _guard) = fixture().await;
    let terminal_names =
        std::iter::once("a".to_owned()).chain((0..129).map(|index| format!("a{index:03}")));
    let cases = [
        Vec::new(),
        files((0..128).map(|index| format!("f{index:03}"))),
        files((0..129).map(|index| format!("f{index:03}"))),
        files((0..128).map(|index| format!("{}{index:03}", "x".repeat(197)))),
        files(terminal_names),
        files((0..160).map(|index| format!("目录{index:03}"))),
    ];
    for entries in cases {
        let bytes = Page::build(&entries).unwrap();
        let proof: Value = q
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT mst2_metadata_build_map($1::jsonb) AS proof",
                [encoded_entries(&entries).to_string().into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "proof")
            .unwrap();
        assert_eq!(proof["bytes"].as_str().unwrap(), hex::encode(&bytes));
        assert_eq!(
            proof["page_id"].as_str().unwrap(),
            hex::encode(page_id(&bytes))
        );
        let decoded: Value = q
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT mst2_metadata_decode_local($1) AS decoded",
                [bytes.into()],
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "decoded")
            .unwrap();
        assert_eq!(decoded["count"].as_u64().unwrap(), entries.len() as u64);
    }
}

#[tokio::test]
async fn database_parser_rejects_nonexact_bytes_and_invalid_names() {
    let (_config, _core, _namespace, q, _guard) = fixture().await;
    let bytes = Page::build(&files(["file".to_owned()])).unwrap();
    let mut trailing = bytes.clone();
    trailing.push(0);
    let mut wrong_length = bytes.clone();
    wrong_length[16] = wrong_length[16].wrapping_add(1);
    let mut flags = bytes.clone();
    flags[5] = 1;
    let mut wrong_count = bytes.clone();
    wrong_count[8] = 2;
    let mut invalid_utf8 = bytes.clone();
    invalid_utf8[23] = 255;
    let mut slash = bytes.clone();
    slash[23] = b'/';
    for malformed in [
        trailing,
        wrong_length,
        flags,
        wrong_count,
        invalid_utf8,
        slash,
    ] {
        assert!(
            q.query_one_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT mst2_metadata_decode_local($1)",
                [malformed.into()],
            ))
            .await
            .is_err()
        );
    }
    let mut entries = encoded_entries(&files(["one".to_owned(), "two".to_owned()]));
    entries.as_array_mut().unwrap().reverse();
    assert!(
        q.query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT mst2_metadata_build_map($1::jsonb)",
            [entries.to_string().into()],
        ))
        .await
        .is_err()
    );
}

#[tokio::test]
async fn exact_typed_certificates_keep_shared_directory_occurrences_and_dedup_physical_edges() {
    let (config, core, _namespace, q, _guard) = fixture().await;
    let writer = ShadowQualifiedMetadataWriter::open(&core, &config)
        .await
        .unwrap();
    let pages = prepared();
    let receipt = write(&writer, "certified-shared-directory", &pages).await;
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_page_certificate").await,
        2
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_verified_ref WHERE reference_kind='DIRECTORY'"
        )
        .await,
        2
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_graph_edge").await,
        1
    );
    assert_eq!(
        count(
            &q,
            "SELECT max(rank)::bigint FROM mst2_metadata_page_certificate"
        )
        .await,
        1
    );
    assert_eq!(writer.finalize(receipt.intent()).await.unwrap(), receipt);
    for table in [
        "mst2_metadata_page_certificate",
        "mst2_metadata_verified_ref",
        "mst2_metadata_source_root_attestation",
        "mst2_metadata_prepare_reuse_root",
        "mst2_metadata_reuse_index",
        "mst2_metadata_root_anchor",
        "mst2_metadata_reader_operation",
    ] {
        assert!(
            q.execute_unprepared(&format!("TRUNCATE {table} CASCADE"))
                .await
                .is_err()
        );
    }
    for sql in [
        "UPDATE mst2_metadata_page_certificate SET rank=rank+1",
        "DELETE FROM mst2_metadata_verified_ref",
        "UPDATE mst2_metadata_verified_ref SET reference_ordinal=reference_ordinal+1",
    ] {
        assert!(q.execute_unprepared(sql).await.is_err());
    }
}

#[tokio::test]
async fn raw_sql_cannot_certify_a_leafable_branch_from_valid_certified_children() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let writer = ShadowQualifiedMetadataWriter::open(&core, &config)
        .await
        .unwrap();
    let receipt = write(&writer, "raw-canonical-oracle", &prepared()).await;
    let child = Page::build(&[Entry::file(EntryKind::Regular, b"file", 3, [42; 32])]).unwrap();
    let mut payload = vec![1, 0, b'f', 1, 1, 1, 0, b'f'];
    payload.extend_from_slice(&3_u64.to_le_bytes());
    payload.extend_from_slice(&[17; 32]);
    payload.push(b'i');
    payload.extend_from_slice(&1_u64.to_le_bytes());
    payload.extend_from_slice(&page_id(&child));
    let mut branch = b"MTP2\x01\x00".to_vec();
    branch.extend_from_slice(&1_u16.to_le_bytes());
    branch.extend_from_slice(&2_u64.to_le_bytes());
    branch.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    branch.extend_from_slice(&payload);
    let root = page_id(&branch);
    let pid = uuid::Uuid::new_v4().to_string();
    let txn = q
        .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
        .await
        .unwrap();
    namespace.enter(&txn).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_prepare SELECT (jsonb_populate_record(NULL::mst2_metadata_prepare,
         to_jsonb(q)||jsonb_build_object('prepare_id',$1::text,'operation_id','raw-leafable-branch','metadata_root',$2::bytea,
           'state','PREPARING','committed_at',NULL,'node_count',2,'edge_count',1,'total_bytes',$3::bigint))).*
         FROM mst2_metadata_prepare q WHERE q.prepare_id=$4",
        [pid.clone().into(),root.to_vec().into(),((branch.len()+child.len()) as i64).into(),receipt.intent().prepare_id().into()],
    )).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_lifetime(page_id,node_id,generation,state,metadata_codec,expected_size,graph_domain)
         VALUES($1,'page:sha256:'||encode($1,'hex'),1,'RESERVED',1,$2,'qualified-v1')",
        [root.to_vec().into(),(branch.len() as i32).into()],
    )).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO mst2_metadata_current VALUES($1,1)",
        [root.to_vec().into()],
    ))
    .await
    .unwrap();
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO mst2_metadata_prepare_page SELECT $1,$2,1,$3
         UNION ALL SELECT $1,page_id,generation,expected_size
           FROM mst2_metadata_prepare_page WHERE prepare_id=$4 AND page_id=$5",
        [
            pid.clone().into(),
            root.to_vec().into(),
            (branch.len() as i32).into(),
            receipt.intent().prepare_id().into(),
            page_id(&child).to_vec().into(),
        ],
    ))
    .await
    .unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_payload(page_id,generation,metadata_codec,byte_size,payload) VALUES($1,1,1,$2,$3)",
        [root.to_vec().into(),(branch.len() as i32).into(),branch.into()],
    )).await.unwrap();
    let error = txn
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT mst2_metadata_certify_page($1,$2,1)",
            [pid.into(), root.to_vec().into()],
        ))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("leafable"), "{error}");
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn temporary_root_cannot_disappear_in_a_later_transaction() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let writer = ShadowQualifiedMetadataWriter::open(&core, &config)
        .await
        .unwrap();
    let receipt = write(&writer, "continuous-owned-root", &prepared()).await;
    let txn = q
        .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
        .await
        .unwrap();
    namespace.enter(&txn).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_root_anchor(anchor_id,anchor_kind,owner_key,root_page,root_generation,root_certificate_digest,prepare_id)
         SELECT $1::uuid,'PREPARE',$2,c.page_id,c.generation,c.certificate_digest,$2
           FROM mst2_metadata_page_certificate c JOIN mst2_metadata_prepare q ON q.metadata_root=c.page_id
           WHERE q.prepare_id=$2",
        [uuid::Uuid::new_v4().to_string().into(),receipt.intent().prepare_id().into()],
    )).await.unwrap();
    txn.commit().await.unwrap();
    let error = q
        .execute_unprepared("DELETE FROM mst2_metadata_root_anchor WHERE anchor_kind='PREPARE'")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("continuously owned canonical root"),
        "{error}"
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_root_anchor").await,
        1
    );
}

#[tokio::test]
async fn source_root_is_derived_from_current_core_body_and_verified_blob_facts() {
    let (config, core, namespace, q, _guard) = fixture().await;
    let entries = [Entry::file(EntryKind::Regular, b"file", 3, [17; 32])];
    let page = Page::build(&entries).unwrap();
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&page, &entries).unwrap();
    let prepared = PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page_id(&page)).unwrap()),
        "/",
    );
    let mut body = b"100644 file\0".to_vec();
    body.extend_from_slice(&[187; 20]);
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO mega_tree(id,tree_id,sub_trees,size,created_at,pack_id,pack_offset,commit_id)
         VALUES(1,$1,$2,0,now(),'fixture',0,'fixture')",
        ["a".repeat(40).into(), body.clone().into()],
    ))
    .await
    .unwrap();
    core.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_verified_object(storage_domain,git_oid,object_kind,raw_sha256,size,verification_version,state,created_at)
         VALUES('git',$1,'blob',$2,3,2,'VERIFIED',now())",
        ["b".repeat(40).into(),vec![17_u8;32].into()],
    )).await.unwrap();
    let writer = ShadowQualifiedMetadataWriter::open(&core, &config)
        .await
        .unwrap();
    let receipt = write(&writer, "bound-real-core-source", &prepared).await;
    let aid = uuid::Uuid::new_v4().to_string();
    let txn = q
        .begin_with_config(Some(IsolationLevel::ReadCommitted), None)
        .await
        .unwrap();
    namespace.enter(&txn).await.unwrap();
    txn.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_source_root_attestation(attestation_id,namespace_uuid,origin_prepare_id,
         tagged_tree_oid,source_profile,profile_digest,source_body_digest,root_page,root_generation,
         root_certificate_digest,source_proof,attestation_digest)
         SELECT $1::uuid,(proof->>'namespace')::uuid,$2,$3,proof->'source_profile',decode(proof->>'profile_digest','hex'),
           decode(proof->>'source_body_digest','hex'),$4,1,decode(proof->>'root_certificate','hex'),proof,
           decode(proof->>'attestation','hex') FROM (SELECT mst2_metadata_compute_source_proof($2,$3,$4,1) AS proof) input",
        [aid.into(),receipt.intent().prepare_id().into(),prepared.fixed_root_tree_oid().into(),page_id(&page).to_vec().into()],
    )).await.unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_source_root_attestation"
        )
        .await,
        1
    );
    body[7] = b'F';
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mega_tree SET sub_trees=$1 WHERE tree_id=$2",
        [body.into(), "a".repeat(40).into()],
    ))
    .await
    .unwrap();
    let error = q
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT mst2_metadata_compute_source_proof($1,$2,$3,1)",
            [
                receipt.intent().prepare_id().into(),
                prepared.fixed_root_tree_oid().into(),
                page_id(&page).to_vec().into(),
            ],
        ))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("independently canonical certified root"),
        "{error}"
    );
}

pub(super) async fn seeded_rooted_plan(
    core: &DatabaseConnection,
    tree_char: char,
    name: &str,
    id: i64,
) -> (RootedMetadataInstallPlan, MetadataPagePayload) {
    let entries = [Entry::file(
        EntryKind::Regular,
        name.as_bytes(),
        3,
        [17; 32],
    )];
    let bytes = Page::build(&entries).unwrap();
    let page = page_id(&bytes);
    let mut builder = MetadataDagBuilder::new(MetadataDagLimits::default());
    builder.add_directory(&bytes, &entries).unwrap();
    let prepared = PreparedNativeMetadataRetention::test_installation(
        Arc::new(builder.finish(page).unwrap()),
        "/",
    );
    let mut identity = prepared.install_plan().unwrap().identity;
    identity.tagged_root_tree_oid = format!("sha1:{}", tree_char.to_string().repeat(40));
    let mut body = format!("100644 {name}\0").into_bytes();
    body.extend_from_slice(&[187; 20]);
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO mega_tree(id,tree_id,sub_trees,size,created_at,pack_id,pack_offset,commit_id)
         VALUES($1,$2,$3,0,now(),'fixture',0,'fixture')",
        [
            id.into(),
            tree_char.to_string().repeat(40).into(),
            body.into(),
        ],
    ))
    .await
    .unwrap();
    core.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_verified_object(storage_domain,git_oid,object_kind,raw_sha256,size,verification_version,state,created_at)
         VALUES('git',$1,'blob',$2,3,2,'VERIFIED',now()) ON CONFLICT(storage_domain,git_oid,object_kind) DO NOTHING",
        ["b".repeat(40).into(),vec![17_u8;32].into()],
    )).await.unwrap();
    let source_roots = BTreeMap::from([(identity.tagged_root_tree_oid.clone(), page)]);
    let plan = RootedMetadataInstallPlan::new(
        identity,
        page,
        BTreeMap::from([(page, bytes.len() as u64)]),
        BTreeSet::new(),
        BTreeMap::new(),
        source_roots,
    )
    .unwrap();
    (
        plan,
        MetadataPagePayload {
            id: page,
            size: bytes.len() as u64,
            bytes,
        },
    )
}

#[tokio::test]
async fn actual_rooted_cold_and_zero_delta_reuse_keep_one_canonical_graph() {
    let (config, core, _namespace, q, _guard) = fixture().await;
    let (cold, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let intent = writer
        .begin_intent("actual-rooted-cold", &cold)
        .await
        .unwrap();
    writer.install_pages(&intent, &[payload]).await.unwrap();
    let receipt = writer.finalize(&intent).await.unwrap();
    assert_eq!(receipt.metadata_root(), cold.root);
    assert_eq!(
        writer.recover("actual-rooted-cold", &cold).await.unwrap(),
        Some(receipt.clone())
    );
    let proof=q.query_one_raw(Statement::from_string(DbBackend::Postgres,
        "SELECT a.attestation_id::text,a.attestation_digest,a.root_certificate_digest FROM mst2_metadata_source_root_attestation a"
    )).await.unwrap().unwrap();
    let reused = RootedReuseRoot {
        generation: intent.root_generation(),
        attestation_id: uuid::Uuid::parse_str(
            &proof.try_get::<String>("", "attestation_id").unwrap(),
        )
        .unwrap(),
        attestation_digest: proof
            .try_get::<Vec<u8>>("", "attestation_digest")
            .unwrap()
            .try_into()
            .unwrap(),
        certificate_digest: proof
            .try_get::<Vec<u8>>("", "root_certificate_digest")
            .unwrap()
            .try_into()
            .unwrap(),
    };
    let warm = RootedMetadataInstallPlan::new(
        cold.identity.clone(),
        cold.root,
        BTreeMap::new(),
        BTreeSet::new(),
        BTreeMap::from([(cold.root, reused)]),
        cold.source_roots.clone(),
    )
    .unwrap();
    let warm_intent = writer
        .begin_intent("actual-zero-delta-root", &warm)
        .await
        .unwrap();
    assert_eq!(
        writer.finalize(&warm_intent).await.unwrap().metadata_root(),
        cold.root
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_payload").await,
        1
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_page_certificate").await,
        1
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_source_root_attestation"
        )
        .await,
        1
    );
    assert_eq!(count(&q,"SELECT count(*) FROM mst2_metadata_prepare WHERE plan_kind='ROOTED' AND state='COMMITTED' AND node_count=0").await,1);
    let error = q
        .execute_unprepared("DELETE FROM mst2_metadata_root_anchor WHERE anchor_kind='REUSE'")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("continuously owned canonical root"),
        "{error}"
    );
}

#[tokio::test]
async fn changed_ancestor_installs_only_delta_and_accepts_independently_attested_source_aliases() {
    let (config, core, _namespace, q, _guard) = fixture().await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let (first, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let first_intent = writer.begin_intent("alias-first", &first).await.unwrap();
    writer
        .install_pages(&first_intent, &[payload])
        .await
        .unwrap();
    writer.finalize(&first_intent).await.unwrap();
    let (alias, payload) = seeded_rooted_plan(&core, 'c', "file", 2).await;
    assert_eq!(alias.root, first.root);
    let alias_intent = writer.begin_intent("alias-second", &alias).await.unwrap();
    writer
        .install_pages(&alias_intent, &[payload])
        .await
        .unwrap();
    writer.finalize(&alias_intent).await.unwrap();
    let first_hint = writer
        .lookup_reuse(&first.identity.tagged_root_tree_oid, &first.identity)
        .await
        .unwrap()
        .unwrap();
    let alias_hint = writer
        .lookup_reuse(&alias.identity.tagged_root_tree_oid, &alias.identity)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_hint.page_id, alias_hint.page_id);
    assert_eq!(
        first_hint.proof.certificate_digest,
        alias_hint.proof.certificate_digest
    );
    assert_ne!(
        first_hint.proof.attestation_id,
        alias_hint.proof.attestation_id
    );

    let entries = [
        Entry::dir(b"one", first.root),
        Entry::dir(b"two", first.root),
    ];
    let bytes = Page::build(&entries).unwrap();
    let root = page_id(&bytes);
    let mut body = b"40000 one\0".to_vec();
    body.extend_from_slice(&[0xaa; 20]);
    body.extend_from_slice(b"40000 two\0");
    body.extend_from_slice(&[0xcc; 20]);
    core.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO mega_tree(id,tree_id,sub_trees,size,created_at,pack_id,pack_offset,commit_id)
         VALUES(3,$1,$2,0,now(),'fixture',0,'fixture')",
        ["d".repeat(40).into(), body.into()],
    ))
    .await
    .unwrap();
    let mut identity = first.identity.clone();
    identity.tagged_root_tree_oid = format!("sha1:{}", "d".repeat(40));
    let plan = RootedMetadataInstallPlan::new(
        identity.clone(),
        root,
        BTreeMap::from([(root, bytes.len() as u64)]),
        BTreeSet::from([(root, first.root)]),
        BTreeMap::from([(first.root, first_hint.proof)]),
        BTreeMap::from([
            (identity.tagged_root_tree_oid, root),
            (first.identity.tagged_root_tree_oid.clone(), first.root),
            (alias.identity.tagged_root_tree_oid.clone(), first.root),
        ]),
    )
    .unwrap();
    let intent = writer
        .begin_intent("changed-ancestor-aliases", &plan)
        .await
        .unwrap();
    writer
        .install_pages(
            &intent,
            &[MetadataPagePayload {
                id: root,
                size: bytes.len() as u64,
                bytes,
            }],
        )
        .await
        .unwrap();
    let receipt = writer.finalize(&intent).await.unwrap();
    assert_eq!(receipt.metadata_root(), root);
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_payload").await,
        2
    );
    assert_eq!(
        count(&q, "SELECT count(*) FROM mst2_metadata_graph_edge").await,
        1
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_verified_ref WHERE reference_kind='DIRECTORY'"
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &q,
            "SELECT count(*) FROM mst2_metadata_source_root_attestation"
        )
        .await,
        3
    );
    let row = q.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "SELECT node_count,edge_count,(SELECT count(*) FROM mst2_metadata_prepare_reuse_root r
          WHERE r.prepare_id=q.prepare_id)::bigint AS reuse_count FROM mst2_metadata_prepare q WHERE prepare_id=$1",
        [intent.prepare_id().into()])).await.unwrap().unwrap();
    assert_eq!(row.try_get::<i32>("", "node_count").unwrap(), 1);
    assert_eq!(row.try_get::<i32>("", "edge_count").unwrap(), 1);
    assert_eq!(row.try_get::<i64>("", "reuse_count").unwrap(), 1);
    assert_eq!(
        writer
            .recover("changed-ancestor-aliases", &plan)
            .await
            .unwrap(),
        Some(receipt)
    );
}

#[tokio::test]
async fn late_raw_delta_membership_is_rejected_after_intent_commit() {
    let (config, core, _namespace, q, _guard) = fixture().await;
    let (plan, payload) = seeded_rooted_plan(&core, 'a', "file", 1).await;
    let (donor, donor_payload) = seeded_rooted_plan(&core, 'c', "other", 2).await;
    let writer = RootedQualifiedMetadataRepository::open(&core, &config)
        .await
        .unwrap();
    let donor_intent = writer
        .begin_intent("rooted-extra-donor", &donor)
        .await
        .unwrap();
    writer
        .install_pages(&donor_intent, std::slice::from_ref(&donor_payload))
        .await
        .unwrap();
    writer.finalize(&donor_intent).await.unwrap();
    let intent = writer
        .begin_intent("rooted-bounded-intent", &plan)
        .await
        .unwrap();
    writer.install_pages(&intent, &[payload]).await.unwrap();
    let error=q.execute_raw(Statement::from_sql_and_values(DbBackend::Postgres,
        "INSERT INTO mst2_metadata_prepare_page(prepare_id,page_id,generation,expected_size) VALUES($1,$2,1,$3)",
        [intent.prepare_id().into(),donor.root.to_vec().into(),(donor_payload.size as i32).into()],
    )).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing or extra exact delta/reuse"),
        "{error}"
    );
    assert_eq!(
        writer.finalize(&intent).await.unwrap().metadata_root(),
        plan.root
    );
}
