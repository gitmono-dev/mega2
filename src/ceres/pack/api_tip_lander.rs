//! Tip lander for storage-only / trunk API commits (plan-20260904 AW-01).
//!
//! After API create/save has persisted blob/tree/commit objects, call
//! [`land_api_tip_push`] to advance `refs/heads/main` at `path` through
//! MonoWriteQueue — the same tip authority as protocol
//! [`super::monorepo::Monorepo`] trunk finalize. Never creates `mega_cl` or
//! `refs/cl/*`.

use std::sync::Arc;

use crate::{
    callisto::sea_orm_active_enums::PushQueueKindEnum,
    ceres::api_service::cache::GitObjectCache,
    common::{errors::MegaError, utils::MEGA_BRANCH_NAME},
    jupiter::{
        service::push_queue_service::{
            EnqueueRequest, ExecuteOutcome, ExecuteRequest, PushExecContext, PushPayload,
            QueueWaitResult, push_operation_id,
        },
        storage::Storage,
    },
};

/// ADR-TP-18 / AW-01: surface NFF refusals with a client align hint.
pub(crate) fn trunk_nff_align_message(message: &str) -> String {
    const ALIGN: &str = "git fetch && git reset --hard origin/main";
    let needs_align =
        message.contains("non-fast-forward") || message.contains("push chain is broken");
    if needs_align && !message.contains(ALIGN) {
        format!("{message}; align with `{ALIGN}`")
    } else {
        message.to_owned()
    }
}

/// Follow `enqueue_and_wait` through B3 conflict requeues until a tip lands.
pub(crate) async fn follow_push_queue(
    storage: &Storage,
    git_object_cache: Arc<GitObjectCache>,
    mut wait: QueueWaitResult,
) -> Result<String, MegaError> {
    const MAX_ROUNDS: usize = 32;
    let ctx = PushExecContext {
        storage: storage.clone(),
        git_object_cache,
        pre_apply_enter_barrier: None,
        pre_apply_release_barrier: None,
    };
    for _ in 0..MAX_ROUNDS {
        match wait {
            QueueWaitResult::Replayed {
                landed_commit_id, ..
            } => {
                return landed_commit_id.ok_or_else(|| {
                    MegaError::Other("push replay missing landed_commit_id".into())
                });
            }
            QueueWaitResult::Abandoned { id } => {
                return Err(MegaError::Other(format!(
                    "push wait abandoned for push_queue id {id}"
                )));
            }
            QueueWaitResult::Rejected { id, message } => {
                return Err(MegaError::Other(format!(
                    "push rejected for push_queue id {id}: {}",
                    trunk_nff_align_message(&message)
                )));
            }
            QueueWaitResult::Ready { id } => {
                let outcome = storage
                    .push_queue_service
                    .execute_b3(
                        ExecuteRequest {
                            id,
                            ..Default::default()
                        },
                        None,
                        None,
                        Some(&ctx),
                    )
                    .await?;
                match outcome {
                    ExecuteOutcome::Done {
                        landed_commit_id, ..
                    } => return Ok(landed_commit_id),
                    ExecuteOutcome::Requeued { successor_id, .. } => {
                        wait = storage
                            .push_queue_service
                            .wait_and_claim(successor_id)
                            .await?;
                    }
                    ExecuteOutcome::ClaimLost { id } => {
                        wait = storage.push_queue_service.wait_and_claim(id).await?;
                    }
                    ExecuteOutcome::Failed { message, .. } => {
                        return Err(MegaError::Other(trunk_nff_align_message(&message)));
                    }
                    ExecuteOutcome::HardStopped { id } => {
                        return Err(MegaError::Other(format!(
                            "push hard-stopped for push_queue id {id}"
                        )));
                    }
                    ExecuteOutcome::BypassDetected { id } => {
                        return Err(MegaError::Other(format!(
                            "queue bypass detected for push_queue id {id}"
                        )));
                    }
                }
            }
        }
    }
    Err(MegaError::Other(
        "push follow exceeded max conflict requeue rounds".into(),
    ))
}

/// Advance path tip via MonoWriteQueue after objects are already persisted.
///
/// `path` must be a non-root monorepo path (B0). `payload` is typically N=1
/// for a single API create/save commit. Returns the landed commit id.
pub async fn land_api_tip_push(
    storage: &Storage,
    git_object_cache: Arc<GitObjectCache>,
    path: &str,
    old_id: &str,
    new_id: &str,
    requester: Option<String>,
    payload: &PushPayload,
) -> Result<String, MegaError> {
    let wait = storage
        .push_queue_service
        .enqueue_and_wait(EnqueueRequest {
            kind: PushQueueKindEnum::Push,
            operation_id: push_operation_id(old_id, new_id),
            path: path.to_owned(),
            old_id: old_id.to_owned(),
            new_id: new_id.to_owned(),
            requester,
            payload: payload.to_json(),
            ref_name: Some(MEGA_BRANCH_NAME.to_owned()),
            is_delete: false,
        })
        .await?;
    follow_push_queue(storage, git_object_cache, wait).await
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use git_internal::{
        hash::ObjectHash,
        internal::object::{
            commit::Commit,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
    use tempfile::TempDir;

    use super::{land_api_tip_push, trunk_nff_align_message};
    use crate::{
        callisto::{mega_cl, mega_refs},
        ceres::{api_service::cache::GitObjectCache, pack::materialize},
        common::utils::MEGA_BRANCH_NAME,
        config::{PushPolicy, testing::isolated_config},
        jupiter::{
            service::push_queue_service::PushPayload,
            storage::{Storage, base_storage::StorageConnector},
            tests::{test_storage_with_config, with_test_vault},
        },
    };

    fn blob_item(name: &str, hex: &str) -> TreeItem {
        TreeItem::new(
            TreeItemMode::Blob,
            ObjectHash::from_str(hex).unwrap(),
            name.to_string(),
        )
    }

    async fn trunk_storage(temp: &std::path::Path) -> Storage {
        let mut config = isolated_config(temp.join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        let storage = test_storage_with_config(temp, config).await;
        with_test_vault(storage, temp).await
    }

    async fn trunk_path_fixture(
        dir: &str,
        path_commit_msg: &str,
    ) -> (TempDir, Storage, Commit, String) {
        let temp = TempDir::new().expect("temp");
        let storage = trunk_storage(temp.path()).await;
        let (path_commit, path) = path_fixture_on(&storage, dir, path_commit_msg).await;
        (temp, storage, path_commit, path)
    }

    /// Root + `main@/{dir}` whose tree is `child` and tip is `path_commit`,
    /// seeded onto an existing storage.
    async fn path_fixture_on(
        storage: &Storage,
        dir: &str,
        path_commit_msg: &str,
    ) -> (Commit, String) {
        let mono = storage.mono_storage();
        let child = Tree::from_tree_items(vec![blob_item(
            "x.txt",
            "dddddddddddddddddddddddddddddddddddddddd",
        )])
        .expect("child");
        let root_tree = Tree::from_tree_items(vec![
            blob_item(".gitkeep", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            TreeItem::new(TreeItemMode::Tree, child.id, dir.to_string()),
        ])
        .expect("root");
        let root_commit = Commit::from_tree_id(root_tree.id, vec![], "root");
        let path_commit = Commit::from_tree_id(child.id, vec![], path_commit_msg);
        mono.save_mega_trees(vec![child.clone(), root_tree.clone()], root_commit.id, None)
            .await
            .unwrap();
        mono.save_mega_commits(vec![root_commit.clone(), path_commit.clone()], None)
            .await
            .unwrap();
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                root_commit.id.to_string(),
                root_tree.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        let path = format!("/{dir}");
        mono.save_refs(
            mega_refs::Model::new(
                path.clone(),
                MEGA_BRANCH_NAME.to_owned(),
                path_commit.id.to_string(),
                child.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .unwrap();
        (path_commit, path)
    }

    async fn git_cache() -> Arc<GitObjectCache> {
        Arc::new(GitObjectCache {
            connection: crate::jupiter::tests::test_redis_manager().await,
            prefix: String::new(),
        })
    }

    #[tokio::test]
    async fn api_tip_lander_advances_path_tip() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, path_commit, path) = trunk_path_fixture("aw01ok", "path tip").await;
        let new_child = Tree::from_tree_items(vec![blob_item(
            "y.txt",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        )])
        .unwrap();
        let new_commit = Commit::from_tree_id(new_child.id, vec![path_commit.id], "api n1");
        storage
            .mono_storage()
            .save_mega_trees(vec![new_child], new_commit.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![new_commit.clone()], None)
            .await
            .unwrap();
        let old_id = path_commit.id.to_string();
        let new_id = new_commit.id.to_string();
        let payload = PushPayload {
            commits: vec![new_id.clone()],
            fork_base: Some(old_id.clone()),
            n: 1,
        };
        let landed = land_api_tip_push(
            &storage,
            git_cache().await,
            &path,
            &old_id,
            &new_id,
            Some("agent-ci".into()),
            &payload,
        )
        .await
        .expect("lander advances tip");
        assert_eq!(landed, new_id);
        let pref = storage
            .mono_storage()
            .get_main_ref(&path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pref.ref_commit_hash, new_id);
        let cl_refs = mega_refs::Entity::find()
            .filter(mega_refs::Column::IsCl.eq(true))
            .count(storage.mono_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(cl_refs, 0, "lander must not write refs/cl/*");
        let cl_count = mega_cl::Entity::find()
            .count(storage.mono_storage().get_connection())
            .await
            .unwrap();
        assert_eq!(cl_count, 0, "lander must not create mega_cl");
    }

    #[tokio::test]
    async fn api_tip_lander_nff_is_diagnosable() {
        let _lock = materialize::lock_materialize_tests().await;
        let (_temp, storage, path_commit, path) = trunk_path_fixture("aw01nff", "path tip").await;
        let first_child = Tree::from_tree_items(vec![blob_item(
            "a.txt",
            "ffffffffffffffffffffffffffffffffffffffff",
        )])
        .unwrap();
        let first = Commit::from_tree_id(first_child.id, vec![path_commit.id], "first");
        storage
            .mono_storage()
            .save_mega_trees(vec![first_child], first.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![first.clone()], None)
            .await
            .unwrap();
        let stale_old = path_commit.id.to_string();
        let first_id = first.id.to_string();
        land_api_tip_push(
            &storage,
            git_cache().await,
            &path,
            &stale_old,
            &first_id,
            None,
            &PushPayload {
                commits: vec![first_id.clone()],
                fork_base: Some(stale_old.clone()),
                n: 1,
            },
        )
        .await
        .expect("first land");

        let sibling_child = Tree::from_tree_items(vec![blob_item(
            "b.txt",
            "1111111111111111111111111111111111111111",
        )])
        .unwrap();
        let sibling = Commit::from_tree_id(sibling_child.id, vec![path_commit.id], "sibling");
        storage
            .mono_storage()
            .save_mega_trees(vec![sibling_child], sibling.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![sibling.clone()], None)
            .await
            .unwrap();
        let sibling_id = sibling.id.to_string();
        let err = land_api_tip_push(
            &storage,
            git_cache().await,
            &path,
            &stale_old,
            &sibling_id,
            None,
            &PushPayload {
                commits: vec![sibling_id.clone()],
                fork_base: Some(stale_old.clone()),
                n: 1,
            },
        )
        .await
        .expect_err("stale old_id must NFF");
        let msg = err.to_string();
        assert!(msg.contains("non-fast-forward"), "{msg}");
        assert!(
            msg.contains("git fetch && git reset --hard origin/main"),
            "{msg}"
        );
    }

    #[test]
    fn trunk_nff_align_appends_once() {
        let raw = "non-fast-forward: ref_commit_hash does not match old_id";
        let once = trunk_nff_align_message(raw);
        assert!(once.contains("align with"));
        let twice = trunk_nff_align_message(&once);
        assert_eq!(once, twice);
    }

    /// WH-03 (plan-20260912 / AC3): API writes land through the same B3
    /// commit point as protocol pushes, so `land_api_tip_push` produces
    /// exactly one `repo.push` event with this round's landed snapshot.
    #[tokio::test]
    async fn storage_event_api_commit() {
        use crate::jupiter::service::{
            storage_event_emitter::repo_push_event,
            storage_event_transport::{EventTarget, EventTransport, TransportSuccess},
        };

        /// Recording fake transport: exact body bytes + call counter, no
        /// network (WH-09 test seam).
        #[derive(Default)]
        struct RecordingTransport {
            calls: std::sync::atomic::AtomicUsize,
            bodies: std::sync::Mutex<Vec<bytes::Bytes>>,
        }

        impl EventTransport for RecordingTransport {
            fn post(
                &self,
                _target: &EventTarget,
                body: bytes::Bytes,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                TransportSuccess,
                                crate::jupiter::service::storage_event_transport::TransportError,
                            >,
                        > + Send,
                >,
            > {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.bodies.lock().expect("bodies").push(body);
                Box::pin(async { Ok(TransportSuccess::Accepted2xx { status: 200 }) })
            }
        }

        const INSTALLATION: &str = "it-wh03-api";
        let _lock = materialize::lock_materialize_tests().await;
        let temp = TempDir::new().expect("temp");
        let mut config = isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some(INSTALLATION.to_string());
        let transport = Arc::new(RecordingTransport::default());
        let target_config = crate::config::StorageEventsTargetConfig {
            id: "ops-main".to_string(),
            url: "https://events.example.invalid/ingest".to_string(),
            secret_ref: "vault://secret/config/it/storage_events/targets/ops-main/hmac#value"
                .to_string(),
            events: vec!["repo.push".to_string()],
            git_paths: vec!["/".to_string()],
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: false,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        };
        let secret = crate::config::secret::SecretString::new(
            "hex:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let compiled =
            EventTarget::compile(&target_config.id, &target_config.url, &secret).expect("target");
        let mut storage = test_storage_with_config(temp.path(), config.clone()).await;
        storage.set_storage_event_emitter(
            crate::jupiter::service::storage_event_emitter::StorageEventEmitter::new_with_transport(
                &config,
                transport.clone(),
                vec![(target_config, compiled)],
            ),
        );
        let storage = with_test_vault(storage, temp.path()).await;

        let (path_commit, path) = path_fixture_on(&storage, "wh03api", "path tip").await;
        let new_child = Tree::from_tree_items(vec![blob_item(
            "y.txt",
            "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        )])
        .unwrap();
        let new_commit = Commit::from_tree_id(new_child.id, vec![path_commit.id], "api n1");
        storage
            .mono_storage()
            .save_mega_trees(vec![new_child], new_commit.id, None)
            .await
            .unwrap();
        storage
            .mono_storage()
            .save_mega_commits(vec![new_commit.clone()], None)
            .await
            .unwrap();
        let old_id = path_commit.id.to_string();
        let new_id = new_commit.id.to_string();
        let landed = land_api_tip_push(
            &storage,
            git_cache().await,
            &path,
            &old_id,
            &new_id,
            Some("agent-ci".into()),
            &PushPayload {
                commits: vec![new_id.clone()],
                fork_base: Some(old_id.clone()),
                n: 1,
            },
        )
        .await
        .expect("lander advances tip");
        assert_eq!(landed, new_id);

        // The emitter admits synchronously at the B3 commit point; the send
        // task lands within a bounded window.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if transport.calls.load(std::sync::atomic::Ordering::SeqCst) >= 1 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delivery within 2s");
        let bodies = transport.bodies.lock().expect("bodies").clone();
        assert_eq!(bodies.len(), 1, "exactly one repo.push delivery");
        let envelope: serde_json::Value =
            serde_json::from_slice(&bodies[0]).expect("envelope json");
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["event_type"], "repo.push");
        assert_eq!(envelope["source"], "git");
        assert_eq!(envelope["scope"]["repo_path"], path.as_str());
        assert!(envelope["scope"]["tenant_id"].is_null());
        assert!(envelope["scope"]["oci_repository"].is_null());
        let operation_id =
            crate::jupiter::service::push_queue_service::push_operation_id(&old_id, &new_id);
        assert_eq!(envelope["data"]["operation_id"], operation_id);
        assert_eq!(envelope["data"]["ref_name"], MEGA_BRANCH_NAME);
        assert_eq!(envelope["data"]["old_oid"], old_id);
        assert_eq!(envelope["data"]["requested_oid"], new_id);
        assert_eq!(envelope["data"]["landed_oid"], landed);
        let push_id = envelope["data"]["push_id"]
            .as_str()
            .expect("push_id string")
            .to_string();
        assert!(
            push_id.parse::<i64>().is_ok(),
            "push_id carries the push_queue row id: {push_id}"
        );
        let expected = repo_push_event(
            INSTALLATION,
            &path,
            crate::jupiter::service::storage_event_emitter::RepoPushData {
                push_id: push_id.clone(),
                operation_id: operation_id.clone(),
                ref_name: MEGA_BRANCH_NAME.to_owned(),
                old_oid: old_id.clone(),
                requested_oid: new_id.clone(),
                landed_oid: landed.clone(),
            },
        )
        .expect("builder");
        assert_eq!(envelope["event_id"], expected.event_id.to_string());

        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            storage.storage_event_emitter.shutdown(),
        )
        .await
        .expect("emitter shutdown within 3s");
        // Drain-first final count: exactly one delivery, no late extra.
        assert_eq!(
            transport.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one repo.push delivery after drain"
        );
    }
}
