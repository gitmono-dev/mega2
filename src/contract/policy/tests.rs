//! TP-22: outbox, monotonic publish, and per-instance read barrier.

use std::{str::FromStr, sync::Arc, time::Duration};

use git_internal::{
    hash::ObjectHash,
    internal::object::tree::{Tree, TreeItem, TreeItemMode},
};
use sea_orm::{EntityTrait, TransactionTrait};

use crate::{
    callisto::{authz_notify_outbox, mega_refs},
    common::utils::MEGA_BRANCH_NAME,
    contract::policy::{
        entitystore::{SharedEntityStore, generate_entity},
        notify::{
            authz_barrier_enabled, ensure_authz_snapshot_caught_up,
            insert_b3_authz_outbox_if_builds, mark_authz_dirty_and_compensate, replay_authz_outbox,
            set_after_authz_json_load_publish_for_test, set_rebuild_delay_for_test,
        },
    },
    jupiter::{
        storage::{base_storage::StorageConnector, push_queue_storage::PushQueueStorage},
        tests::test_storage_with_config,
    },
};

async fn storage_enforce(temp: &std::path::Path) -> crate::jupiter::storage::Storage {
    let mut config = crate::config::testing::isolated_config(temp.join("config"));
    config.cedar.enforcement = "enforce".to_string();
    test_storage_with_config(temp, config).await
}

async fn save_authz_json(storage: &crate::jupiter::storage::Storage, json: &str) -> String {
    storage
        .git_service
        .save_object_from_raw(bytes::Bytes::from(json.to_string()))
        .await
        .expect("save authz blob")
}

async fn seed_cedar_on_main(storage: &crate::jupiter::storage::Storage, json: &str) {
    let blob_id = save_authz_json(storage, json).await;
    let tree = Tree::from_tree_items(vec![TreeItem::new(
        TreeItemMode::Blob,
        ObjectHash::from_str(&blob_id).expect("blob hash"),
        ".mega_cedar.json".to_string(),
    )])
    .expect("root tree");
    let commit_id = "1111111111111111111111111111111111111111";
    storage
        .mono_storage()
        .save_mega_trees(
            vec![tree.clone()],
            ObjectHash::from_str(commit_id).unwrap(),
            None,
        )
        .await
        .expect("save tree");
    storage
        .mono_storage()
        .save_refs(
            mega_refs::Model {
                id: 1,
                path: "/".to_string(),
                ref_name: MEGA_BRANCH_NAME.to_string(),
                ref_commit_hash: commit_id.to_string(),
                ref_tree_hash: tree.id.to_string(),
                created_at: chrono::Utc::now().naive_utc(),
                updated_at: chrono::Utc::now().naive_utc(),
                is_cl: false,
            },
            None,
        )
        .await
        .expect("save main ref");
}

#[tokio::test]
async fn tp22_migration_seeds_published_version_and_outbox() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let conn = storage.mono_storage().get_connection().clone();
    assert_eq!(
        PushQueueStorage::load_published_version(&conn)
            .await
            .expect("watermark"),
        0
    );
    let count = authz_notify_outbox::Entity::find()
        .all(&conn)
        .await
        .expect("outbox readable");
    assert!(count.is_empty());
}

#[tokio::test]
async fn tp22_outbox_shares_b3_transaction() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let conn = storage.mono_storage().get_connection().clone();

    let txn = conn.begin().await.expect("begin");
    insert_b3_authz_outbox_if_builds(&storage, &txn, 42)
        .await
        .expect("insert");
    txn.rollback().await.expect("rollback");
    assert!(
        PushQueueStorage::pending_authz_outbox(&conn)
            .await
            .expect("pending")
            .is_empty(),
        "rolled-back outbox must not be visible"
    );

    let txn = conn.begin().await.expect("begin");
    insert_b3_authz_outbox_if_builds(&storage, &txn, 42)
        .await
        .expect("insert");
    txn.commit().await.expect("commit");
    let pending = PushQueueStorage::pending_authz_outbox(&conn)
        .await
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].version, Some(42));
    assert!(!pending[0].dirty);
}

#[tokio::test]
async fn tp22_kill9_after_outbox_replay_rebuilds_from_latest_root() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
    seed_cedar_on_main(&storage, &json).await;
    let conn = storage.mono_storage().get_connection().clone();

    PushQueueStorage::insert_authz_outbox(&conn, Some(9), false)
        .await
        .expect("outbox committed as if B3 finished before notify");
    assert!(storage.entity_store().snapshot().is_none());

    replay_authz_outbox(&storage)
        .await
        .expect("compensation replay");
    assert!(
        storage.entity_store().snapshot().is_some(),
        "replay must rebuild from the latest root"
    );
    assert_eq!(storage.entity_store().local_published_version(), 9);
    assert_eq!(
        PushQueueStorage::load_published_version(&conn)
            .await
            .expect("watermark"),
        9
    );
    assert!(
        PushQueueStorage::pending_authz_outbox(&conn)
            .await
            .expect("pending")
            .is_empty()
    );
}

#[tokio::test]
async fn tp22_monotonic_cas_rejects_stale_publish() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
    seed_cedar_on_main(&storage, &json).await;
    let conn = storage.mono_storage().get_connection().clone();

    PushQueueStorage::insert_authz_outbox(&conn, Some(10), false)
        .await
        .expect("newer outbox");
    replay_authz_outbox(&storage).await.expect("publish 10");
    let first = storage.entity_store().snapshot().expect("snapshot");
    assert_eq!(
        PushQueueStorage::load_published_version(&conn)
            .await
            .expect("watermark"),
        10
    );

    PushQueueStorage::insert_authz_outbox(&conn, Some(5), false)
        .await
        .expect("stale outbox");
    replay_authz_outbox(&storage)
        .await
        .expect("stale replay is Ok");
    assert_eq!(
        PushQueueStorage::load_published_version(&conn)
            .await
            .expect("watermark"),
        10,
        "publish order is id order"
    );
    let after = storage.entity_store().snapshot().expect("snapshot");
    assert!(
        Arc::ptr_eq(&first, &after),
        "CAS miss must not replace the newer snapshot"
    );
    assert_eq!(storage.entity_store().local_published_version(), 10);
}

#[tokio::test]
async fn tp22_read_barrier_rebuilds_lagging_instance() {
    let temp = tempfile::tempdir().expect("temp");
    let mut storage = storage_enforce(temp.path()).await;
    let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
    seed_cedar_on_main(&storage, &json).await;
    let conn = storage.mono_storage().get_connection().clone();
    PushQueueStorage::insert_authz_outbox(&conn, Some(10), false)
        .await
        .expect("outbox");
    replay_authz_outbox(&storage)
        .await
        .expect("instance A publish");
    assert_eq!(storage.entity_store().local_published_version(), 10);

    storage.set_entity_store_for_test(Arc::new(SharedEntityStore::new()));
    assert_eq!(storage.entity_store().local_published_version(), 0);
    ensure_authz_snapshot_caught_up(&storage, Duration::from_secs(5))
        .await
        .expect("barrier catch-up");
    assert_eq!(storage.entity_store().local_published_version(), 10);
    assert!(storage.entity_store().snapshot().is_some());
    assert!(!storage.entity_store().is_dirty());
}

#[tokio::test]
async fn tp22_read_barrier_timeout_is_fail_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let conn = storage.mono_storage().get_connection().clone();
    assert!(
        PushQueueStorage::cas_published_version(&conn, 10)
            .await
            .expect("advance watermark")
    );
    let err = ensure_authz_snapshot_caught_up(&storage, Duration::ZERO)
        .await
        .expect_err("zero timeout while behind");
    assert!(err.to_string().contains("timed out"));
    assert!(storage.entity_store().is_dirty());
}

#[tokio::test]
async fn tp22_barrier_clears_dirty_when_watermarks_already_match() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
    seed_cedar_on_main(&storage, &json).await;
    let conn = storage.mono_storage().get_connection().clone();
    assert!(
        PushQueueStorage::cas_published_version(&conn, 10)
            .await
            .expect("watermark")
    );
    storage
        .entity_store()
        .swap_at_version(&json, 10)
        .expect("local caught up");
    storage.entity_store().mark_dirty();
    ensure_authz_snapshot_caught_up(&storage, Duration::from_secs(5))
        .await
        .expect("dirty equal-watermark catch-up");
    assert!(!storage.entity_store().is_dirty());
    assert_eq!(storage.entity_store().local_published_version(), 10);
    assert!(storage.entity_store().snapshot().is_some());
}

#[tokio::test]
async fn tp22_off_bypasses_the_trio() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = test_storage_with_config(
        temp.path(),
        crate::config::testing::isolated_config(temp.path().join("config")),
    )
    .await;
    assert!(!authz_barrier_enabled(&storage));
    let conn = storage.mono_storage().get_connection().clone();
    let txn = conn.begin().await.expect("begin");
    insert_b3_authz_outbox_if_builds(&storage, &txn, 7)
        .await
        .expect("no-op insert");
    txn.commit().await.expect("commit");
    assert!(
        PushQueueStorage::pending_authz_outbox(&conn)
            .await
            .expect("pending")
            .is_empty()
    );
    assert!(
        PushQueueStorage::cas_published_version(&conn, 10)
            .await
            .expect("SQL CAS still works")
    );
    ensure_authz_snapshot_caught_up(&storage, Duration::ZERO)
        .await
        .expect("off barrier is a no-op");
    assert_eq!(storage.entity_store().local_published_version(), 0);
    assert!(!storage.entity_store().is_dirty());
}

#[tokio::test]
async fn tp22_dirty_outbox_skips_version_compare() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
    seed_cedar_on_main(&storage, &json).await;
    storage
        .entity_store()
        .swap(&json)
        .expect("baseline snapshot");
    let conn = storage.mono_storage().get_connection().clone();
    assert_eq!(
        PushQueueStorage::load_published_version(&conn)
            .await
            .expect("watermark"),
        0
    );

    mark_authz_dirty_and_compensate(&storage).await;
    let rows = authz_notify_outbox::Entity::find()
        .all(&conn)
        .await
        .expect("outbox");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].dirty);
    assert!(rows[0].version.is_none());
    assert!(rows[0].replayed_at.is_some());
    assert_eq!(
        PushQueueStorage::load_published_version(&conn)
            .await
            .expect("watermark"),
        0,
        "dirty compensate must not CAS published_version"
    );
    assert!(!storage.entity_store().is_dirty());
}

#[tokio::test]
async fn tp22_barrier_drains_pending_outbox_when_watermarks_match() {
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
    seed_cedar_on_main(&storage, &json).await;
    storage
        .entity_store()
        .swap(&json)
        .expect("stale local snapshot");
    assert_eq!(storage.entity_store().local_published_version(), 0);
    let conn = storage.mono_storage().get_connection().clone();
    PushQueueStorage::insert_authz_outbox(&conn, Some(9), false)
        .await
        .expect("outbox committed, notify never ran");

    ensure_authz_snapshot_caught_up(&storage, Duration::from_secs(5))
        .await
        .expect("barrier must drain pending outbox");
    assert_eq!(storage.entity_store().local_published_version(), 9);
    assert_eq!(
        PushQueueStorage::load_published_version(&conn)
            .await
            .expect("watermark"),
        9
    );
    assert!(
        PushQueueStorage::pending_authz_outbox(&conn)
            .await
            .expect("pending")
            .is_empty()
    );
}

struct AfterJsonLoadGuard;

impl Drop for AfterJsonLoadGuard {
    fn drop(&mut self) {
        set_after_authz_json_load_publish_for_test(None);
    }
}

#[tokio::test]
async fn tp22_catch_up_does_not_stamp_stale_tree_with_newer_watermark() {
    let _guard = AfterJsonLoadGuard;
    let temp = tempfile::tempdir().expect("temp");
    let mut storage = storage_enforce(temp.path()).await;
    let json_v10 = generate_entity(&["admin".to_string()], "v10").expect("generate");
    let json_v11 = generate_entity(&["admin".to_string()], "v11").expect("generate");
    seed_cedar_on_main(&storage, &json_v10).await;
    let conn = storage.mono_storage().get_connection().clone();
    assert!(
        PushQueueStorage::cas_published_version(&conn, 10)
            .await
            .expect("watermark 10")
    );
    storage.set_entity_store_for_test(Arc::new(SharedEntityStore::new()));
    set_after_authz_json_load_publish_for_test(Some((json_v11, 11)));
    ensure_authz_snapshot_caught_up(&storage, Duration::from_secs(5))
        .await
        .expect("barrier");
    assert_eq!(storage.entity_store().local_published_version(), 11);
    let snap = storage.entity_store().snapshot().expect("snapshot");
    assert!(
        snap.store()
            .contains_repository(&r#"Repository::"v11""#.parse().unwrap()),
        "catch-up must reload after a concurrent publish, not label the pre-load tree as v11"
    );
    assert!(
        !snap
            .store()
            .contains_repository(&r#"Repository::"v10""#.parse().unwrap()),
        "stale pre-load ACL must not remain labelled as the newer watermark"
    );
}

struct RebuildDelayGuard;

impl Drop for RebuildDelayGuard {
    fn drop(&mut self) {
        set_rebuild_delay_for_test(Duration::ZERO);
    }
}

#[tokio::test]
async fn tp22_read_barrier_timeout_covers_rebuild_io() {
    let _guard = RebuildDelayGuard;
    set_rebuild_delay_for_test(Duration::from_millis(400));
    let temp = tempfile::tempdir().expect("temp");
    let storage = storage_enforce(temp.path()).await;
    let conn = storage.mono_storage().get_connection().clone();
    assert!(
        PushQueueStorage::cas_published_version(&conn, 10)
            .await
            .expect("advance watermark")
    );
    let err = ensure_authz_snapshot_caught_up(&storage, Duration::from_millis(50))
        .await
        .expect_err("hung rebuild must fail-close at the deadline");
    assert!(err.to_string().contains("timed out"));
    assert!(storage.entity_store().is_dirty());
}
