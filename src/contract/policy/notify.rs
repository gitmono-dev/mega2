//! Single-source authorization-change notification (UN-16).
//!
//! `notify_authz_changed` is the only place that triggers a rebuild of the
//! shared authorization snapshot after a write path advances the main branch.
//! Callers (merge funnel, import, receive-pack delete) carry the old/new
//! `/.mega_cedar.json` blob IDs; the comparison is O(1) and a no-op when the
//! blob is unchanged. The `Storage` handle was injected by UN-02; this module
//! only consumes it (zero assembly changes).
//!
//! When `cedar.enforcement` builds a store (shadow/enforce), TP-22 adds a
//! persistent outbox, monotonic `published_version` CAS, and a per-instance
//! read barrier. `off` (including trunk) bypasses that trio and keeps the
//! in-memory blob-id notify path.

#[cfg(test)]
use std::str::FromStr;
#[cfg(test)]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use git_internal::internal::object::tree::Tree;
#[cfg(test)]
use git_internal::{
    hash::ObjectHash,
    internal::object::tree::{TreeItem, TreeItemMode},
};
use sea_orm::DatabaseTransaction;
#[cfg(test)]
use sea_orm::{ConnectionTrait, DbBackend, Statement, Value};

use crate::{
    common::errors::MegaError,
    contract::policy::{enforcement::Enforcement, entitystore::MEGA_CEDAR_PATH},
    jupiter::{
        storage::{Storage, base_storage::StorageConnector, push_queue_storage::PushQueueStorage},
        utils::converter::FromMegaModel,
    },
};

/// UN-19 fail-closed wait while this process rebuilds a lagging snapshot.
pub const AUTHZ_BARRIER_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
static REBUILD_DELAY: Mutex<Duration> = Mutex::new(Duration::ZERO);

/// Test-only: after the current `load_latest_authz_json` returns, replace
/// main's `/.mega_cedar.json` and CAS `published_version` so catch-up can
/// prove it stamps the pre-load watermark (not a watermark advanced during
/// the load).
#[cfg(test)]
static AFTER_JSON_LOAD_PUBLISH: Mutex<Option<(String, i64)>> = Mutex::new(None);

/// Test-only: delay `load_latest_authz_json` so the barrier timeout can fire
/// while rebuild I/O is in flight.
#[cfg(test)]
pub fn set_rebuild_delay_for_test(delay: Duration) {
    *REBUILD_DELAY.lock().expect("rebuild delay lock") = delay;
}

#[cfg(test)]
pub fn set_after_authz_json_load_publish_for_test(publish: Option<(String, i64)>) {
    *AFTER_JSON_LOAD_PUBLISH
        .lock()
        .expect("after-json-load lock") = publish;
}

fn barrier_timeout_error() -> MegaError {
    MegaError::Other("authorization snapshot rebuild timed out".into())
}

fn emit_barrier_timeout(db_version: i64, local: i64, dirty: bool) {
    tracing::error!(
        event = "authz_barrier_timeout",
        db_version,
        local_version = local,
        dirty,
        "authorization snapshot barrier timed out; fail-closed"
    );
}

/// Resolve the `/.mega_cedar.json` blob ID from a tree, if the file is
/// present. `None` means the file is absent from that tree.
pub fn authz_blob_id(tree: &Tree) -> Option<String> {
    let file_name = MEGA_CEDAR_PATH.trim_start_matches('/');
    tree.tree_items
        .iter()
        .find(|item| item.name == file_name)
        .map(|item| item.id.to_string())
}

/// Notify that the main branch's `/.mega_cedar.json` may have changed.
///
/// The caller carries the old/new blob IDs. When they are equal (or both
/// absent) this is a no-op. When the blob changed, the new content is read
/// from object storage and the shared snapshot is rebuilt (build-then-swap; a
/// failed rebuild keeps the old snapshot and sets the dirty flag). When the
/// file was deleted (`new_blob_id` is `None`), there is no content to build,
/// so the snapshot is marked dirty (fail-closed).
pub async fn notify_authz_changed(
    storage: &Storage,
    old_blob_id: Option<&str>,
    new_blob_id: Option<&str>,
) -> Result<(), MegaError> {
    if old_blob_id == new_blob_id {
        return Ok(());
    }
    let Some(new_blob_id) = new_blob_id else {
        // The authz file was deleted from main: no content to build. Mark the
        // shared snapshot dirty so enforce mode rejects all (fail-closed).
        storage.entity_store().mark_dirty();
        return Ok(());
    };
    let content = match storage.git_service.get_object_as_bytes(new_blob_id).await {
        Ok(c) => c,
        Err(e) => {
            // The main ref advanced but the new authz content cannot be read:
            // the snapshot is stale — mark dirty (fail-closed).
            storage.entity_store().mark_dirty();
            return Err(e);
        }
    };
    let json = match String::from_utf8(content) {
        Ok(j) => j,
        Err(e) => {
            storage.entity_store().mark_dirty();
            return Err(MegaError::Other(format!(
                "authz file UTF-8 decode failed: {e}"
            )));
        }
    };
    // `swap` is build-then-swap: on failure it keeps the old snapshot, sets
    // dirty, and logs `event=authz_rebuild_failed`.
    storage
        .entity_store()
        .swap(&json)
        .map_err(|e| MegaError::Other(format!("authorization snapshot rebuild failed: {e}")))?;
    Ok(())
}

/// Best-effort notify for post-commit hook points (merge funnel, import,
/// receive-pack delete). The write is already committed, so a notify failure
/// must not fail the operation; `notify_authz_changed` marks the snapshot
/// dirty (fail-closed) on any failure, and this logs and continues.
pub async fn notify_authz_changed_best_effort(
    storage: &Storage,
    old_blob_id: Option<&str>,
    new_blob_id: Option<&str>,
) {
    if let Err(e) = notify_authz_changed(storage, old_blob_id, new_blob_id).await {
        tracing::warn!(
            event = "authz_notify_failed",
            path = MEGA_CEDAR_PATH,
            error = %e,
            "authorization notify failed after committed write; snapshot marked dirty (fail-closed)"
        );
    }
}

/// Whether TP-22 outbox / CAS / read-barrier are active.
pub fn authz_barrier_enabled(storage: &Storage) -> bool {
    Enforcement::parse(&storage.config().cedar.enforcement)
        .unwrap_or(Enforcement::Off)
        .builds()
}

/// B3 same-txn outbox insert when the trio is enabled; no-op when `off`.
pub async fn insert_b3_authz_outbox_if_builds(
    storage: &Storage,
    txn: &DatabaseTransaction,
    queue_id: i64,
) -> Result<(), MegaError> {
    if !authz_barrier_enabled(storage) {
        return Ok(());
    }
    PushQueueStorage::insert_authz_outbox(txn, Some(queue_id), false).await
}

/// After a B3 commit: replay the outbox (builds) or fall back to blob-id notify (`off`).
pub async fn after_b3_commit_authz(
    storage: &Storage,
    old_blob_id: Option<&str>,
    new_blob_id: Option<&str>,
) {
    if authz_barrier_enabled(storage) {
        replay_authz_outbox_best_effort(storage).await;
    } else {
        notify_authz_changed_best_effort(storage, old_blob_id, new_blob_id).await;
    }
}

/// Queue-external delete path: dirty mark + unversioned outbox + compensate.
/// No `published_version` compare. No-op when the trio is bypassed.
pub async fn mark_authz_dirty_and_compensate(storage: &Storage) {
    if !authz_barrier_enabled(storage) {
        return;
    }
    storage.entity_store().mark_dirty();
    let conn = storage.mono_storage().get_connection().clone();
    if let Err(e) = PushQueueStorage::insert_authz_outbox(&conn, None, true).await {
        tracing::warn!(
            event = "authz_dirty_outbox_insert_failed",
            error = %e,
            "failed to persist dirty authz outbox; snapshot already marked dirty (fail-closed)"
        );
        return;
    }
    replay_authz_outbox_best_effort(storage).await;
}

pub async fn replay_authz_outbox_best_effort(storage: &Storage) {
    if let Err(e) = replay_authz_outbox(storage).await {
        storage.entity_store().mark_dirty();
        tracing::warn!(
            event = "authz_outbox_replay_failed",
            path = MEGA_CEDAR_PATH,
            error = %e,
            "authz outbox replay failed; snapshot marked dirty (fail-closed)"
        );
    }
}

/// Replay pending outbox rows: rebuild from the latest root, then CAS-publish
/// versioned rows. Dirty rows skip the version compare.
pub async fn replay_authz_outbox(storage: &Storage) -> Result<(), MegaError> {
    let conn = storage.mono_storage().get_connection().clone();
    let pending = PushQueueStorage::pending_authz_outbox(&conn).await?;
    for row in pending {
        if row.dirty {
            refresh_from_latest_root(storage).await?;
        } else if let Some(version) = row.version {
            publish_from_latest_root(storage, version).await?;
        } else {
            rebuild_from_latest_root(storage).await?;
        }
        PushQueueStorage::mark_authz_outbox_replayed(&conn, row.id).await?;
    }
    Ok(())
}

/// UN-19 read barrier: this process must not decide against a snapshot older
/// than DB `published_version`. Rebuild from the latest root until caught up
/// or `timeout` elapses (fail-closed + alert).
pub async fn ensure_authz_snapshot_caught_up(
    storage: &Storage,
    timeout: Duration,
) -> Result<(), MegaError> {
    if !authz_barrier_enabled(storage) {
        return Ok(());
    }
    let conn = storage.mono_storage().get_connection().clone();
    let deadline = Instant::now() + timeout;
    loop {
        let db_version = PushQueueStorage::load_published_version(&conn).await?;
        let local = storage.entity_store().local_published_version();
        let dirty = storage.entity_store().is_dirty();
        let pending = PushQueueStorage::pending_authz_outbox(&conn).await?;
        if local >= db_version && !dirty && pending.is_empty() {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            emit_barrier_timeout(db_version, local, dirty);
            storage.entity_store().mark_dirty();
            return Err(barrier_timeout_error());
        }
        // Drain committed-but-unpublished outbox first. A B3 crash after
        // commit leaves `published_version` unchanged, so watermark compare
        // alone would skip rebuild and UN-19 would decide on a stale snapshot.
        match tokio::time::timeout(remaining, replay_authz_outbox(storage)).await {
            Err(_) => {
                emit_barrier_timeout(db_version, local, dirty);
                storage.entity_store().mark_dirty();
                return Err(barrier_timeout_error());
            }
            Ok(Err(e)) => return Err(e),
            Ok(Ok(())) => {}
        }
        let db_version = PushQueueStorage::load_published_version(&conn).await?;
        let local = storage.entity_store().local_published_version();
        let dirty = storage.entity_store().is_dirty();
        if local >= db_version && !dirty {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            emit_barrier_timeout(db_version, local, dirty);
            storage.entity_store().mark_dirty();
            return Err(barrier_timeout_error());
        }
        match tokio::time::timeout(remaining, catch_up_from_latest_root(storage, db_version)).await
        {
            Err(_) => {
                emit_barrier_timeout(db_version, local, dirty);
                storage.entity_store().mark_dirty();
                return Err(barrier_timeout_error());
            }
            Ok(Err(e)) => return Err(e),
            Ok(Ok(())) => {}
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn catch_up_from_latest_root(storage: &Storage, db_version: i64) -> Result<(), MegaError> {
    // Stamp the watermark observed *before* this root read. A concurrent
    // B3 may advance the DB watermark during `load_latest_authz_json`;
    // labelling the older tree with that newer watermark would make the
    // next barrier treat a stale snapshot as current (TP-22 AC3).
    //
    // When this process is already at `db_version` but dirty, `swap_at_version`
    // would no-op (`local >= stamp`) and leave dirty set — the barrier would
    // then spin until timeout. Refresh content at the current watermark instead.
    if storage.entity_store().local_published_version() >= db_version {
        refresh_from_latest_root(storage).await
    } else {
        install_latest_root_at(storage, db_version).await
    }
}

async fn rebuild_from_latest_root(storage: &Storage) -> Result<(), MegaError> {
    let conn = storage.mono_storage().get_connection().clone();
    let db_version = PushQueueStorage::load_published_version(&conn).await?;
    install_latest_root_at(storage, db_version).await
}

async fn install_latest_root_at(storage: &Storage, stamp: i64) -> Result<(), MegaError> {
    match load_latest_authz_json(storage).await? {
        None => {
            storage.entity_store().mark_dirty();
            Ok(())
        }
        Some(json) => if stamp > 0 {
            storage.entity_store().swap_at_version(&json, stamp)
        } else {
            storage.entity_store().swap(&json)
        }
        .map_err(|e| MegaError::Other(format!("authorization snapshot rebuild failed: {e}"))),
    }
}

/// Queue-external dirty path: refresh snapshot content from the latest root
/// without a version CAS. Load and install are not one lock; the watermark
/// check and content swap *are* (`swap_if_watermark_eq`). If a versioned
/// publish wins in between, reload instead of installing a stale tree over
/// the newer snapshot.
async fn refresh_from_latest_root(storage: &Storage) -> Result<(), MegaError> {
    for _ in 0..32 {
        let expected = storage.entity_store().local_published_version();
        match load_latest_authz_json(storage).await? {
            None => {
                storage.entity_store().mark_dirty();
                return Ok(());
            }
            Some(json) => {
                let applied = storage
                    .entity_store()
                    .swap_if_watermark_eq(&json, expected)
                    .map_err(|e| {
                        MegaError::Other(format!("authorization snapshot rebuild failed: {e}"))
                    })?;
                if applied {
                    return Ok(());
                }
            }
        }
    }
    Err(MegaError::Other(
        "dirty authz refresh lost to concurrent versioned publishes".into(),
    ))
}

async fn publish_from_latest_root(storage: &Storage, version: i64) -> Result<(), MegaError> {
    let Some(json) = load_latest_authz_json(storage).await? else {
        storage.entity_store().mark_dirty();
        return Ok(());
    };
    let conn = storage.mono_storage().get_connection().clone();
    let won = PushQueueStorage::cas_published_version(&conn, version).await?;
    if !won {
        tracing::info!(
            event = "authz_publish_rejected",
            version,
            "stale authz snapshot publish lost published_version CAS"
        );
        return Ok(());
    }
    // Local swap is also monotonic: a CAS-winning older id that resumes after
    // a newer swap cannot overwrite the newer snapshot (TP-22 CAS+swap race).
    storage
        .entity_store()
        .swap_at_version(&json, version)
        .map_err(|e| MegaError::Other(format!("authorization snapshot rebuild failed: {e}")))?;
    Ok(())
}

async fn load_latest_authz_json(storage: &Storage) -> Result<Option<String>, MegaError> {
    #[cfg(test)]
    {
        let delay = *REBUILD_DELAY.lock().expect("rebuild delay lock");
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
    let Some(root) = storage.mono_storage().get_main_ref("/").await? else {
        return Ok(None);
    };
    let Some(tree) = storage
        .mono_storage()
        .get_tree_by_hash(&root.ref_tree_hash)
        .await?
    else {
        return Ok(None);
    };
    let Some(blob_id) = authz_blob_id(&Tree::from_mega_model(tree)) else {
        return Ok(None);
    };
    let content = match storage.git_service.get_object_as_bytes(&blob_id).await {
        Ok(c) => c,
        Err(e) => {
            storage.entity_store().mark_dirty();
            return Err(e);
        }
    };
    match String::from_utf8(content) {
        Ok(json) => {
            #[cfg(test)]
            apply_after_json_load_publish(storage).await?;
            Ok(Some(json))
        }
        Err(e) => {
            storage.entity_store().mark_dirty();
            Err(MegaError::Other(format!(
                "authz file UTF-8 decode failed: {e}"
            )))
        }
    }
}

#[cfg(test)]
async fn apply_after_json_load_publish(storage: &Storage) -> Result<(), MegaError> {
    let Some((json, version)) = AFTER_JSON_LOAD_PUBLISH
        .lock()
        .expect("after-json-load lock")
        .take()
    else {
        return Ok(());
    };
    let blob_id = storage
        .git_service
        .save_object_from_raw(bytes::Bytes::from(json))
        .await?;
    let blob_hash = ObjectHash::from_str(&blob_id)
        .map_err(|e| MegaError::Other(format!("authz test blob hash: {e}")))?;
    let tree = Tree::from_tree_items(vec![TreeItem::new(
        TreeItemMode::Blob,
        blob_hash,
        ".mega_cedar.json".to_string(),
    )])
    .map_err(|e| MegaError::Other(format!("authz test tree: {e}")))?;
    let Some(root) = storage.mono_storage().get_main_ref("/").await? else {
        return Ok(());
    };
    let commit_id = ObjectHash::from_str(&root.ref_commit_hash)
        .map_err(|e| MegaError::Other(format!("authz test commit hash: {e}")))?;
    storage
        .mono_storage()
        .save_mega_trees(vec![tree.clone()], commit_id, None)
        .await?;
    let conn = storage.mono_storage().get_connection().clone();
    conn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mega_refs SET ref_tree_hash = $1, updated_at = now() \
         WHERE path = '/' AND ref_name = $2",
        [
            Value::from(tree.id.to_string()),
            Value::from(crate::common::utils::MEGA_BRANCH_NAME.to_owned()),
        ],
    ))
    .await?;
    PushQueueStorage::cas_published_version(&conn, version).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use git_internal::{
        hash::ObjectHash,
        internal::object::tree::{TreeItem, TreeItemMode},
    };

    use super::*;
    use crate::{contract::policy::entitystore::generate_entity, jupiter::storage::Storage};

    fn blob_item(name: &str, hex: &str) -> TreeItem {
        TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str(hex).unwrap(),
            name.to_string(),
        )
    }

    #[test]
    fn authz_blob_id_finds_file_in_root_tree() {
        // A tree whose only item is the authz file.
        let tree = Tree::from_tree_items(vec![blob_item(
            MEGA_CEDAR_PATH.trim_start_matches('/'),
            "0123456789abcdef0123456789abcdef01234567",
        )])
        .unwrap();
        let id = authz_blob_id(&tree).unwrap();
        assert_eq!(id, "0123456789abcdef0123456789abcdef01234567");
    }

    #[test]
    fn authz_blob_id_absent_when_file_missing() {
        let tree = Tree::from_tree_items(vec![blob_item(
            "README.md",
            "0123456789abcdef0123456789abcdef01234567",
        )])
        .unwrap();
        assert!(authz_blob_id(&tree).is_none());
    }

    // --- UN-16: notify single-source + handle consumption (Storage::mock) ---

    async fn save_authz_json(storage: &Storage, json: &str) -> String {
        storage
            .git_service
            .save_object_from_raw(bytes::Bytes::from(json.to_string()))
            .await
            .expect("save authz blob to mock object storage")
    }

    #[tokio::test]
    async fn notify_rebuilds_snapshot_when_blob_changed() {
        let storage = Storage::mock();
        let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
        let blob_id = save_authz_json(&storage, &json).await;

        assert!(storage.entity_store().snapshot().is_none());
        notify_authz_changed(&storage, None, Some(&blob_id))
            .await
            .expect("notify should succeed");
        assert!(!storage.entity_store().is_dirty());
        let snap = storage.entity_store().snapshot().expect("snapshot built");
        assert!(
            snap.store()
                .contains_repository(&r#"Repository::"repo""#.parse().unwrap()),
            "snapshot should reflect the new authz content"
        );
    }

    #[tokio::test]
    async fn notify_is_noop_when_blob_unchanged() {
        let storage = Storage::mock();
        let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
        let blob_id = save_authz_json(&storage, &json).await;

        notify_authz_changed(&storage, None, Some(&blob_id))
            .await
            .expect("first notify");
        let before = storage.entity_store().snapshot().expect("snapshot");
        // Same blob ID on both sides: O(1) comparison short-circuits, no rebuild.
        notify_authz_changed(&storage, Some(&blob_id), Some(&blob_id))
            .await
            .expect("noop notify");
        let after = storage.entity_store().snapshot().expect("snapshot");
        assert!(
            Arc::ptr_eq(&before, &after),
            "unchanged blob must not rebuild the snapshot"
        );
        assert!(!storage.entity_store().is_dirty());
    }

    #[tokio::test]
    async fn notify_marks_dirty_when_blob_deleted() {
        let storage = Storage::mock();
        let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
        let blob_id = save_authz_json(&storage, &json).await;
        notify_authz_changed(&storage, None, Some(&blob_id))
            .await
            .expect("first notify");
        assert!(!storage.entity_store().is_dirty());

        // `/.mega_cedar.json` deleted from main: no content to build → dirty (fail-closed).
        notify_authz_changed(&storage, Some(&blob_id), None)
            .await
            .expect("delete notify is Ok");
        assert!(storage.entity_store().is_dirty());
    }

    #[tokio::test]
    async fn notify_marks_dirty_on_read_failure() {
        let storage = Storage::mock();
        // A well-formed hex id that does not exist in object storage.
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let err = notify_authz_changed(&storage, None, Some(missing))
            .await
            .expect_err("missing blob must fail");
        assert!(storage.entity_store().is_dirty(), "read failure → dirty");
        assert!(!err.to_string().is_empty(), "error must carry a message");
    }

    #[tokio::test]
    async fn notify_marks_dirty_on_invalid_json() {
        let storage = Storage::mock();
        let blob_id = save_authz_json(&storage, "not-json").await;
        let err = notify_authz_changed(&storage, None, Some(&blob_id))
            .await
            .expect_err("invalid json must fail");
        assert!(storage.entity_store().is_dirty(), "rebuild failure → dirty");
        assert!(err.to_string().contains("rebuild failed"));
    }

    #[tokio::test]
    async fn notify_after_guard_reads_same_instance() {
        // Object identity: after notify, the read path (guard/push) observes the
        // same Arc<EntitySnapshot> instance across reads — no per-request rebuild.
        let storage = Storage::mock();
        let json = generate_entity(&["admin".to_string()], "repo").expect("generate");
        let blob_id = save_authz_json(&storage, &json).await;
        notify_authz_changed(&storage, None, Some(&blob_id))
            .await
            .expect("notify");

        let a = storage.entity_store().snapshot().expect("a");
        let b = storage.entity_store().snapshot().expect("b");
        assert!(
            Arc::ptr_eq(&a, &b),
            "guard/push must read the same instance"
        );
        assert!(
            a.store()
                .contains_repository(&r#"Repository::"repo""#.parse().unwrap())
        );
    }
}
