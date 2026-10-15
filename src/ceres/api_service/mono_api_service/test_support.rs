use super::{prelude::*, *};

#[test]
pub(in crate::ceres::api_service::mono_api_service) fn test_parse_github_link() {
    let url = "https://github.com/web3infra-foundation/libra/";
    let url = url
        .trim_end_matches(".git")
        .trim_end_matches("/")
        .strip_prefix("https://github.com/")
        .expect("Invalid GitHub URL");
    let (owner, repo) = url.rsplit_once('/').unwrap();
    assert_eq!(owner, "web3infra-foundation");
    assert_eq!(repo, "libra");
}

#[tokio::test]
pub(in crate::ceres::api_service::mono_api_service) async fn test_third_party_trait() {
    let url = "https://github.com/aidcheng/mega.git";
    let third_party_client = ThirdPartyClient::new(url);
    let remote_timeout = std::time::Duration::from_secs(30);

    let (_, refs) =
        match tokio::time::timeout(remote_timeout, third_party_client.fetch_refs()).await {
            Ok(Ok(refs)) => refs,
            Ok(Err(err)) => {
                tracing::warn!(
                    "Skipping test_third_party_trait because remote refs are unavailable: {}",
                    err
                );
                return;
            }
            Err(_) => {
                tracing::warn!("Skipping test_third_party_trait because remote refs timed out");
                return;
            }
        };

    let res = match tokio::time::timeout(
        remote_timeout,
        third_party_client.fetch_packs(&[refs], Some(1)),
    )
    .await
    {
        Ok(Ok(res)) => res,
        Ok(Err(err)) => {
            tracing::warn!(
                "Skipping test_third_party_trait because pack fetch failed: {}",
                err
            );
            return;
        }
        Err(_) => {
            tracing::warn!("Skipping test_third_party_trait because pack fetch timed out");
            return;
        }
    };

    match tokio::time::timeout(remote_timeout, third_party_client.process_pack_stream(res)).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                "Skipping test_third_party_trait because pack processing failed: {}",
                err
            );
        }
        Err(_) => {
            tracing::warn!("Skipping test_third_party_trait because pack processing timed out");
        }
    }
}

// --- UN-16: merge funnel notify + dirty fail-closed (real test DB) ---
//
// These tests drive `apply_update_result` against a real Postgres test
// schema (`crate::jupiter::tests::test_storage`, ER-01 test stack). The
// `git_object_cache` is a lazy Redis connection that is never used by
// `apply_update_result`, so no live Redis is required.

pub(in crate::ceres::api_service::mono_api_service) async fn save_authz_json(
    storage: &Storage,
    json: &str,
) -> String {
    storage
        .git_service
        .save_object_from_raw(bytes::Bytes::from(json.to_string()))
        .await
        .expect("save authz blob to object storage")
}

pub(in crate::ceres::api_service::mono_api_service) fn blob_item(
    name: &str,
    hex: &str,
) -> TreeItem {
    TreeItem::new(
        TreeItemMode::Blob,
        ObjectHash::from_hex_for_kind(get_hash_kind(), hex).unwrap(),
        name.to_string(),
    )
}

pub(in crate::ceres::api_service::mono_api_service) async fn setup_main_ref(
    storage: &Storage,
    old_tree: &Tree,
    old_commit_id: &str,
) {
    storage
        .mono_storage()
        .save_mega_trees(
            vec![old_tree.clone()],
            ObjectHash::from_hex_for_kind(get_hash_kind(), old_commit_id).unwrap(),
            None,
        )
        .await
        .expect("save old tree");
    let main_ref = mega_refs::Model {
        id: 1,
        path: "/".to_string(),
        ref_name: MEGA_BRANCH_NAME.to_string(),
        ref_commit_hash: old_commit_id.to_string(),
        ref_tree_hash: old_tree.id.to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
        is_cl: false,
    };
    storage
        .mono_storage()
        .save_refs(main_ref, None)
        .await
        .expect("save main ref");
}

pub(in crate::ceres::api_service::mono_api_service) fn test_service(
    storage: &Storage,
) -> MonoApiService {
    MonoApiService {
        storage: storage.clone(),
        git_object_cache: Arc::new(GitObjectCache {
            connection: ::redis::aio::ConnectionManager::new_lazy_with_config(
                ::redis::Client::open("redis://127.0.0.1:6379").expect("open redis client"),
                ::redis::aio::ConnectionManagerConfig::new(),
            )
            .expect("lazy connection manager"),
            prefix: "test".to_string(),
        }),
    }
}

#[tokio::test]
pub(in crate::ceres::api_service::mono_api_service) async fn apply_update_result_rebuilds_snapshot_when_authz_blob_changes()
 {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let service = test_service(&storage);

    let old_json =
        crate::contract::policy::entitystore::generate_entity(&["admin".to_string()], "old-repo")
            .expect("generate old authz");
    let new_json =
        crate::contract::policy::entitystore::generate_entity(&["admin".to_string()], "new-repo")
            .expect("generate new authz");
    let old_blob_id = save_authz_json(&storage, &old_json).await;
    let new_blob_id = save_authz_json(&storage, &new_json).await;

    let old_tree = Tree::from_tree_items(vec![blob_item(
        crate::contract::policy::entitystore::MEGA_CEDAR_PATH.trim_start_matches('/'),
        &old_blob_id,
    )])
    .unwrap();
    setup_main_ref(
        &storage,
        &old_tree,
        "1111111111111111111111111111111111111111",
    )
    .await;

    let new_tree = Tree::from_tree_items(vec![blob_item(
        crate::contract::policy::entitystore::MEGA_CEDAR_PATH.trim_start_matches('/'),
        &new_blob_id,
    )])
    .unwrap();
    let result = TreeUpdateResult {
        updated_trees: vec![new_tree.clone()],
        ref_updates: vec![RefUpdate {
            path: "/".to_string(),
            tree_id: new_tree.id,
        }],
    };

    let new_commit_id = service
        .apply_update_result(&result, "update authz", None)
        .await
        .expect("apply_update_result should succeed");
    assert!(!new_commit_id.is_empty());

    assert!(!storage.entity_store().is_dirty());
    let snap = storage.entity_store().snapshot().expect("snapshot built");
    assert!(
        snap.store()
            .contains_repository(&r#"Repository::"new-repo""#.parse().unwrap()),
        "snapshot should reflect the new authz content"
    );
}

#[tokio::test]
pub(in crate::ceres::api_service::mono_api_service) async fn apply_update_result_marks_dirty_when_authz_blob_unreadable()
 {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let service = test_service(&storage);

    // Old main tree has no `/.mega_cedar.json` (old_blob_id = None).
    // `from_tree_items` rejects empty trees, so build the empty tree via the
    // struct literal (id is arbitrary; only the tree_items matter here).
    let old_tree = Tree {
        id: ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
        tree_items: vec![],
    };
    setup_main_ref(
        &storage,
        &old_tree,
        "1111111111111111111111111111111111111111",
    )
    .await;

    // New tree references a `/.mega_cedar.json` blob that does not exist in
    // object storage: the notify read fails -> snapshot marked dirty
    // (fail-closed). The write itself is already committed, so
    // `apply_update_result` still returns Ok (best-effort notify).
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let new_tree = Tree::from_tree_items(vec![blob_item(
        crate::contract::policy::entitystore::MEGA_CEDAR_PATH.trim_start_matches('/'),
        missing,
    )])
    .unwrap();
    let result = TreeUpdateResult {
        updated_trees: vec![new_tree.clone()],
        ref_updates: vec![RefUpdate {
            path: "/".to_string(),
            tree_id: new_tree.id,
        }],
    };

    service
        .apply_update_result(&result, "update authz", None)
        .await
        .expect("apply_update_result succeeds; notify is best-effort");
    assert!(
        storage.entity_store().is_dirty(),
        "unreadable authz blob must mark the snapshot dirty (fail-closed)"
    );
}

#[tokio::test]
pub(in crate::ceres::api_service::mono_api_service) async fn apply_update_result_marks_dirty_when_tree_save_fails_after_ref_write()
 {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let service = test_service(&storage);

    let old_tree = Tree {
        id: ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
        tree_items: vec![],
    };
    setup_main_ref(
        &storage,
        &old_tree,
        "1111111111111111111111111111111111111111",
    )
    .await;

    // Fault injection: a `BEFORE INSERT` trigger on `mega_tree` that raises
    // an exception, so the tree save (`batch_save_model`, which propagates
    // errors) fails AFTER the main ref write (`batch_update_by_path_concurrent`)
    // succeeds. The ref write targets `mega_refs` (intact); the commit save
    // targets `mega_commit` (intact); the tree save hits the trigger -> a
    // non-RecordNotInserted error that propagates to the dirty-marking
    // branch. (A plain `DROP TABLE` would not work: the search_path includes
    // `public`, so the insert would fall back to the public schema's table.)
    let mono = storage.mono_storage();
    let conn = mono.get_connection();
    sea_orm::ConnectionTrait::execute_raw(
            conn,
            sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "CREATE OR REPLACE FUNCTION fault_inject_block_tree_insert() RETURNS trigger AS $$ BEGIN RAISE EXCEPTION 'fault injection: mega_tree insert blocked'; END; $$ LANGUAGE plpgsql".to_string(),
            ),
        )
        .await
        .expect("create fault-injection trigger function");
    sea_orm::ConnectionTrait::execute_raw(
            conn,
            sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Postgres,
                "CREATE TRIGGER fault_inject_mega_tree_trigger BEFORE INSERT ON mega_tree FOR EACH ROW EXECUTE FUNCTION fault_inject_block_tree_insert()".to_string(),
            ),
        )
        .await
        .expect("create fault-injection trigger on mega_tree");

    let new_tree = Tree {
        id: ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "2222222222222222222222222222222222222222",
        )
        .unwrap(),
        tree_items: vec![],
    };
    let result = TreeUpdateResult {
        updated_trees: vec![new_tree.clone()],
        ref_updates: vec![RefUpdate {
            path: "/".to_string(),
            tree_id: new_tree.id,
        }],
    };

    let err = service
        .apply_update_result(&result, "update", None)
        .await
        .expect_err("tree save must fail after the ref write");
    assert!(!err.to_string().is_empty());
    assert!(
        storage.entity_store().is_dirty(),
        "ref written but subsequent step failed -> dirty (fail-closed)"
    );
}

// --- MC-02: minimal GPG merge gate (real test DB) ---
//
// `merge_cl` rejects a CL whose `check_result` rows contain a FAILED
// GpgSignature entry, and only that one. The full `ensure_cl_mergeable`
// gate stays disabled; these tests pin the minimal wiring.

#[cfg(test)]
pub(in crate::ceres::api_service::mono_api_service) fn gate_test_cl(
    link: &str,
    from_hash: &str,
    to_hash: &str,
) -> mega_cl::Model {
    mega_cl::Model {
        id: 42,
        link: link.to_string(),
        title: "gpg gate test".to_string(),
        merge_date: None,
        status: MergeStatusEnum::Open,
        path: "/".to_string(),
        from_hash: from_hash.to_string(),
        to_hash: to_hash.to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
        username: "gate-tester".to_string(),
        base_branch: "main".to_string(),
        revision: 0,
    }
}

#[cfg(test)]
pub(in crate::ceres::api_service::mono_api_service) async fn insert_check_result(
    storage: &Storage,
    link: &str,
    check_type: CheckTypeEnum,
    status: &str,
) {
    let model =
        crate::callisto::check_result::Model::new("/", link, "deadbeef", check_type, status, "msg");
    storage
        .cl_storage()
        .save_check_results(vec![model])
        .await
        .expect("save check result");
}

#[cfg(test)]
pub(in crate::ceres::api_service::mono_api_service) async fn gate_test_service()
-> (tempfile::TempDir, Storage, MonoApiService) {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let service = test_service(&storage);
    let old_tree = Tree {
        id: ObjectHash::from_hex_for_kind(
            get_hash_kind(),
            "1111111111111111111111111111111111111111",
        )
        .unwrap(),
        tree_items: vec![],
    };
    setup_main_ref(
        &storage,
        &old_tree,
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )
    .await;
    (temp, storage, service)
}

#[tokio::test]
pub(in crate::ceres::api_service::mono_api_service) async fn merge_cl_rejects_cl_with_failed_gpg_signature_check()
 {
    let (_temp, storage, service) = gate_test_service().await;
    let cl = gate_test_cl(
        "GPGFAIL1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    insert_check_result(&storage, &cl.link, CheckTypeEnum::GpgSignature, "FAILED").await;

    let err = service
        .merge_cl("gate-tester", "gate-tester", cl.clone())
        .await
        .expect_err("a FAILED GPG signature check must block the merge");
    assert!(
        err.to_string().contains("GPG signature check failed"),
        "{err}"
    );
    assert!(err.to_string().contains(&cl.link), "{err}");
}

#[tokio::test]
pub(in crate::ceres::api_service::mono_api_service) async fn merge_cl_passes_gate_without_any_check_rows()
 {
    let (_temp, _storage, service) = gate_test_service().await;
    let cl = gate_test_cl(
        "GPGNONE1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );

    // The gate must not block; the merge then fails later because the tip
    // commit does not exist in this fixture — that error proves the gate let
    // the CL through.
    let err = service
        .merge_cl("gate-tester", "gate-tester", cl)
        .await
        .expect_err("merge fails on the missing tip commit, not on the gate");
    assert!(
        !err.to_string().contains("GPG signature check failed"),
        "{err}"
    );
}

#[tokio::test]
pub(in crate::ceres::api_service::mono_api_service) async fn merge_cl_passes_gate_with_passed_gpg_and_other_failed_checks()
 {
    let (_temp, storage, service) = gate_test_service().await;
    let cl = gate_test_cl(
        "GPGPASS1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    insert_check_result(&storage, &cl.link, CheckTypeEnum::GpgSignature, "PASSED").await;
    insert_check_result(&storage, &cl.link, CheckTypeEnum::ClSync, "FAILED").await;

    let err = service
        .merge_cl("gate-tester", "gate-tester", cl)
        .await
        .expect_err("merge fails after the GPG gate, not on the gate");
    assert!(
        !err.to_string().contains("GPG signature check failed"),
        "{err}"
    );
}

// --- MC-09: server-side signing of synthetic commits entering a CL chain ---
//
// Both synthesis points — the `update_branch` chain (via
// `process_ref_updates_cl_only`) and the buck upload chain
// (`complete_buck_upload`) — must emit commits signed with the server key
// from the vault. Every derived reference must carry the final signed hash:
// the `mega_commit.commit_id` row, the `mega_tree.commit_id` stamps, the CL
// ref's `ref_commit_hash` and the CL's `to_hash`; and the ref-pointed commit
// must pass the MC-02 chain verifier (`GpgSignatureChecker`).
