use std::time::Duration;

use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

use super::{test_support::*, *};
use crate::{
    ceres::merge_checker::{Checker, ConditionResult, gpg_signature_checker::GpgSignatureChecker},
    contract::vault::{integration::vault_core::VaultCore, server_signing::SERVER_SIGNING_EMAIL},
    jupiter::{
        migration::apply_migrations,
        storage::base_storage::{BaseStorage, StorageConnector},
        tests::test_db_connection,
    },
};

async fn test_redis() -> ::redis::aio::ConnectionManager {
    let url =
        std::env::var("MEGA_REDIS__URL").unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string());
    let client = ::redis::Client::open(url).expect("redis client");
    ::redis::aio::ConnectionManager::new(client)
        .await
        .expect("redis connection")
}

/// A standalone test vault (own schema, own key file) for signing.
async fn test_vault(dir: &Path) -> VaultCore {
    let conn = Arc::new(test_db_connection(dir).await);
    apply_migrations(&conn, true).await.expect("migrations");
    VaultCore::config(
        crate::jupiter::storage::vault_storage::VaultStorage {
            base: BaseStorage::new(conn),
        },
        dir.join("mc09_core_key.json"),
    )
    .await
    .expect("vault core should initialize")
}

/// A service whose `git_object_cache` connection is the real test Redis —
/// the RedLock backend for first-time signing-key initialization. The
/// object cache itself is disabled (empty prefix).
fn signing_service(storage: &Storage, redis: ::redis::aio::ConnectionManager) -> MonoApiService {
    MonoApiService {
        storage: storage.clone(),
        git_object_cache: Arc::new(GitObjectCache {
            connection: redis,
            prefix: String::new(),
        }),
    }
}

/// Run the MC-02 chain verifier over `(from, to]` and demand PASSED.
async fn assert_chain_verifies(storage: &Storage, from: &str, to: &str) {
    let checker = GpgSignatureChecker {
        storage: Arc::new(storage.clone()),
    };
    let res = checker
        .run(&serde_json::json!({"cl_from": from, "cl_to": to}))
        .await;
    assert_eq!(
        res.status,
        ConditionResult::PASSED,
        "MC-02 chain verification must pass for the signed commit: {}",
        res.message
    );
}

fn assert_server_commit(commit: &crate::callisto::mega_commit::Model) {
    let content = commit.content.as_deref().unwrap_or_default();
    assert!(
        content.starts_with("gpgsig -----BEGIN PGP SIGNATURE-----"),
        "synthesized commit must embed the gpgsig header: {content}"
    );
    assert!(
        commit
            .author
            .as_deref()
            .unwrap_or_default()
            .contains(SERVER_SIGNING_EMAIL),
        "author must be the reserved server identity"
    );
    assert!(
        commit
            .committer
            .as_deref()
            .unwrap_or_default()
            .contains(SERVER_SIGNING_EMAIL),
        "committer must be the reserved server identity"
    );
}

async fn commit_count(storage: &Storage) -> usize {
    crate::callisto::mega_commit::Entity::find()
        .all(storage.mono_storage().get_connection())
        .await
        .expect("count commits")
        .len()
}

async fn tree_count(storage: &Storage) -> usize {
    crate::callisto::mega_tree::Entity::find()
        .all(storage.mono_storage().get_connection())
        .await
        .expect("count trees")
        .len()
}

/// `update_branch` fixture: three commits — `from` (old CL base),
/// `cl_tip` (CL head adding `cl.txt`) and `target` (current main, adding
/// `other.txt`; disjoint from the CL change so no conflict) — plus the
/// main ref, the CL ref and the open CL.
async fn chain_fixture(storage: &Storage, link: &str) -> (String, String, String) {
    let blob_base =
        ObjectHash::from_hex_for_kind(get_hash_kind(), "1111111111111111111111111111111111111111")
            .unwrap();
    let blob_other =
        ObjectHash::from_hex_for_kind(get_hash_kind(), "2222222222222222222222222222222222222222")
            .unwrap();
    let blob_cl =
        ObjectHash::from_hex_for_kind(get_hash_kind(), "3333333333333333333333333333333333333333")
            .unwrap();

    let from_tree = Tree::from_tree_items(vec![blob_item("base.txt", &blob_base.to_string())])
        .expect("from tree");
    let target_tree = Tree::from_tree_items(vec![
        blob_item("base.txt", &blob_base.to_string()),
        blob_item("other.txt", &blob_other.to_string()),
    ])
    .expect("target tree");
    let cl_tree = Tree::from_tree_items(vec![
        blob_item("base.txt", &blob_base.to_string()),
        blob_item("cl.txt", &blob_cl.to_string()),
    ])
    .expect("cl tree");

    let from_commit = Commit::from_tree_id(from_tree.id, vec![], "mc09 base");
    let target_commit = Commit::from_tree_id(target_tree.id, vec![], "mc09 main advanced");
    let cl_commit = Commit::from_tree_id(cl_tree.id, vec![from_commit.id], "mc09 cl change");

    let mono = storage.mono_storage();
    mono.save_mega_commits(
        vec![
            from_commit.clone(),
            target_commit.clone(),
            cl_commit.clone(),
        ],
        None,
    )
    .await
    .expect("fixture commits");
    mono.save_mega_trees(vec![from_tree.clone()], from_commit.id, None)
        .await
        .expect("from tree");
    mono.save_mega_trees(vec![cl_tree.clone()], cl_commit.id, None)
        .await
        .expect("cl tree");
    // Saves the target tree and the main ref.
    setup_main_ref(storage, &target_tree, &target_commit.id.to_string()).await;

    let now = chrono::Utc::now().naive_utc();
    let cl_ref = mega_refs::Model {
        id: 2,
        path: "/".to_string(),
        ref_name: format!("refs/cl/{link}"),
        ref_commit_hash: cl_commit.id.to_string(),
        ref_tree_hash: cl_tree.id.to_string(),
        created_at: now,
        updated_at: now,
        is_cl: true,
    };
    mono.save_refs(cl_ref, None).await.expect("cl ref");

    storage
        .cl_storage()
        .new_cl(
            "/",
            link,
            "mc09 update-branch",
            "main",
            &from_commit.id.to_string(),
            &cl_commit.id.to_string(),
            "alice",
        )
        .await
        .expect("cl");

    (
        from_commit.id.to_string(),
        cl_commit.id.to_string(),
        target_commit.id.to_string(),
    )
}

// AC⑥ (update_branch chain): the synthesized rebase commit is
// server-signed and all four derived references carry the final signed
// hash; the ref-pointed commit passes the MC-02 chain verifier.
#[tokio::test]
async fn update_branch_signs_commit_and_backfills_references() {
    let temp = tempfile::tempdir().unwrap();
    let vault = test_vault(temp.path()).await;
    let redis = test_redis().await;
    let storage = crate::jupiter::tests::test_storage(temp.path())
        .await
        .with_vault(vault.clone());
    let service = signing_service(&storage, redis.clone());

    let link = "MC09UPD1";
    let (_from_hash, cl_tip, target_head) = chain_fixture(&storage, link).await;

    let new_head = service
        .update_branch("alice", link)
        .await
        .expect("update_branch with signing");
    assert_ne!(new_head, cl_tip, "a new rebased commit must be created");

    let mono = storage.mono_storage();
    let commit = mono
        .get_commit_by_hash(&new_head)
        .await
        .expect("load commit")
        .expect("signed commit row");
    let cl_ref = mono
        .get_ref_by_name(&format!("refs/cl/{link}"))
        .await
        .expect("load cl ref")
        .expect("cl ref row");
    let cl = storage
        .cl_storage()
        .get_cl(link)
        .await
        .expect("load cl")
        .expect("cl row");
    let tree = mono
        .get_tree_by_hash(&cl_ref.ref_tree_hash)
        .await
        .expect("load tree")
        .expect("new root tree row");

    // Four sinks, one final signed hash.
    assert_eq!(commit.commit_id, new_head);
    assert_eq!(tree.commit_id, new_head, "mega_tree.commit_id");
    assert_eq!(cl_ref.ref_commit_hash, new_head, "CL ref");
    assert_eq!(cl.to_hash, new_head, "mega_cl.to_hash");
    assert_eq!(cl.from_hash, target_head, "rebase advances the CL base");
    assert_server_commit(&commit);

    // MC-10 test hygiene: no `gpg_key` registration — the chain must
    // verify through the server keyring routing alone (MC-11).
    assert_chain_verifies(&storage, &target_head, &new_head).await;
}

// MC-04 R2: update_branch's CL hash move and the commit-listing rebuild
// share one transaction — after a rebase the listing is readable (the
// read-side coverage check must not fail closed on the moved range) and
// equals the new `(target_head, new_head]` chain: exactly the synthesized
// commit.
#[tokio::test]
async fn update_branch_rebuilds_cl_commits_listing() {
    let temp = tempfile::tempdir().unwrap();
    let vault = test_vault(temp.path()).await;
    let redis = test_redis().await;
    let storage = crate::jupiter::tests::test_storage(temp.path())
        .await
        .with_vault(vault.clone());
    let service = signing_service(&storage, redis.clone());

    let link = "MC04UB1";
    let (_from_hash, cl_tip, _target_head) = chain_fixture(&storage, link).await;

    // Pre-stage a stale listing (the pre-rebase tip) so the rebuild's
    // delete+insert is exercised.
    let cl_stg = storage.cl_storage();
    let stale = [Commit::from_mega_model(
        storage
            .mono_storage()
            .get_commit_by_hash(&cl_tip)
            .await
            .expect("load cl tip")
            .expect("cl tip row"),
    )];
    let txn = storage.begin_db_transaction().await.expect("begin txn");
    cl_stg
        .save_cl_commits_in_txn(link, &stale, &txn)
        .await
        .expect("stage stale listing");
    txn.commit().await.expect("commit staging txn");

    let new_head = service
        .update_branch("alice", link)
        .await
        .expect("update_branch with signing");

    let listing = cl_stg
        .get_cl_commits(link)
        .await
        .expect("listing must be readable after update_branch");
    assert_eq!(
        listing.len(),
        1,
        "the rebase emits exactly one synthesized commit"
    );
    assert_eq!(
        listing[0].commit_sha, new_head,
        "the listing is the new (target_head, new_head] chain"
    );
    assert_eq!(listing[0].message, "update-branch: rebase");
    assert!(
        listing[0].author_email.contains(SERVER_SIGNING_EMAIL),
        "the listing member is the server-signed synthesized commit"
    );
}

// MC-04 R2 (no-change branch): a no-op rebase synthesizes no commit, so
// the new range has no valid chain — the listing must be cleared with the
// base move (atomically), and reads return an empty list rather than
// failing the coverage check.
#[tokio::test]
async fn update_branch_no_change_clears_cl_commits_listing() {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    // No vault: the no-change branch synthesizes nothing, so no signing
    // capability is needed.
    let service = test_service(&storage);

    let link = "MC04UB2";
    // Fixture like chain_fixture, but the CL commit shares the base tree —
    // the CL diff (from → to) is empty.
    let blob_base =
        ObjectHash::from_hex_for_kind(get_hash_kind(), "1111111111111111111111111111111111111111")
            .unwrap();
    let blob_other =
        ObjectHash::from_hex_for_kind(get_hash_kind(), "2222222222222222222222222222222222222222")
            .unwrap();
    let from_tree = Tree::from_tree_items(vec![blob_item("base.txt", &blob_base.to_string())])
        .expect("from tree");
    let target_tree = Tree::from_tree_items(vec![
        blob_item("base.txt", &blob_base.to_string()),
        blob_item("other.txt", &blob_other.to_string()),
    ])
    .expect("target tree");
    let from_commit = Commit::from_tree_id(from_tree.id, vec![], "mc04 base");
    let cl_commit = Commit::from_tree_id(from_tree.id, vec![from_commit.id], "mc04 cl no-op");
    let target_commit = Commit::from_tree_id(target_tree.id, vec![], "mc04 main advanced");

    let mono = storage.mono_storage();
    mono.save_mega_commits(
        vec![
            from_commit.clone(),
            cl_commit.clone(),
            target_commit.clone(),
        ],
        None,
    )
    .await
    .expect("fixture commits");
    mono.save_mega_trees(vec![from_tree.clone()], from_commit.id, None)
        .await
        .expect("from/cl tree");
    setup_main_ref(&storage, &target_tree, &target_commit.id.to_string()).await;
    let now = chrono::Utc::now().naive_utc();
    mono.save_refs(
        mega_refs::Model {
            id: 2,
            path: "/".to_string(),
            ref_name: format!("refs/cl/{link}"),
            ref_commit_hash: cl_commit.id.to_string(),
            ref_tree_hash: from_tree.id.to_string(),
            created_at: now,
            updated_at: now,
            is_cl: true,
        },
        None,
    )
    .await
    .expect("cl ref");
    storage
        .cl_storage()
        .new_cl(
            "/",
            link,
            "mc04 no-change update-branch",
            "main",
            &from_commit.id.to_string(),
            &cl_commit.id.to_string(),
            "alice",
        )
        .await
        .expect("cl");
    // Pre-stage the pre-rebase listing (the CL tip).
    let cl_stg = storage.cl_storage();
    let txn = storage.begin_db_transaction().await.expect("begin txn");
    cl_stg
        .save_cl_commits_in_txn(link, std::slice::from_ref(&cl_commit), &txn)
        .await
        .expect("stage listing");
    txn.commit().await.expect("commit staging txn");

    let returned = service
        .update_branch("alice", link)
        .await
        .expect("no-change update_branch");

    // MC-04 R3: the empty-range state is self-consistent —
    // from == to == target_head, the CL ref follows, the listing is
    // cleared, and the old tip stays in the object store for audit.
    assert_eq!(
        returned,
        target_commit.id.to_string(),
        "the empty range's head is the target head"
    );
    let cl = cl_stg.get_cl(link).await.expect("get_cl").expect("cl row");
    assert_eq!(cl.from_hash, target_commit.id.to_string(), "base advanced");
    assert_eq!(
        cl.to_hash,
        target_commit.id.to_string(),
        "head follows the base: the empty range is self-consistent"
    );
    let cl_ref = mono
        .get_ref_by_name(&format!("refs/cl/{link}"))
        .await
        .expect("load cl ref")
        .expect("cl ref row");
    assert_eq!(
        cl_ref.ref_commit_hash,
        target_commit.id.to_string(),
        "the active CL ref follows to the target head"
    );
    assert_eq!(
        cl_ref.ref_tree_hash,
        target_tree.id.to_string(),
        "the CL ref tree follows to the target tree"
    );
    let listing = cl_stg
        .get_cl_commits(link)
        .await
        .expect("listing must be readable (empty) after a no-change rebase");
    assert!(
        listing.is_empty(),
        "a no-change rebase clears the listing (the new range has no chain)"
    );
    assert!(
        mono.get_commit_by_hash(&cl_commit.id.to_string())
            .await
            .expect("lookup old tip")
            .is_some(),
        "the old tip commit stays in the object store for audit"
    );
}

// MC-04 R3 fault injection: if the commit-listing write fails inside
// update_branch's single transaction, every sink stays untouched — the CL
// ref, commit rows, tree rows and the CL row all keep the old range. The
// transaction body below mirrors `update_branch`'s statement sequence.
#[tokio::test]
async fn update_branch_listing_failure_leaves_all_sinks_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let vault = test_vault(temp.path()).await;
    let redis = test_redis().await;
    let storage = crate::jupiter::tests::test_storage(temp.path())
        .await
        .with_vault(vault.clone());
    let service = signing_service(&storage, redis.clone());

    let link = "MC04UB3";
    let (from_hash, cl_tip, target_head) = chain_fixture(&storage, link).await;

    let cl = storage
        .cl_storage()
        .get_cl(link)
        .await
        .expect("get_cl")
        .expect("cl row");
    let old_blobs = service
        .get_commit_blobs(&cl.from_hash)
        .await
        .expect("old blobs");
    let new_blobs = service
        .get_commit_blobs(&cl.to_hash)
        .await
        .expect("new blobs");
    let changes = service
        .cl_files_list(old_blobs, new_blobs)
        .await
        .expect("cl changes");
    assert!(!changes.is_empty(), "the fixture CL has a real change");

    let prepared = service
        .build_signed_cl_update(&cl, &changes, &target_head)
        .await
        .expect("construct + sign the update");

    // Mirror update_branch's single-transaction persistence body, with a
    // poisoned listing batch (duplicate (link, sha) primary key) — the
    // write must fail and roll everything back.
    let mono = storage.mono_storage();
    let mut cl_ref = prepared.cl_ref.clone();
    for update in &prepared.updates {
        if update.ref_name == cl_ref.ref_name {
            cl_ref.ref_commit_hash = update.commit_id.clone();
            cl_ref.ref_tree_hash = update.tree_hash.clone();
        }
    }
    let poisoned: Vec<Commit> = vec![prepared.commits[0].clone(), prepared.commits[0].clone()];
    let txn = storage.begin_db_transaction().await.expect("begin txn");
    mono.update_ref(cl_ref, Some(&txn))
        .await
        .expect("ref update in txn");
    mono.save_mega_commits(prepared.commits.clone(), Some(&txn))
        .await
        .expect("commits in txn");
    mono.batch_save_model_with_txn(prepared.tree_models.clone(), Some(&txn))
        .await
        .expect("trees in txn");
    storage
        .cl_storage()
        .update_cl_hash_in_txn(cl.clone(), &target_head, &prepared.new_commit_id, &txn)
        .await
        .expect("cl hashes in txn");
    storage
        .cl_storage()
        .save_cl_commits_in_txn(link, &poisoned, &txn)
        .await
        .expect_err("the poisoned listing write must fail");
    drop(txn); // rolls back

    // All four sinks unchanged.
    let cl_ref_after = mono
        .get_ref_by_name(&format!("refs/cl/{link}"))
        .await
        .expect("load cl ref")
        .expect("cl ref row");
    assert_eq!(cl_ref_after.ref_commit_hash, cl_tip, "CL ref unchanged");
    assert_eq!(commit_count(&storage).await, 3, "no new commit persisted");
    assert_eq!(tree_count(&storage).await, 3, "no new tree persisted");
    let cl_after = storage
        .cl_storage()
        .get_cl(link)
        .await
        .expect("get_cl")
        .expect("cl row");
    assert_eq!(cl_after.from_hash, from_hash, "CL from_hash unchanged");
    assert_eq!(cl_after.to_hash, cl_tip, "CL to_hash unchanged");
    assert!(
        storage
            .cl_storage()
            .get_cl_commits(link)
            .await
            .expect("listing read")
            .is_empty(),
        "no listing rows persisted"
    );
}

// AC③/AC④ (update_branch chain): without the vault handle the synthesis
// fails closed and nothing is persisted — ref, commits, trees and CL
// hashes all unchanged.
#[tokio::test]
async fn update_branch_without_vault_fails_closed_with_zero_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    // `test_service`'s lazy Redis connection is never reached: the
    // fail-closed vault check happens first.
    let service = test_service(&storage);

    let link = "MC09UPD2";
    let (from_hash, cl_tip, _target_head) = chain_fixture(&storage, link).await;

    let err = service
        .update_branch("alice", link)
        .await
        .expect_err("update_branch must fail closed without a vault");
    assert!(
        err.to_string().contains("server signing unavailable"),
        "{err}"
    );

    let mono = storage.mono_storage();
    let cl = storage
        .cl_storage()
        .get_cl(link)
        .await
        .expect("load cl")
        .expect("cl row");
    assert_eq!(cl.from_hash, from_hash, "CL from_hash unchanged");
    assert_eq!(cl.to_hash, cl_tip, "CL to_hash unchanged");
    let cl_ref = mono
        .get_ref_by_name(&format!("refs/cl/{link}"))
        .await
        .expect("load cl ref")
        .expect("cl ref row");
    assert_eq!(cl_ref.ref_commit_hash, cl_tip, "CL ref unchanged");
    assert_eq!(commit_count(&storage).await, 3, "no new commit persisted");
    assert_eq!(tree_count(&storage).await, 3, "no new tree persisted");
}

/// Buck-upload fixture: base commit + root tree, the draft CL consumed by
/// `get_and_update_cl_in_txn`, and a session with one uploaded file.
/// Returns the base commit hash (`from_hash`).
async fn buck_fixture(storage: &Storage, link: &str, username: &str) -> String {
    let blob_old =
        ObjectHash::from_hex_for_kind(get_hash_kind(), "4444444444444444444444444444444444444444")
            .unwrap();
    let base_tree = Tree::from_tree_items(vec![blob_item("old.txt", &blob_old.to_string())])
        .expect("base tree");
    let base_commit = Commit::from_tree_id(base_tree.id, vec![], "mc09 buck base");
    let base_hash = base_commit.id.to_string();

    let mono = storage.mono_storage();
    mono.save_mega_commits(vec![base_commit.clone()], None)
        .await
        .expect("base commit");
    mono.save_mega_trees(vec![base_tree.clone()], base_commit.id, None)
        .await
        .expect("base tree");

    storage
        .cl_storage()
        .new_cl(
            "/",
            link,
            "mc09 buck",
            "main",
            &base_hash,
            &base_hash,
            username,
        )
        .await
        .expect("cl");

    let now = chrono::Utc::now().naive_utc();
    crate::callisto::buck_session::ActiveModel {
        id: Set(1),
        session_id: Set(link.to_string()),
        user_id: Set(username.to_string()),
        repo_path: Set("/".to_string()),
        status: Set(session_status::MANIFEST_UPLOADED.to_string()),
        commit_message: Set(Some("buck upload".to_string())),
        from_hash: Set(Some(base_hash.clone())),
        expires_at: Set(now + chrono::Duration::hours(1)),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(storage.buck_storage().get_connection())
    .await
    .expect("buck session");

    let new_blob = "5555555555555555555555555555555555555555";
    crate::callisto::buck_session_file::ActiveModel {
        id: Set(1),
        session_id: Set(link.to_string()),
        file_path: Set("new.txt".to_string()),
        file_size: Set(3),
        file_hash: Set(format!("sha1:{new_blob}")),
        file_mode: Set(Some("100644".to_string())),
        upload_status: Set(upload_status::UPLOADED.to_string()),
        upload_reason: Set(None),
        blob_id: Set(Some(format!("sha1:{new_blob}"))),
        uploaded_at: Set(Some(now)),
        created_at: Set(now),
    }
    .insert(storage.buck_storage().get_connection())
    .await
    .expect("buck file");

    base_hash
}

/// `test_storage` mocks `BuckService` with a disconnected connection;
/// rewire it onto the test database so `complete_upload` persists.
fn wire_buck_service(storage: &mut Storage) {
    let conn = storage.mono_storage().get_connection().clone();
    let base = BaseStorage::new(Arc::new(conn));
    storage.buck_service = crate::jupiter::service::buck_service::BuckService::new(
        base,
        crate::jupiter::service::cl_service::CLService::mock(),
        Arc::new(tokio::sync::Semaphore::new(10)),
        Arc::new(tokio::sync::Semaphore::new(5)),
        crate::config::BuckConfig::default(),
        crate::jupiter::service::git_service::GitService::mock(),
    )
    .expect("buck service");
}

// AC⑥ (buck chain): the upload-synthesized commit is server-signed and
// all four derived references — including every tree model's commit_id,
// backfilled one by one — carry the final signed hash; the ref-pointed
// commit passes the MC-02 chain verifier.
#[tokio::test]
async fn complete_buck_upload_signs_commit_and_backfills_references() {
    let temp = tempfile::tempdir().unwrap();
    let vault = test_vault(temp.path()).await;
    let redis = test_redis().await;
    let mut storage = crate::jupiter::tests::test_storage(temp.path())
        .await
        .with_vault(vault.clone());
    wire_buck_service(&mut storage);
    let service = signing_service(&storage, redis.clone());

    let link = "MC09BUP1";
    let base_hash = buck_fixture(&storage, link, "alice").await;

    let response = service
        .complete_buck_upload("alice", link, CompletePayload {})
        .await
        .expect("complete_buck_upload with signing");
    let signed = response.commit_id;
    assert_ne!(signed, base_hash, "a new upload commit must be created");

    let mono = storage.mono_storage();
    let commit = mono
        .get_commit_by_hash(&signed)
        .await
        .expect("load commit")
        .expect("signed commit row");
    let cl_ref = mono
        .get_ref_by_name(&format!("refs/cl/{link}"))
        .await
        .expect("load cl ref")
        .expect("cl ref created by complete_upload");
    let cl = storage
        .cl_storage()
        .get_cl(link)
        .await
        .expect("load cl")
        .expect("cl row");

    // Four sinks, one final signed hash.
    assert_eq!(commit.commit_id, signed);
    assert_eq!(cl_ref.ref_commit_hash, signed, "CL ref");
    assert_eq!(cl.to_hash, signed, "mega_cl.to_hash");
    assert_eq!(cl.from_hash, base_hash);
    assert_server_commit(&commit);

    // Every new tree model was backfilled with the signed commit_id (the
    // root tree is reachable through the ref; assert the whole set).
    let stamped = crate::callisto::mega_tree::Entity::find()
        .filter(crate::callisto::mega_tree::Column::CommitId.eq(&signed))
        .all(mono.get_connection())
        .await
        .expect("stamped trees");
    assert!(
        !stamped.is_empty(),
        "new tree models must carry the signed commit_id"
    );
    assert!(
        stamped.iter().any(|t| t.tree_id == cl_ref.ref_tree_hash),
        "the ref-pointed root tree must carry the signed commit_id"
    );
    assert_eq!(
        tree_count(&storage).await,
        1 + stamped.len(),
        "only the base tree plus the stamped new trees exist"
    );

    // MC-10 test hygiene: no `gpg_key` registration — the chain must
    // verify through the server keyring routing alone (MC-11).
    assert_chain_verifies(&storage, &base_hash, &signed).await;
}

// AC③/AC④ (buck chain): without the vault handle the precheck fails
// closed before `build_commit`; session, CL, refs, commits and trees are
// all untouched.
#[tokio::test]
async fn complete_buck_upload_without_vault_fails_closed_with_zero_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let mut storage = crate::jupiter::tests::test_storage(temp.path()).await;
    wire_buck_service(&mut storage);
    let service = test_service(&storage);

    let link = "MC09BUP2";
    let base_hash = buck_fixture(&storage, link, "alice").await;

    let err = service
        .complete_buck_upload("alice", link, CompletePayload {})
        .await
        .expect_err("complete_buck_upload must fail closed without a vault");
    assert!(
        err.to_string().contains("server signing unavailable"),
        "{err}"
    );

    let mono = storage.mono_storage();
    let cl = storage
        .cl_storage()
        .get_cl(link)
        .await
        .expect("load cl")
        .expect("cl row");
    assert_eq!(cl.from_hash, base_hash, "CL from_hash unchanged");
    assert_eq!(cl.to_hash, base_hash, "CL to_hash unchanged");
    let cl_ref = mono
        .get_ref_by_name(&format!("refs/cl/{link}"))
        .await
        .expect("load cl ref");
    assert!(cl_ref.is_none(), "no CL ref may be created");
    assert_eq!(commit_count(&storage).await, 1, "no new commit persisted");
    assert_eq!(tree_count(&storage).await, 1, "no new tree persisted");
    let session = storage
        .buck_storage()
        .get_session(link)
        .await
        .expect("load session")
        .expect("session row");
    assert_eq!(
        session.status,
        session_status::MANIFEST_UPLOADED,
        "session status unchanged"
    );
}

/// TP-04: root CAS tripwire is the sole root write at the storage boundary
/// that future MonoWriteQueue writers (merge/attach/push) will call through.
#[tokio::test]
async fn root_cas_update_is_exactly_one_write_per_success() {
    use sea_orm::TransactionTrait;

    let temp = tempfile::TempDir::new().unwrap();
    let storage = crate::jupiter::tests::test_storage(temp.path()).await;
    let mono = storage.mono_storage();
    let root = mega_refs::Model::new(
        "/",
        MEGA_BRANCH_NAME.to_owned(),
        "a".repeat(40),
        "b".repeat(40),
        false,
    );
    mono.save_refs(root.clone(), None).await.unwrap();

    let conn = mono.get_connection();
    let txn = conn.begin().await.unwrap();
    let first = mono
        .cas_update_root_main_ref_in_txn(
            &txn,
            Some(&root.ref_commit_hash),
            Some(&root.ref_tree_hash),
            &"c".repeat(40),
            &"d".repeat(40),
        )
        .await
        .unwrap();
    assert!(first, "successful CAS must affect exactly one root row");
    let second = mono
        .cas_update_root_main_ref_in_txn(
            &txn,
            Some(&root.ref_commit_hash),
            Some(&root.ref_tree_hash),
            &"e".repeat(40),
            &"f".repeat(40),
        )
        .await
        .unwrap();
    assert!(
        !second,
        "stale expected_* must miss — no second root write in the same success round"
    );
    txn.commit().await.unwrap();

    let after = mono.get_main_ref("/").await.unwrap().unwrap();
    assert_eq!(after.ref_commit_hash, "c".repeat(40));
    assert_eq!(after.ref_tree_hash, "d".repeat(40));
}

async fn queue_merge_fixture(
    path: &str,
    link: &str,
    extra_child: Option<(String, Tree)>,
) -> (
    tempfile::TempDir,
    Storage,
    MonoApiService,
    mega_cl::Model,
    String,
) {
    use git_internal::internal::object::commit::Commit;

    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
    let service = test_service(&storage);
    let mono = storage.mono_storage();

    let mut root_items = vec![blob_item(
        ".gitkeep",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    )];
    let path_tree_hash = extra_child.as_ref().map(|(_, t)| t.id.to_string());
    if let Some((name, child)) = extra_child {
        mono.save_mega_trees(
            vec![child.clone()],
            ObjectHash::from_hex_for_kind(get_hash_kind(), &"1".repeat(40)).unwrap(),
            None,
        )
        .await
        .unwrap();
        root_items.push(TreeItem::new(TreeItemMode::Tree, child.id, name));
    }
    let old_tree = Tree::from_tree_items(root_items).expect("old tree");
    let new_tree = Tree::from_tree_items(vec![blob_item(
        "queued.txt",
        "cccccccccccccccccccccccccccccccccccccccc",
    )])
    .expect("new tree");

    let old_commit = Commit::from_tree_id(old_tree.id, vec![], "base");
    let new_commit = Commit::from_tree_id(new_tree.id, vec![old_commit.id], "cl tip");
    mono.save_mega_trees(
        vec![old_tree.clone(), new_tree.clone()],
        old_commit.id,
        None,
    )
    .await
    .unwrap();
    mono.save_mega_commits(vec![old_commit.clone(), new_commit.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &old_tree, &old_commit.id.to_string()).await;
    if path != "/" {
        mono.save_refs(
            mega_refs::Model {
                id: crate::callisto::entity_ext::generate_id(),
                path: path.to_string(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: old_commit.id.to_string(),
                ref_tree_hash: path_tree_hash
                    .clone()
                    .unwrap_or_else(|| old_tree.id.to_string()),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .unwrap();
    }
    mono.save_or_update_cl_ref(
        path,
        &format!("refs/cl/{link}"),
        &new_commit.id.to_string(),
        &new_tree.id.to_string(),
    )
    .await
    .unwrap();
    let cl = storage
        .cl_storage()
        .new_cl_model(
            path,
            link,
            "queue merge",
            "main",
            &old_commit.id.to_string(),
            &new_commit.id.to_string(),
            "gate-tester",
        )
        .await
        .unwrap();
    (temp, storage, service, cl, new_commit.id.to_string())
}

#[tokio::test]
async fn queue_merge_new_id_differs_from_landed_commit() {
    let (_temp, storage, service, cl, to_hash) = queue_merge_fixture("/", "QMRG01", None).await;
    service
        .merge_cl("gate-tester", "gate-tester", cl.clone())
        .await
        .expect("queue merge should succeed");
    let merged = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.status, MergeStatusEnum::Merged);
    let root = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(root.ref_commit_hash, to_hash, "landed tip is synthesized");
    let landed = storage
        .mono_storage()
        .get_commit_by_hash(&root.ref_commit_hash)
        .await
        .unwrap()
        .unwrap();
    let parents: Vec<String> = serde_json::from_value(landed.parents_id).unwrap_or_default();
    assert_eq!(
        parents,
        vec![to_hash.clone()],
        "parent is execute-time to_hash"
    );
    let row = crate::callisto::push_queue::Entity::find()
        .filter(crate::callisto::push_queue::Column::OperationId.eq(&cl.link))
        .one(storage.push_queue_storage().get_connection())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.new_id, to_hash);
    assert_eq!(
        row.landed_commit_id.as_deref(),
        Some(root.ref_commit_hash.as_str())
    );
    assert_ne!(row.new_id, row.landed_commit_id.clone().unwrap());
}

#[tokio::test]
async fn queue_merge_missing_path_main_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
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
    let cl = gate_test_cl(
        "QMISS1",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    let mut cl = cl;
    cl.path = "/missing".into();
    let err = service
        .merge_cl("gate-tester", "gate-tester", cl)
        .await
        .expect_err("missing main@P must refuse at entry");
    assert!(err.to_string().to_lowercase().contains("main ref"), "{err}");
}

#[tokio::test]
async fn queue_merge_nested_descendant_continues_then_second_merge_lands() {
    let _lock = crate::ceres::pack::materialize::lock_materialize_tests().await;
    let blob_old = blob_item("x.txt", "dddddddddddddddddddddddddddddddddddddddd");
    let b_old = Tree::from_tree_items(vec![blob_old]).expect("b old");
    let a_old = Tree::from_tree_items(vec![TreeItem::new(
        TreeItemMode::Tree,
        b_old.id,
        "b".into(),
    )])
    .expect("a old");
    let blob_new = blob_item("x.txt", "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
    let b_mid = Tree::from_tree_items(vec![blob_new]).expect("b mid");
    let a_new = Tree::from_tree_items(vec![
        TreeItem::new(TreeItemMode::Tree, b_mid.id, "b".into()),
        blob_item("queued.txt", "cccccccccccccccccccccccccccccccccccccccc"),
    ])
    .expect("a new");

    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
    let service = test_service(&storage);
    let mono = storage.mono_storage();

    let root = Tree::from_tree_items(vec![
        blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        TreeItem::new(TreeItemMode::Tree, a_old.id, "a".into()),
    ])
    .expect("root");
    let old_commit = Commit::from_tree_id(root.id, vec![], "base");
    let cl_a_commit = Commit::from_tree_id(a_new.id, vec![old_commit.id], "cl /a");
    let b_commit = Commit::from_tree_id(b_old.id, vec![], "main@/a/b");
    let a_commit = Commit::from_tree_id(a_old.id, vec![], "main@/a");
    mono.save_mega_trees(
        vec![
            b_old.clone(),
            a_old.clone(),
            b_mid.clone(),
            a_new.clone(),
            root.clone(),
        ],
        old_commit.id,
        None,
    )
    .await
    .unwrap();
    mono.save_mega_commits(
        vec![
            old_commit.clone(),
            cl_a_commit.clone(),
            b_commit.clone(),
            a_commit.clone(),
        ],
        None,
    )
    .await
    .unwrap();
    setup_main_ref(&storage, &root, &old_commit.id.to_string()).await;
    mono.save_refs(
        mega_refs::Model {
            id: crate::callisto::entity_ext::generate_id(),
            path: "/a".into(),
            ref_name: MEGA_BRANCH_NAME.to_string(),
            ref_commit_hash: a_commit.id.to_string(),
            ref_tree_hash: a_old.id.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            is_cl: false,
        },
        None,
    )
    .await
    .unwrap();
    mono.save_refs(
        mega_refs::Model {
            id: crate::callisto::entity_ext::generate_id(),
            path: "/a/b".into(),
            ref_name: MEGA_BRANCH_NAME.to_string(),
            ref_commit_hash: b_commit.id.to_string(),
            ref_tree_hash: b_old.id.to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            is_cl: false,
        },
        None,
    )
    .await
    .unwrap();
    mono.save_or_update_cl_ref(
        "/a",
        "refs/cl/QNEST1",
        &cl_a_commit.id.to_string(),
        &a_new.id.to_string(),
    )
    .await
    .unwrap();
    let cl_a = storage
        .cl_storage()
        .new_cl_model(
            "/a",
            "QNEST1",
            "queue merge /a",
            "main",
            &a_commit.id.to_string(),
            &cl_a_commit.id.to_string(),
            "gate-tester",
        )
        .await
        .unwrap();

    service
        .merge_cl("gate-tester", "gate-tester", cl_a)
        .await
        .expect("/a merge");

    let after_a = storage
        .mono_storage()
        .get_main_ref("/a/b")
        .await
        .unwrap()
        .expect("descendant must continue, not be deleted");
    assert_eq!(after_a.ref_tree_hash, b_mid.id.to_string());
    let cont = storage
        .mono_storage()
        .get_commit_by_hash(&after_a.ref_commit_hash)
        .await
        .unwrap()
        .unwrap();
    let parents: Vec<String> = serde_json::from_value(cont.parents_id).unwrap();
    assert_eq!(parents, vec![b_commit.id.to_string()]);

    let blob_tip = blob_item("x.txt", "ffffffffffffffffffffffffffffffffffffffff");
    let b_tip = Tree::from_tree_items(vec![blob_tip]).expect("b tip");
    let parent = ObjectHash::from_hex_for_kind(get_hash_kind(), &after_a.ref_commit_hash).unwrap();
    let cl_b_commit = Commit::from_tree_id(b_tip.id, vec![parent], "cl /a/b");
    mono.save_mega_trees(vec![b_tip.clone()], cl_b_commit.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![cl_b_commit.clone()], None)
        .await
        .unwrap();
    let cl_b = storage
        .cl_storage()
        .new_cl_model(
            "/a/b",
            "QNEST2",
            "queue merge /a/b",
            "main",
            &after_a.ref_commit_hash,
            &cl_b_commit.id.to_string(),
            "gate-tester",
        )
        .await
        .unwrap();
    service
        .merge_cl("gate-tester", "gate-tester", cl_b)
        .await
        .expect("/a/b merge after parent continuation");
    let landed = storage
        .mono_storage()
        .get_main_ref("/a/b")
        .await
        .unwrap()
        .unwrap();
    let landed_c = storage
        .mono_storage()
        .get_commit_by_hash(&landed.ref_commit_hash)
        .await
        .unwrap()
        .unwrap();
    let landed_parents: Vec<String> = serde_json::from_value(landed_c.parents_id).unwrap();
    assert_eq!(
        landed_parents,
        vec![after_a.ref_commit_hash],
        "second merge parent must equal post-/a continuation tip"
    );
}

#[tokio::test]
async fn queue_merge_add_to_merge_queue_executes_synchronously() {
    let (_temp, storage, service, cl, to_hash) = queue_merge_fixture("/", "QADD01", None).await;
    let id = service
        .add_to_merge_queue_as(cl.link.clone(), Some("gate-tester".into()))
        .await
        .expect("queue-mode add is sync merge");
    assert!(id > 0);
    let merged = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.status, MergeStatusEnum::Merged);
    let root = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(root.ref_commit_hash, to_hash);
}

#[tokio::test]
async fn queue_merge_twenty_non_nested_paths_serialize() {
    use git_internal::internal::object::commit::Commit;

    const N: usize = 20;
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
    let service = test_service(&storage);
    let mono = storage.mono_storage();

    let mut child_trees = Vec::new();
    for i in 0..N {
        let blob = blob_item("base.txt", &format!("{:040x}", i + 1));
        let tree = Tree::from_tree_items(vec![blob]).expect("child tree");
        child_trees.push(tree);
    }
    let root_items: Vec<TreeItem> = child_trees
        .iter()
        .enumerate()
        .map(|(i, t)| TreeItem::new(TreeItemMode::Tree, t.id, format!("p{i:02}")))
        .collect();
    let old_root = Tree::from_tree_items(root_items).expect("root");
    let old_commit = Commit::from_tree_id(old_root.id, vec![], "base");
    for t in &child_trees {
        mono.save_mega_trees(vec![t.clone()], old_commit.id, None)
            .await
            .unwrap();
    }
    mono.save_mega_trees(vec![old_root.clone()], old_commit.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![old_commit.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &old_root, &old_commit.id.to_string()).await;

    let mut cls = Vec::new();
    for (i, child) in child_trees.iter().enumerate() {
        let path = format!("/p{i:02}");
        let link = format!("Q20{i:02}");
        let tip_tree =
            Tree::from_tree_items(vec![blob_item("new.txt", &format!("{:040x}", 1000 + i))])
                .expect("tip tree");
        let tip = Commit::from_tree_id(tip_tree.id, vec![old_commit.id], "cl tip");
        mono.save_mega_trees(vec![tip_tree.clone()], tip.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![tip.clone()], None)
            .await
            .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                path.clone(),
                MEGA_BRANCH_NAME.to_owned(),
                old_commit.id.to_string(),
                child.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        mono.save_or_update_cl_ref(
            &path,
            &format!("refs/cl/{link}"),
            &tip.id.to_string(),
            &tip_tree.id.to_string(),
        )
        .await
        .unwrap();
        let cl = storage
            .cl_storage()
            .new_cl_model(
                &path,
                &link,
                "q20",
                "main",
                &old_commit.id.to_string(),
                &tip.id.to_string(),
                "gate-tester",
            )
            .await
            .unwrap();
        cls.push(cl);
    }

    let mut joins = Vec::new();
    for cl in cls.clone() {
        let svc = service.clone();
        joins.push(tokio::spawn(async move {
            svc.merge_cl("gate-tester", "gate-tester", cl).await
        }));
    }
    for j in joins {
        j.await.unwrap().expect("concurrent merge");
    }

    for cl in &cls {
        let row = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, MergeStatusEnum::Merged, "{}", cl.link);
    }
    let final_root = mono.get_main_ref("/").await.unwrap().unwrap();
    let final_tree = Tree::from_mega_model(
        mono.get_tree_by_hash(&final_root.ref_tree_hash)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(final_tree.tree_items.len(), N, "all path trees remain");

    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
    let qrows = crate::callisto::push_queue::Entity::find()
        .filter(
            crate::callisto::push_queue::Column::Kind
                .eq(crate::callisto::sea_orm_active_enums::PushQueueKindEnum::Merge),
        )
        .filter(
            crate::callisto::push_queue::Column::Status
                .eq(crate::callisto::sea_orm_active_enums::PushQueueStatusEnum::Done),
        )
        .order_by_asc(crate::callisto::push_queue::Column::Id)
        .all(storage.push_queue_storage().get_connection())
        .await
        .unwrap();
    assert_eq!(qrows.len(), N);
    assert_eq!(
        qrows.last().unwrap().landed_commit_id.as_deref(),
        Some(final_root.ref_commit_hash.as_str()),
        "highest queue id is the final root tip"
    );
    let mut seen = std::collections::HashSet::new();
    for row in &qrows {
        assert!(seen.insert(row.landed_commit_id.clone().unwrap()));
    }
    let mut hash = final_root.ref_commit_hash.clone();
    for row in qrows.iter().rev() {
        assert_eq!(
            row.landed_commit_id.as_deref(),
            Some(hash.as_str()),
            "root roll-up parent chain must follow push_queue.id order"
        );
        let commit = mono.get_commit_by_hash(&hash).await.unwrap().unwrap();
        let parents: Vec<String> = serde_json::from_value(commit.parents_id).unwrap_or_default();
        hash = parents
            .into_iter()
            .next()
            .expect("root roll-up must have a parent");
    }
    assert_eq!(
        hash,
        old_commit.id.to_string(),
        "oldest roll-up parent is the original root"
    );
}

fn merge_exec_ctx(storage: &Storage) -> MergeExecContext {
    MergeExecContext {
        storage: storage.clone(),
        git_object_cache: test_service(storage).git_object_cache,
        abort_before_cl_status: false,
        pause_after_apply: Duration::ZERO,
        pause_after_apply_barrier: None,
    }
}

async fn enqueue_merge_row(storage: &Storage, cl: &mega_cl::Model) -> i64 {
    use crate::jupiter::storage::push_queue_storage::EnqueueOutcome;

    let payload = MergePayload {
        cl_link: cl.link.clone(),
        authz_principal: "gate-tester".into(),
        execution_actor: "gate-tester".into(),
        apply_queue_execution_decision: false,
        requester: Some("gate-tester".into()),
    };
    let old_id = storage
        .mono_storage()
        .get_main_ref(&cl.path)
        .await
        .unwrap()
        .map(|r| r.ref_commit_hash)
        .unwrap_or_else(|| ZERO_ID.to_owned());
    match storage
        .push_queue_service
        .enqueue(EnqueueRequest {
            kind: PushQueueKindEnum::Merge,
            operation_id: merge_operation_id(&cl.link),
            path: cl.path.clone(),
            old_id,
            new_id: cl.to_hash.clone(),
            requester: Some("gate-tester".into()),
            payload: serde_json::to_value(&payload).unwrap(),
            ref_name: None,
            is_delete: false,
        })
        .await
        .unwrap()
    {
        EnqueueOutcome::Inserted { id } | EnqueueOutcome::Adopted { id } => id,
        other => panic!("expected insert, got {other:?}"),
    }
}

#[tokio::test]
async fn queue_merge_add_rejects_failed_gpg_like_merge_cl() {
    let (_temp, storage, service, cl, _) = queue_merge_fixture("/", "QADDGPG", None).await;
    insert_check_result(&storage, &cl.link, CheckTypeEnum::GpgSignature, "FAILED").await;
    let err = service
        .add_to_merge_queue_as(cl.link.clone(), Some("gate-tester".into()))
        .await
        .expect_err("queue-mode add must run GPG entry precheck");
    assert!(
        err.to_string().contains("GPG signature check failed"),
        "{err}"
    );
}

#[tokio::test]
async fn queue_merge_add_rejects_from_hash_mismatch() {
    let (_temp, storage, service, cl, _) = queue_merge_fixture("/", "QADDFH", None).await;
    let txn = storage.begin_db_transaction().await.unwrap();
    let current = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    assert!(
        storage
            .cl_storage()
            .cas_update_cl_hashes_in_txn(&current, &"f".repeat(40), &current.to_hash, &txn)
            .await
            .unwrap()
    );
    txn.commit().await.unwrap();
    let err = service
        .add_to_merge_queue_as(cl.link, Some("gate-tester".into()))
        .await
        .expect_err("from_hash != main must refuse at entry");
    assert!(err.to_string().to_lowercase().contains("conflict"), "{err}");
}

#[tokio::test]
async fn queue_merge_abort_before_cl_status_rolls_back() {
    let (_temp, storage, _service, cl, _) = queue_merge_fixture("/", "QABORT", None).await;
    let root_before = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    let id = enqueue_merge_row(&storage, &cl).await;
    assert_eq!(
        storage.push_queue_service.wait_and_claim(id).await.unwrap(),
        QueueWaitResult::Ready { id }
    );
    let mut ctx = merge_exec_ctx(&storage);
    ctx.abort_before_cl_status = true;
    let outcome = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id,
                ..Default::default()
            },
            None,
            Some(&ctx),
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, ExecuteOutcome::Failed { .. }),
        "abort should terminalize after rollback, got {outcome:?}"
    );
    let root_after = storage
        .mono_storage()
        .get_main_ref("/")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root_after.ref_commit_hash, root_before.ref_commit_hash);
    let still = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.status, MergeStatusEnum::Open);
}

#[tokio::test]
async fn queue_merge_intervening_other_path_is_preserved() {
    use git_internal::internal::object::commit::Commit;

    const N: usize = 2;
    let temp = tempfile::tempdir().unwrap();
    let storage = crate::jupiter::tests::test_storage_queue_merge(temp.path()).await;
    let mono = storage.mono_storage();
    let mut child_trees = Vec::new();
    for i in 0..N {
        let blob = blob_item("base.txt", &format!("{:040x}", i + 1));
        child_trees.push(Tree::from_tree_items(vec![blob]).expect("child"));
    }
    let root_items: Vec<TreeItem> = child_trees
        .iter()
        .enumerate()
        .map(|(i, t)| TreeItem::new(TreeItemMode::Tree, t.id, format!("p{i:02}")))
        .collect();
    let old_root = Tree::from_tree_items(root_items).expect("root");
    let old_commit = Commit::from_tree_id(old_root.id, vec![], "base");
    for t in &child_trees {
        mono.save_mega_trees(vec![t.clone()], old_commit.id, None)
            .await
            .unwrap();
    }
    mono.save_mega_trees(vec![old_root.clone()], old_commit.id, None)
        .await
        .unwrap();
    mono.save_mega_commits(vec![old_commit.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &old_root, &old_commit.id.to_string()).await;

    let mut cls = Vec::new();
    for (i, child) in child_trees.iter().enumerate() {
        let path = format!("/p{i:02}");
        let link = format!("QINT{i:02}");
        let tip_tree =
            Tree::from_tree_items(vec![blob_item("new.txt", &format!("{:040x}", 1000 + i))])
                .expect("tip");
        let tip = Commit::from_tree_id(tip_tree.id, vec![old_commit.id], "cl tip");
        mono.save_mega_trees(vec![tip_tree.clone()], tip.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![tip.clone()], None)
            .await
            .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                path.clone(),
                MEGA_BRANCH_NAME.to_owned(),
                old_commit.id.to_string(),
                child.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        mono.save_or_update_cl_ref(
            &path,
            &format!("refs/cl/{link}"),
            &tip.id.to_string(),
            &tip_tree.id.to_string(),
        )
        .await
        .unwrap();
        cls.push(
            storage
                .cl_storage()
                .new_cl_model(
                    &path,
                    &link,
                    "qint",
                    "main",
                    &old_commit.id.to_string(),
                    &tip.id.to_string(),
                    "gate-tester",
                )
                .await
                .unwrap(),
        );
    }

    let first = enqueue_merge_row(&storage, &cls[0]).await;
    let second = enqueue_merge_row(&storage, &cls[1]).await;
    let ctx = merge_exec_ctx(&storage);
    assert_eq!(
        storage
            .push_queue_service
            .wait_and_claim(first)
            .await
            .unwrap(),
        QueueWaitResult::Ready { id: first }
    );
    let first_out = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id: first,
                ..Default::default()
            },
            None,
            Some(&ctx),
            None,
        )
        .await
        .unwrap();
    let ExecuteOutcome::Done {
        landed_commit_id: tip_after_first,
        ..
    } = first_out
    else {
        panic!("first merge Done, got {first_out:?}");
    };
    assert_eq!(
        storage
            .push_queue_service
            .wait_and_claim(second)
            .await
            .unwrap(),
        QueueWaitResult::Ready { id: second }
    );
    let second_out = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id: second,
                ..Default::default()
            },
            None,
            Some(&ctx),
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(second_out, ExecuteOutcome::Done { .. }),
        "second merge Done after intervening, got {second_out:?}"
    );
    let final_root = mono.get_main_ref("/").await.unwrap().unwrap();
    let final_tree = Tree::from_mega_model(
        mono.get_tree_by_hash(&final_root.ref_tree_hash)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(final_tree.tree_items.len(), N);
    let names: Vec<_> = final_tree
        .tree_items
        .iter()
        .map(|i| i.name.clone())
        .collect();
    assert!(names.contains(&"p00".into()) && names.contains(&"p01".into()));
    assert_ne!(final_root.ref_commit_hash, tip_after_first);
}

#[tokio::test]
async fn queue_merge_conflict_requeue_then_succeeds() {
    let blob = blob_item("x.txt", "dddddddddddddddddddddddddddddddddddddddd");
    let child = Tree::from_tree_items(vec![blob.clone()]).expect("child");
    let (_temp, storage, _service, cl, _) =
        queue_merge_fixture("/p00", "QCF01", Some(("p00".into(), child))).await;
    let id = enqueue_merge_row(&storage, &cl).await;
    assert_eq!(
        storage.push_queue_service.wait_and_claim(id).await.unwrap(),
        QueueWaitResult::Ready { id }
    );
    let path_main = storage
        .mono_storage()
        .get_main_ref("/p00")
        .await
        .unwrap()
        .unwrap();
    let mut advanced = path_main.clone();
    advanced.ref_commit_hash = "e".repeat(40);
    storage
        .mono_storage()
        .update_ref(advanced, None)
        .await
        .unwrap();
    let ctx = merge_exec_ctx(&storage);
    let outcome = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id,
                ..Default::default()
            },
            None,
            Some(&ctx),
            None,
        )
        .await
        .unwrap();
    let ExecuteOutcome::Requeued { successor_id, .. } = outcome else {
        panic!("expected B4 requeue, got {outcome:?}");
    };
    let old_row = storage
        .push_queue_storage()
        .get_by_id(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_row.superseded_by, Some(successor_id));
    assert!(successor_id > id);

    let current = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    let txn = storage.begin_db_transaction().await.unwrap();
    assert!(
        storage
            .cl_storage()
            .cas_update_cl_hashes_in_txn(&current, &"e".repeat(40), &current.to_hash, &txn)
            .await
            .unwrap()
    );
    txn.commit().await.unwrap();

    assert_eq!(
        storage
            .push_queue_service
            .wait_and_claim(successor_id)
            .await
            .unwrap(),
        QueueWaitResult::Ready { id: successor_id }
    );
    let done = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id: successor_id,
                ..Default::default()
            },
            None,
            Some(&ctx),
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(done, ExecuteOutcome::Done { .. }),
        "successor must merge, got {done:?}"
    );
    let merged = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(merged.status, MergeStatusEnum::Merged);
}

#[tokio::test]
async fn queue_merge_revision_cas_miss_is_claim_lost_then_retries() {
    let (_temp, storage, _service, cl, _) = queue_merge_fixture("/", "QCAS1", None).await;
    let id = enqueue_merge_row(&storage, &cl).await;
    assert_eq!(
        storage.push_queue_service.wait_and_claim(id).await.unwrap(),
        QueueWaitResult::Ready { id }
    );
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mut ctx = merge_exec_ctx(&storage);
    ctx.pause_after_apply = Duration::from_millis(200);
    ctx.pause_after_apply_barrier = Some(barrier.clone());
    let storage_racer = storage.clone();
    let cl_link = cl.link.clone();
    let racer = tokio::spawn(async move {
        barrier.wait().await;
        let current = storage_racer
            .cl_storage()
            .get_cl(&cl_link)
            .await
            .unwrap()
            .unwrap();
        let txn = storage_racer.begin_db_transaction().await.unwrap();
        let ok = storage_racer
            .cl_storage()
            .cas_update_cl_hashes_in_txn(&current, &current.from_hash, &current.to_hash, &txn)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        ok
    });
    let outcome = storage
        .push_queue_service
        .execute_b3(
            ExecuteRequest {
                id,
                ..Default::default()
            },
            None,
            Some(&ctx),
            None,
        )
        .await
        .unwrap();
    let raced = racer.await.unwrap();
    if raced {
        assert_eq!(outcome, ExecuteOutcome::ClaimLost { id });
        let still = storage
            .cl_storage()
            .get_cl(&cl.link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.status, MergeStatusEnum::Open);
        let ctx = merge_exec_ctx(&storage);
        assert_eq!(
            storage.push_queue_service.wait_and_claim(id).await.unwrap(),
            QueueWaitResult::Ready { id }
        );
        let retry = storage
            .push_queue_service
            .execute_b3(
                ExecuteRequest {
                    id,
                    ..Default::default()
                },
                None,
                Some(&ctx),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(retry, ExecuteOutcome::Done { .. }),
            "retry after ClaimLost, got {retry:?}"
        );
    } else {
        assert!(
            matches!(outcome, ExecuteOutcome::Done { .. }),
            "if the racer missed, the merge itself must Done, got {outcome:?}"
        );
    }
}

#[tokio::test]
async fn queue_merge_queue_entry_un17_freezes_anonymous_under_enforce() {
    use git_internal::internal::object::commit::Commit;

    use crate::config::testing::isolated_config;

    let temp = tempfile::tempdir().unwrap();
    let mut config = isolated_config(temp.path().join("config"));
    config.cedar.enforcement = "enforce".into();
    let storage = crate::jupiter::tests::test_storage_with_config(temp.path(), config).await;
    let blob = blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let old_tree = Tree::from_tree_items(vec![blob]).expect("old");
    let new_tree = Tree::from_tree_items(vec![blob_item(
        "queued.txt",
        "cccccccccccccccccccccccccccccccccccccccc",
    )])
    .expect("new");
    let old_commit = Commit::from_tree_id(old_tree.id, vec![], "base");
    let new_commit = Commit::from_tree_id(new_tree.id, vec![old_commit.id], "cl tip");
    let mono = storage.mono_storage();
    mono.save_mega_trees(
        vec![old_tree.clone(), new_tree.clone()],
        old_commit.id,
        None,
    )
    .await
    .unwrap();
    mono.save_mega_commits(vec![old_commit.clone(), new_commit.clone()], None)
        .await
        .unwrap();
    setup_main_ref(&storage, &old_tree, &old_commit.id.to_string()).await;
    mono.save_or_update_cl_ref(
        "/",
        "refs/cl/QUN17",
        &new_commit.id.to_string(),
        &new_tree.id.to_string(),
    )
    .await
    .unwrap();
    let cl = storage
        .cl_storage()
        .new_cl_model(
            "/",
            "QUN17",
            "un17",
            "main",
            &old_commit.id.to_string(),
            &new_commit.id.to_string(),
            "gate-tester",
        )
        .await
        .unwrap();
    let service = test_service(&storage);
    let err = service
        .add_to_merge_queue_as(cl.link.clone(), None)
        .await
        .expect_err("anonymous queue add under enforce must freeze");
    assert!(
        err.to_string().to_lowercase().contains("frozen")
            || err.to_string().to_lowercase().contains("requester"),
        "{err}"
    );
    let still = storage
        .cl_storage()
        .get_cl(&cl.link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.status, MergeStatusEnum::Open);
}
