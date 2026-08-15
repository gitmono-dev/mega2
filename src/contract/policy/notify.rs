//! Single-source authorization-change notification (UN-16).
//!
//! `notify_authz_changed` is the only place that triggers a rebuild of the
//! shared authorization snapshot after a write path advances the main branch.
//! Callers (merge funnel, import, receive-pack delete) carry the old/new
//! `/.mega_cedar.json` blob IDs; the comparison is O(1) and a no-op when the
//! blob is unchanged. The `Storage` handle was injected by UN-02; this module
//! only consumes it (zero assembly changes).

use git_internal::internal::object::tree::Tree;

use crate::{
    common::errors::MegaError, contract::policy::entitystore::MEGA_CEDAR_PATH,
    jupiter::storage::Storage,
};

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
