//! Target path lifecycle. Callers pass the connection that owns the code write
//! when they already have a transaction.

use std::collections::BTreeMap;

use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseBackend, DatabaseTransaction, EntityTrait, Set,
    Statement, TransactionTrait,
};

use crate::{
    callisto::trace_target,
    common::errors::MegaError,
    jupiter::storage::{
        base_storage::StorageConnector,
        mono_storage::MonoStorage,
        trace_target_storage::{self, Model, TARGET_KIND_NATIVE},
    },
};

/// A live binding that has never been retired.
pub const INITIAL_BINDING_GENERATION: i64 = 1;

/// The import target retired inside a detach transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredImportTarget {
    pub id: String,
    pub binding_generation: i64,
}

pub async fn bind_first_native_write<C: ConnectionTrait>(
    conn: &C,
    path: &str,
) -> Result<Model, MegaError> {
    trace_target_storage::bind_first_write(conn, TARGET_KIND_NATIVE, path).await
}

pub async fn move_native_target<C>(conn: &C, from: &str, to: &str) -> Result<Model, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = DatabaseTransaction>,
{
    trace_target_storage::move_active_tree(conn, from, to).await
}

pub async fn delete_native_target<C>(conn: &C, path: &str) -> Result<Vec<String>, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = DatabaseTransaction>,
{
    trace_target_storage::deactivate_tree(conn, path).await
}

pub async fn rebind_if_path_exists<C: ConnectionTrait>(
    conn: &C,
    target_id: &str,
    new_path: &str,
    path_exists: bool,
) -> Result<Model, MegaError> {
    trace_target_storage::rebind_existing_path(conn, target_id, new_path, path_exists).await
}

pub fn identity_for_same_tree_oid(target: &Model, tree_oid: &str) -> Model {
    trace_target_storage::unchanged_for_tree_oid(target, tree_oid)
}

/// Insert a native binding only when this call creates the row.
/// `None` means another active row already owns the path, including a row
/// inserted between the lookup and this insert. A later failed commit must
/// not retire that other row.
pub async fn bind_new_native_directory<C: ConnectionTrait>(
    conn: &C,
    path: &str,
) -> Result<Option<String>, MegaError> {
    if trace_target_storage::active_at(conn, path).await?.is_some() {
        return Ok(None);
    }
    match trace_target_storage::insert_active(
        conn,
        trace_target_storage::TARGET_KIND_NATIVE,
        path,
        None,
    )
    .await
    {
        Ok(row) => Ok(Some(row.id)),
        Err(err) if err.to_string().contains("trace_target_one_active_path") => Ok(None),
        Err(err) => Err(err),
    }
}

/// A borrowed transaction keeps this fence until its owner commits or rolls
/// back. Errors, including unique conflicts, must abort the owner's write.
pub(crate) async fn bind_if_still_landed_in_txn(
    mono: &MonoStorage,
    txn: &DatabaseTransaction,
    path: &str,
) -> Result<Option<String>, MegaError> {
    let path = trace_target_storage::canonical_project_path(path)?;
    let Some(root) = mono.lock_native_root_in_txn(txn).await? else {
        return Ok(None);
    };
    if mono
        .resolve_path_tree_hash_in_txn(&root.ref_tree_hash, &path, txn)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let row = trace_target_storage::insert_active(txn, TARGET_KIND_NATIVE, &path, None).await?;
    Ok(Some(row.id))
}

pub(crate) async fn bind_if_still_landed(mono: &MonoStorage, path: &str) -> Result<(), MegaError> {
    let txn = mono.get_connection().begin().await?;
    match bind_if_still_landed_in_txn(mono, &txn, path).await {
        Ok(Some(_)) => {
            txn.commit().await?;
            Ok(())
        }
        Ok(None) => {
            txn.rollback().await?;
            Ok(())
        }
        Err(err) => {
            txn.rollback().await.map_err(|rollback| {
                MegaError::Other(format!(
                    "native bind failed: {err}; rollback failed: {rollback}"
                ))
            })?;
            // Preserve the handler's Postgres named-constraint no-op, only
            // after rollback; an aborted transaction cannot be read or committed.
            if err.to_string().contains("trace_target_one_active_path") {
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

pub(crate) async fn restore_active_ids_in_txn(
    mono: &MonoStorage,
    txn: &DatabaseTransaction,
    ids: &[String],
) -> Result<(), MegaError> {
    let Some(root) = mono.lock_native_root_in_txn(txn).await? else {
        return Ok(());
    };
    let rows = trace_target_storage::lock_native_ids_in_txn(txn, ids).await?;
    let mut winners: BTreeMap<String, String> = BTreeMap::new();
    for row in rows.into_iter().filter(|row| !row.active) {
        // Stored corruption must fail closed, never silently select another path.
        trace_target_storage::strict_canonical_project_path(&row.project_path)?;
        winners
            .entry(row.project_path)
            .and_modify(|id| {
                if row.id < *id {
                    *id = row.id.clone();
                }
            })
            .or_insert(row.id);
    }
    let paths: Vec<String> = winners.keys().cloned().collect();
    let occupied = trace_target_storage::active_owner_paths_in_txn(txn, &paths).await?;
    winners.retain(|path, _| !occupied.contains(path));
    let paths: Vec<String> = winners.keys().cloned().collect();
    let eligible = mono
        .existing_path_trees_in_txn(&root.ref_tree_hash, &paths, txn)
        .await?;
    let ids: Vec<String> = winners
        .into_iter()
        .filter_map(|(path, id)| eligible.contains(&path).then_some(id))
        .collect();
    trace_target_storage::reactivate_locked_native_ids_in_txn(txn, &ids).await?;
    Ok(())
}

pub(crate) async fn restore_active_ids(
    mono: &MonoStorage,
    ids: &[String],
) -> Result<(), MegaError> {
    let txn = mono.get_connection().begin().await?;
    match restore_active_ids_in_txn(mono, &txn, ids).await {
        Ok(()) => {
            txn.commit().await?;
            Ok(())
        }
        Err(err) => {
            txn.rollback().await.map_err(|rollback| {
                MegaError::Other(format!(
                    "native restore failed: {err}; rollback failed: {rollback}"
                ))
            })?;
            Err(err)
        }
    }
}

/// Move the path and every active target under it. Descendants move even
/// when `from` itself has no target.
pub async fn move_covered_native_targets<C>(
    conn: &C,
    from: &str,
    to: &str,
) -> Result<Vec<String>, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = DatabaseTransaction>,
{
    trace_target_storage::move_covered_targets(conn, from, to).await
}

/// Retire the row created by [`bind_new_native_directory`] after the directory
/// commit fails. Other targets are unchanged.
pub async fn abandon_new_binding<C: ConnectionTrait>(conn: &C, id: &str) -> Result<(), MegaError> {
    trace_target_storage::deactivate_by_id(conn, id).await
}

/// Inside the code-publish transaction, deactivate descendant targets whose
/// path is no longer a directory in `landed_tree_id`. The push path itself
/// stays. A missing tree row changes nothing.
pub async fn deactivate_descendants_missing_from_tree(
    txn: &DatabaseTransaction,
    mono: &MonoStorage,
    push_path: &str,
    landed_tree_id: &str,
) -> Result<Vec<String>, MegaError> {
    let push_path = trace_target_storage::canonical_project_path(push_path)?;
    let rows = trace_target_storage::active_at_and_below(txn, &push_path).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    if mono
        .get_tree_by_hash_in_txn(landed_tree_id, txn)
        .await?
        .is_none()
    {
        return Ok(Vec::new());
    }
    let mut deactivated = Vec::new();
    for row in rows {
        if row.project_path == push_path {
            continue;
        }
        let relative = if push_path == "/" {
            row.project_path.clone()
        } else {
            row.project_path[push_path.len()..].to_owned()
        };
        let present = mono
            .resolve_path_tree_hash_in_txn(landed_tree_id, &relative, txn)
            .await?
            .is_some();
        if !present {
            deactivated
                .extend(trace_target_storage::deactivate_tree(txn, &row.project_path).await?);
        }
    }
    Ok(deactivated)
}

/// Generation recorded for `target_id`. A target with no detach audit is at
/// [`INITIAL_BINDING_GENERATION`]. Each successful detach writes the next
/// integer into that audit row.
pub async fn binding_generation<C: ConnectionTrait>(
    conn: &C,
    target_id: &str,
) -> Result<i64, MegaError> {
    let row = conn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT COALESCE(MAX((metadata->>'binding_generation')::bigint), $2::bigint) AS generation
             FROM audit_logs
             WHERE metadata->>'trace_target_id' = $1",
            [target_id.into(), INITIAL_BINDING_GENERATION.into()],
        ))
        .await?;
    let Some(row) = row else {
        return Ok(INITIAL_BINDING_GENERATION);
    };
    row.try_get("", "generation")
        .map_err(|err| MegaError::Other(err.to_string()))
}

/// Deactivate the active import target backed by `repo_id` at `path`.
/// Returns the generation this detach must record, one higher than the
/// current value. No matching row writes nothing.
pub async fn retire_import_target_in_txn(
    txn: &DatabaseTransaction,
    repo_id: i64,
    path: &str,
) -> Result<Option<RetiredImportTarget>, MegaError> {
    let path = trace_target_storage::canonical_project_path(path)?;
    let locked = txn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id FROM trace_target
             WHERE project_path = $1
               AND backing_import_repo_id = $2
               AND active
             FOR UPDATE",
            [path.into(), repo_id.to_string().into()],
        ))
        .await?;
    let Some(locked) = locked else {
        return Ok(None);
    };
    let id: String = locked
        .try_get("", "id")
        .map_err(|err| MegaError::Other(err.to_string()))?;
    let generation = binding_generation(txn, &id).await? + 1;
    let row = trace_target::Entity::find_by_id(&id)
        .one(txn)
        .await?
        .ok_or_else(|| MegaError::Other(format!("trace target {id} disappeared")))?;
    let mut active: trace_target::ActiveModel = row.into();
    active.active = Set(false);
    active.update(txn).await?;
    Ok(Some(RetiredImportTarget {
        id,
        binding_generation: generation,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let temp = tempfile::TempDir::new().expect("temp dir");
        let conn = test_db_connection(temp.path()).await;
        apply_migrations(&conn, true)
            .await
            .expect("migrations should apply");
        (temp, conn)
    }

    fn fence_mono(conn: &sea_orm::DatabaseConnection) -> MonoStorage {
        MonoStorage {
            base: crate::jupiter::storage::base_storage::BaseStorage::new(std::sync::Arc::new(
                conn.clone(),
            )),
        }
    }

    async fn fence_seed_root(mono: &MonoStorage) {
        use git_internal::{
            hash::ObjectHash,
            internal::object::{
                ObjectTrait,
                commit::Commit,
                tree::{Tree, TreeItem, TreeItemMode},
                types::ObjectType,
            },
        };

        use crate::{callisto::mega_refs, common::utils::MEGA_BRANCH_NAME};
        let leaf = Tree::from_bytes(&[], ObjectHash::from_type_and_data(ObjectType::Tree, &[]))
            .expect("empty leaf tree");
        let root = Tree::from_tree_items(
            ["a", "b", "neighbor"]
                .into_iter()
                .map(|name| TreeItem {
                    mode: TreeItemMode::Tree,
                    id: leaf.id,
                    name: name.into(),
                })
                .collect(),
        )
        .expect("root");
        let commit = Commit::from_tree_id(root.id, vec![], "native service fence root");
        mono.save_mega_trees(vec![leaf.clone(), root.clone()], commit.id, None)
            .await
            .expect("persist complete tree closure");
        mono.save_mega_commits(vec![commit.clone()], None)
            .await
            .expect("persist root commit");
        mono.save_refs(
            mega_refs::Model::new(
                "/",
                MEGA_BRANCH_NAME.to_owned(),
                commit.id.to_string(),
                root.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .expect("root ref");
        assert!(
            mono.get_commit_by_hash(&commit.id.to_string())
                .await
                .expect("load commit")
                .is_some()
        );
        for tree in [&leaf, &root] {
            assert!(
                mono.get_tree_by_hash(&tree.id.to_string())
                    .await
                    .expect("load tree")
                    .is_some()
            );
        }
    }

    async fn fence_inactive(conn: &sea_orm::DatabaseConnection, path: &str) -> Model {
        let row = trace_target_storage::insert_active(conn, TARGET_KIND_NATIVE, path, None)
            .await
            .expect("target");
        trace_target_storage::deactivate_by_id(conn, &row.id)
            .await
            .expect("retire");
        trace_target_storage::find_by_id(conn, &row.id)
            .await
            .expect("load")
            .expect("row")
    }

    async fn fence_snapshot(conn: &sea_orm::DatabaseConnection) -> Vec<Model> {
        use sea_orm::QueryOrder;
        trace_target::Entity::find()
            .order_by_asc(trace_target::Column::Id)
            .all(conn)
            .await
            .expect("snapshot")
    }

    async fn fence_pid(txn: &DatabaseTransaction) -> i32 {
        txn.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid".to_owned(),
        ))
        .await
        .expect("pid query")
        .expect("pid row")
        .try_get("", "pid")
        .expect("pid")
    }

    async fn fence_waits_on(conn: &sea_orm::DatabaseConnection, waiter: i32, blocker: i32) -> bool {
        fence_waits_on_either(conn, waiter, blocker, blocker).await
    }

    async fn fence_waits_on_either(
        conn: &sea_orm::DatabaseConnection,
        waiter: i32,
        blocker: i32,
        queued_blocker: i32,
    ) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let row = conn
                    .query_one_raw(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "SELECT $1::integer = ANY(pg_blocking_pids($2::integer))
                            OR $3::integer = ANY(pg_blocking_pids($2::integer)) AS blocked",
                        [blocker.into(), waiter.into(), queued_blocker.into()],
                    ))
                    .await
                    .expect("owned blocking relation")
                    .expect("blocking row");
                if row.try_get::<bool>("", "blocked").expect("blocked") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok()
    }

    async fn fence_abort_all(tasks: Vec<tokio::task::JoinHandle<Result<(), MegaError>>>) {
        for task in &tasks {
            task.abort();
        }
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for task in tasks {
                let _ = task.await;
            }
        })
        .await;
        assert!(
            terminal.is_ok(),
            "aborted fixture tasks did not reach terminal within cleanup bound"
        );
    }

    async fn fence_join_all(mut tasks: Vec<tokio::task::JoinHandle<Result<(), MegaError>>>) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut results = Vec::new();
        while let Some(mut task) = tasks.pop() {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(result) => results.push(result),
                Err(_) => {
                    tasks.push(task);
                    fence_abort_all(tasks).await;
                    panic!("restore did not finish");
                }
            }
        }
        for result in results {
            result.expect("join").expect("restore");
        }
    }

    async fn fence_join(task: tokio::task::JoinHandle<Result<(), MegaError>>) {
        fence_join_all(vec![task]).await;
    }

    async fn fence_lock_target(txn: &DatabaseTransaction, id: &str) {
        txn.query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT id FROM trace_target WHERE id = $1 FOR UPDATE",
            [id.into()],
        ))
        .await
        .expect("lock target")
        .expect("target exists");
    }

    #[tokio::test]
    async fn native_target_fence_missing_root_and_directory_leave_snapshot_unchanged() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        let retired = fence_inactive(&conn, "/a").await;
        let before = fence_snapshot(&conn).await;
        bind_if_still_landed(&mono, "/a")
            .await
            .expect("absent root bind");
        restore_active_ids(&mono, std::slice::from_ref(&retired.id))
            .await
            .expect("absent root restore");
        assert_eq!(fence_snapshot(&conn).await, before);
        fence_seed_root(&mono).await;
        bind_if_still_landed(&mono, "/missing")
            .await
            .expect("absent directory");
        let missing = fence_inactive(&conn, "/missing").await;
        let before = fence_snapshot(&conn).await;
        restore_active_ids(&mono, &[missing.id])
            .await
            .expect("missing path restore");
        assert_eq!(fence_snapshot(&conn).await, before);
    }

    #[tokio::test]
    async fn native_target_fence_bind_error_rolls_back_entire_borrowed_transaction() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let neighbor =
            trace_target_storage::insert_active(&conn, TARGET_KIND_NATIVE, "/neighbor", None)
                .await
                .expect("neighbor");
        let before = fence_snapshot(&conn).await;
        let txn = conn.begin().await.expect("transaction");
        let created = bind_if_still_landed_in_txn(&mono, &txn, "/a")
            .await
            .expect("insert")
            .expect("owned id");
        let error = bind_if_still_landed_in_txn(&mono, &txn, "/neighbor")
            .await
            .expect_err("unique failure must propagate from borrowed core");
        assert!(error.to_string().contains("trace_target_one_active_path"));
        txn.rollback().await.expect("outer rollback");
        assert_eq!(fence_snapshot(&conn).await, before);
        assert!(
            trace_target_storage::find_by_id(&conn, &created)
                .await
                .expect("load")
                .is_none()
        );
        bind_if_still_landed(&mono, "/neighbor")
            .await
            .expect("owned conflict rolls back to no-op");
        assert_eq!(fence_snapshot(&conn).await, before);
        assert_eq!(
            trace_target_storage::active_at(&conn, "/neighbor")
                .await
                .expect("neighbor")
                .expect("active neighbor")
                .id,
            neighbor.id
        );
        conn.execute_unprepared("ALTER TABLE trace_target ADD CONSTRAINT fence_reject_bind CHECK (project_path <> '/b')")
            .await.expect("fixture bind constraint");
        let error = bind_if_still_landed(&mono, "/b")
            .await
            .expect_err("owned bind failure");
        assert!(error.to_string().contains("fence_reject_bind"));
        assert_eq!(fence_snapshot(&conn).await, before);
        bind_if_still_landed(&mono, "/a")
            .await
            .expect("fresh connection after rollback");
        assert!(
            trace_target_storage::active_at(&conn, "/a")
                .await
                .expect("load")
                .is_some()
        );
    }

    #[tokio::test]
    async fn native_target_fence_restore_many_preserves_other_identities() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let a = fence_inactive(&conn, "/a").await;
        let b = fence_inactive(&conn, "/b").await;
        let missing = fence_inactive(&conn, "/missing").await;
        let displaced = fence_inactive(&conn, "/neighbor").await;
        let neighbor =
            trace_target_storage::insert_active(&conn, TARGET_KIND_NATIVE, "/neighbor", None)
                .await
                .expect("neighbor");
        let imported = trace_target_storage::insert_active(
            &conn,
            trace_target_storage::TARGET_KIND_IMPORT,
            "/a",
            Some("fixture-import".into()),
        )
        .await
        .expect("import target");
        trace_target_storage::deactivate_by_id(&conn, &imported.id)
            .await
            .expect("retire import");
        let imported = trace_target_storage::find_by_id(&conn, &imported.id)
            .await
            .expect("import")
            .expect("row");
        let ids = vec![
            b.id.clone(),
            a.id.clone(),
            missing.id.clone(),
            imported.id.clone(),
            displaced.id.clone(),
            a.id.clone(),
            "missing-id".into(),
        ];
        restore_active_ids(&mono, &ids).await.expect("restore");
        for target in [&a, &b] {
            let row = trace_target_storage::find_by_id(&conn, &target.id)
                .await
                .expect("load")
                .expect("row");
            let mut expected = target.clone();
            expected.active = true;
            assert_eq!(row, expected);
        }
        assert_eq!(
            trace_target_storage::find_by_id(&conn, &displaced.id)
                .await
                .expect("load"),
            Some(displaced)
        );
        assert_eq!(
            trace_target_storage::find_by_id(&conn, &imported.id)
                .await
                .expect("load"),
            Some(imported)
        );
        assert_eq!(
            trace_target_storage::find_by_id(&conn, &neighbor.id)
                .await
                .expect("load"),
            Some(neighbor)
        );
        assert_eq!(
            trace_target_storage::find_by_id(&conn, &missing.id)
                .await
                .expect("load"),
            Some(missing)
        );
    }

    #[tokio::test]
    async fn native_target_fence_restore_error_rolls_back_prior_activation() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let mut targets = [
            fence_inactive(&conn, "/a").await,
            fence_inactive(&conn, "/b").await,
        ];
        targets.sort_by(|a, b| a.id.cmp(&b.id));
        let before = fence_snapshot(&conn).await;
        // Schema-local constraint injects a real database failure after the
        // first sorted target was updated; no production failpoint is used.
        let last = uuid::Uuid::parse_str(&targets[1].id).expect("generated UUID");
        conn.execute_unprepared(&format!("ALTER TABLE trace_target ADD CONSTRAINT fence_reject_second CHECK (NOT (active AND id = '{last}'))"))
            .await.expect("fixture constraint");
        let error = restore_active_ids(&mono, &[targets[1].id.clone(), targets[0].id.clone()])
            .await
            .expect_err("second update fails");
        assert!(error.to_string().contains("fence_reject_second"));
        assert_eq!(fence_snapshot(&conn).await, before);
    }

    #[tokio::test]
    async fn native_target_fence_restore_uses_path_after_waiting_for_row_lock() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let target = fence_inactive(&conn, "/a").await;
        let mover = conn.begin().await.expect("mover");
        let mover_pid = fence_pid(&mover).await;
        let mut moved: trace_target::ActiveModel = target.clone().into();
        moved.project_path = Set("/missing".into());
        moved.update(&mover).await.expect("uncommitted move");
        let restoring = conn.begin().await.expect("restoring");
        let restoring_pid = fence_pid(&restoring).await;
        let id = target.id.clone();
        let task = tokio::spawn(async move {
            let result = restore_active_ids_in_txn(&mono, &restoring, &[id]).await;
            match result {
                Ok(()) => restoring.commit().await.map_err(MegaError::from),
                Err(err) => {
                    restoring.rollback().await?;
                    Err(err)
                }
            }
        });
        let blocked = fence_waits_on(&conn, restoring_pid, mover_pid).await;
        if !blocked {
            let released = mover.rollback().await;
            fence_abort_all(vec![task]).await;
            released.expect("release mover");
            panic!("restorer did not wait on the owned target lock");
        }
        mover.commit().await.expect("move commits");
        fence_join(task).await;
        let row = trace_target_storage::find_by_id(&conn, &target.id)
            .await
            .expect("load")
            .expect("row");
        assert_eq!(row.project_path, "/missing");
        assert!(
            !row.active,
            "old landed path cannot justify the newly locked path"
        );
    }

    #[tokio::test]
    async fn native_target_fence_root_lock_precedes_target_lock() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let target = fence_inactive(&conn, "/a").await;
        let publisher = conn.begin().await.expect("publisher");
        let publisher_pid = fence_pid(&publisher).await;
        publisher.query_one_raw(Statement::from_sql_and_values(DatabaseBackend::Postgres,
            "SELECT id FROM mega_refs WHERE path = '/' AND ref_name = $1 AND is_cl = false FOR UPDATE",
            [crate::common::utils::MEGA_BRANCH_NAME.into()])).await.expect("root lock").expect("root");
        let restoring = conn.begin().await.expect("restoring");
        let restoring_pid = fence_pid(&restoring).await;
        let id = target.id.clone();
        let task = tokio::spawn(async move {
            let result = restore_active_ids_in_txn(&mono, &restoring, &[id]).await;
            restoring
                .rollback()
                .await
                .expect("restore fixture rollback");
            result
        });
        let blocked = fence_waits_on(&conn, restoring_pid, publisher_pid).await;
        if !blocked {
            let released = publisher.rollback().await;
            fence_abort_all(vec![task]).await;
            released.expect("release publisher");
            panic!("restorer did not wait on the owned root lock");
        }
        let probe = conn.begin().await.expect("probe");
        let unlocked = probe
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT id FROM trace_target WHERE id = $1 FOR UPDATE NOWAIT",
                [target.id.into()],
            ))
            .await;
        probe.rollback().await.expect("release probe");
        publisher.rollback().await.expect("release publisher");
        fence_join(task).await;
        assert!(
            unlocked.is_ok(),
            "target must remain unlocked while waiting for root"
        );
    }

    #[tokio::test]
    async fn native_target_fence_reverse_id_inputs_lock_in_same_order() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        use git_internal::{
            hash::ObjectHash,
            internal::object::{
                ObjectTrait,
                tree::{Tree, TreeItem, TreeItemMode},
                types::ObjectType,
            },
        };
        fence_seed_root(&mono).await;
        let mut targets = vec![
            fence_inactive(&conn, "/a").await,
            fence_inactive(&conn, "/b").await,
        ];
        // Keep the original /a and /b identities and real directory closure;
        // extend that same fixture past the production chunk boundary.
        let paths: Vec<String> = (0..999).map(|i| format!("/extra_{i:04}")).collect();
        let leaf = Tree::from_bytes(&[], ObjectHash::from_type_and_data(ObjectType::Tree, &[]))
            .expect("real empty leaf");
        let root = Tree::from_tree_items(
            ["a", "b", "neighbor"]
                .into_iter()
                .chain(paths.iter().map(|path| &path[1..]))
                .map(|name| TreeItem {
                    mode: TreeItemMode::Tree,
                    id: leaf.id,
                    name: name.to_owned(),
                })
                .collect(),
        )
        .expect("expanded real root");
        let commit = git_internal::internal::object::commit::Commit::from_tree_id(
            root.id,
            vec![],
            "cross-chunk expanded native root",
        );
        // The original seed already persisted the leaf and the unique root ref.
        mono.save_mega_trees(vec![root.clone()], commit.id, None)
            .await
            .expect("persist expanded root tree");
        mono.save_mega_commits(vec![commit.clone()], None)
            .await
            .expect("persist expanded root commit");
        let mut root_ref = mono
            .get_main_ref("/")
            .await
            .expect("load seeded root ref")
            .expect("seeded root ref exists");
        let root_ref_id = root_ref.id;
        root_ref.ref_commit_hash = commit.id.to_string();
        root_ref.ref_tree_hash = root.id.to_string();
        mono.update_ref(root_ref, None)
            .await
            .expect("update existing root ref");
        let expanded_ref = mono
            .get_main_ref("/")
            .await
            .expect("load expanded root ref")
            .expect("expanded root ref exists");
        assert_eq!(expanded_ref.id, root_ref_id);
        assert_eq!(expanded_ref.ref_commit_hash, commit.id.to_string());
        assert_eq!(expanded_ref.ref_tree_hash, root.id.to_string());
        let ids = batch_seed_inactive(&conn, &paths).await;
        targets.extend(
            fence_snapshot(&conn)
                .await
                .into_iter()
                .filter(|target| ids.contains(&target.id)),
        );
        assert_eq!(targets.len(), 1001);
        assert_eq!(ids.len(), 999);
        targets.sort_by(|a, b| a.id.cmp(&b.id));
        let blocker = conn.begin().await.expect("blocker");
        let blocker_pid = fence_pid(&blocker).await;
        fence_lock_target(&blocker, &targets[0].id).await;
        let boundary_blocker = conn.begin().await.expect("next chunk blocker");
        let boundary_pid = fence_pid(&boundary_blocker).await;
        fence_lock_target(&boundary_blocker, &targets[1000].id).await;
        let ascending: Vec<String> = targets.iter().map(|target| target.id.clone()).collect();
        let descending: Vec<String> = ascending.iter().rev().cloned().collect();
        let mut first_pid = None;
        let mut tasks = Vec::new();
        let mut blocked = true;
        let mut queued_blocker = blocker_pid;
        for ids in [ascending, descending] {
            let txn = conn.begin().await.expect("restore txn");
            let pid = fence_pid(&txn).await;
            first_pid.get_or_insert(pid);
            let mono = mono.clone();
            tasks.push(tokio::spawn(async move {
                let result = restore_active_ids_in_txn(&mono, &txn, &ids).await;
                match result {
                    Ok(()) => txn.commit().await.map_err(MegaError::from),
                    Err(err) => {
                        txn.rollback().await?;
                        Err(err)
                    }
                }
            }));
            // Postgres can queue the second waiter behind the first
            // waiter's tuple lock rather than directly behind its owner.
            blocked &= fence_waits_on_either(&conn, pid, blocker_pid, queued_blocker).await;
            queued_blocker = pid;
        }
        let probe = conn.begin().await.expect("probe");
        let high_unlocked = probe
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT id FROM trace_target WHERE id = $1 FOR UPDATE NOWAIT",
                [targets[1].id.clone().into()],
            ))
            .await;
        probe.rollback().await.expect("release probe");
        blocker.rollback().await.expect("release blocker");
        // After the lower blocker releases, the first restorer must hold the
        // entire first chunk before it can wait on the next chunk's first ID.
        let boundary_blocked = if blocked {
            fence_waits_on(&conn, first_pid.expect("first restorer PID"), boundary_pid).await
        } else {
            false
        };
        let first_chunk_probe = conn.begin().await.expect("first chunk probe");
        let first_chunk_tail = first_chunk_probe
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT id FROM trace_target WHERE id = $1 FOR UPDATE NOWAIT",
                [targets[999].id.clone().into()],
            ))
            .await;
        let first_chunk_locked = match &first_chunk_tail {
            Err(sea_orm::DbErr::Query(sea_orm::RuntimeErr::SqlxError(error)))
            | Err(sea_orm::DbErr::Exec(sea_orm::RuntimeErr::SqlxError(error))) => error
                .as_database_error()
                .is_some_and(|database| database.code().as_deref() == Some("55P03")),
            _ => false,
        };
        first_chunk_probe
            .rollback()
            .await
            .expect("release first chunk probe");
        boundary_blocker
            .rollback()
            .await
            .expect("release next chunk blocker");
        if blocked && boundary_blocked {
            fence_join_all(tasks).await;
        } else {
            fence_abort_all(tasks).await;
        }
        assert!(
            blocked,
            "both input orders must wait on the same lower target ID"
        );
        assert!(
            high_unlocked.is_ok(),
            "no higher target may be locked before the lower ID"
        );
        assert!(
            boundary_blocked,
            "first restorer must wait on the next chunk only after the lower blocker releases"
        );
        assert!(
            first_chunk_locked,
            "the last target of the first chunk must remain locked while waiting on the next chunk"
        );
        for target in targets {
            assert!(
                trace_target_storage::find_by_id(&conn, &target.id)
                    .await
                    .expect("load")
                    .expect("row")
                    .active
            );
        }
    }

    #[tokio::test]
    async fn target_path_lifecycle_create_move_delete_rebuild_and_rebind() {
        let (_temp, conn) = db().await;
        let created = bind_new_native_directory(&conn, "/project/app")
            .await
            .expect("create")
            .expect("new id");
        assert!(
            bind_new_native_directory(&conn, "/project/app")
                .await
                .expect("existing")
                .is_none()
        );
        let child = bind_first_native_write(&conn, "/project/app/lib")
            .await
            .expect("child");
        let moved = move_native_target(&conn, "/project/app", "/project/moved")
            .await
            .expect("move");
        assert_eq!(moved.id, created);
        assert_eq!(moved.project_path, "/project/moved");
        assert_eq!(
            trace_target_storage::active_at(&conn, "/project/moved/lib")
                .await
                .expect("child load")
                .expect("child moved")
                .id,
            child.id
        );

        let same = identity_for_same_tree_oid(&moved, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(same.id, moved.id);
        assert_eq!(same.project_path, "/project/moved");
        assert_eq!(
            trace_target_storage::active_at(&conn, "/project/moved")
                .await
                .expect("oid ignored")
                .expect("still there")
                .id,
            created
        );

        let refused = rebind_if_path_exists(&conn, &created, "/project/next", false)
            .await
            .expect_err("missing path");
        assert!(refused.to_string().contains("does not exist"));
        assert_eq!(
            trace_target_storage::find_by_id(&conn, &created)
                .await
                .expect("load")
                .expect("row")
                .project_path,
            "/project/moved"
        );
        let rebound = rebind_if_path_exists(&conn, &created, "/project/next", true)
            .await
            .expect("rebind");
        assert_eq!(rebound.id, created);
        assert_eq!(rebound.project_path, "/project/next");

        let gone = delete_native_target(&conn, "/project/next")
            .await
            .expect("delete");
        assert_eq!(gone, vec![created.clone()]);
        assert!(
            trace_target_storage::active_at(&conn, "/project/next")
                .await
                .expect("inactive")
                .is_none()
        );
        assert_eq!(
            trace_target_storage::active_at(&conn, "/project/moved/lib")
                .await
                .expect("child stays")
                .expect("child")
                .id,
            child.id
        );
        let rebuilt = bind_first_native_write(&conn, "/project/next")
            .await
            .expect("rebuild");
        assert_ne!(rebuilt.id, created);
        let abandoned = bind_new_native_directory(&conn, "/project/temp")
            .await
            .expect("temp")
            .expect("temp id");
        abandon_new_binding(&conn, &abandoned)
            .await
            .expect("abandon");
        assert!(
            trace_target_storage::active_at(&conn, "/project/temp")
                .await
                .expect("abandoned")
                .is_none()
        );
        assert_eq!(
            trace_target_storage::active_at(&conn, "/project/next")
                .await
                .expect("rebuild stays")
                .expect("rebuilt")
                .id,
            rebuilt.id
        );
    }

    #[tokio::test]
    async fn target_path_lifecycle_unbound_parent_move_keeps_child() {
        let (_temp, conn) = db().await;
        let child = bind_first_native_write(&conn, "/project/x/lib")
            .await
            .expect("child");
        let moved = move_covered_native_targets(&conn, "/project/x", "/project/y")
            .await
            .expect("move");
        assert_eq!(moved, vec![child.id.clone()]);
        assert_eq!(
            trace_target_storage::active_at(&conn, "/project/y/lib")
                .await
                .expect("dest")
                .expect("moved")
                .id,
            child.id
        );
        assert!(
            trace_target_storage::active_at(&conn, "/project/x")
                .await
                .expect("parent")
                .is_none()
        );
    }

    type BatchSqlLog = std::sync::Arc<std::sync::Mutex<Vec<(String, bool)>>>;

    async fn batch_budget_db() -> (
        tempfile::TempDir,
        crate::jupiter::tests::TestSchemaGuard,
        sea_orm::DatabaseConnection,
        sea_orm::DatabaseConnection,
        BatchSqlLog,
    ) {
        let temp = tempfile::tempdir().expect("private fixture directory");
        let (config, schema) = crate::jupiter::tests::test_db_config(temp.path()).await;
        let mut conn = crate::jupiter::storage::init::database_connection(&config)
            .await
            .expect("independently guarded metric connection");
        apply_migrations(&conn, true)
            .await
            .expect("explicit fixture migrations");
        let observer = crate::jupiter::storage::init::read_only_database_connection(&config)
            .await
            .expect("independent same-schema observation connection");
        let counter_schema: String = conn
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT current_schema()::text AS schema_name".to_owned(),
            ))
            .await
            .expect("counter schema")
            .expect("counter schema row")
            .try_get("", "schema_name")
            .expect("counter schema name");
        let fixture_name = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        let index = observer
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT n.nspname::text AS schema_name,
                    i.indisunique AS is_unique, i.indisvalid AS is_valid,
                    i.indisready AS is_ready,
                    i.indnatts = 1 AND i.indnkeyatts = 1 AND i.indexprs IS NULL AS one_plain_key,
                    pg_get_indexdef(i.indexrelid) AS definition,
                    pg_get_indexdef(i.indexrelid, 1, true) AS index_key,
                    pg_get_expr(i.indpred, i.indrelid) AS predicate
             FROM pg_index i
             JOIN pg_class c ON c.oid = i.indexrelid
             JOIN pg_class t ON t.oid = i.indrelid
             JOIN pg_namespace n ON n.oid = t.relnamespace AND c.relnamespace = n.oid
             WHERE n.nspname = current_schema()
               AND t.relname = 'trace_target'
               AND c.relname = 'trace_target_one_active_path'"
                    .to_owned(),
            ))
            .await
            .expect("private fixture index catalog")
            .expect("required active-owner index");
        let index_schema: String = index.try_get("", "schema_name").expect("index schema");
        let is_unique: bool = index.try_get("", "is_unique").expect("unique flag");
        let is_valid: bool = index.try_get("", "is_valid").expect("valid flag");
        let is_ready: bool = index.try_get("", "is_ready").expect("ready flag");
        let one_plain_key: bool = index
            .try_get("", "one_plain_key")
            .expect("single plain index key");
        let definition: String = index.try_get("", "definition").expect("index definition");
        let index_key: String = index.try_get("", "index_key").expect("index key");
        let predicate: String = index
            .try_get("", "predicate")
            .expect("partial index predicate");
        eprintln!(
            "FIX06_VER7_SYNTHETIC_INDEX fixture={fixture_name:?} schema={index_schema:?} unique={is_unique} valid={is_valid} ready={is_ready} one_plain_key={one_plain_key} key={index_key:?} predicate={predicate:?} definition={definition:?}"
        );
        assert!(
            index_schema.starts_with("mega2_test_"),
            "index must be private: {index_schema}"
        );
        assert_eq!(index_schema, counter_schema, "same private schema");
        assert!(is_unique && is_valid && is_ready && one_plain_key);
        assert_eq!(index_key, "project_path");
        assert_eq!(predicate, "active");
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let callback_log = log.clone();
        conn.set_metric_callback(move |info| {
            callback_log
                .lock()
                .expect("metric log")
                .push((info.statement.sql.clone(), info.failed));
            if info.statement.sql.starts_with("SELECT")
                && info.statement.sql.contains("\"trace_target\"")
                && info.statement.sql.contains("FOR UPDATE")
            {
                eprintln!(
                    "FIX06_VER6_SYNTHETIC_TARGET_LOCK fixture={fixture_name:?} failed={} statement={:?}",
                    info.failed, info.statement
                );
            }
        });
        observer
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT 1 AS observation_connection_probe".to_owned(),
            ))
            .await
            .expect("independent observer probe")
            .expect("observer probe row");
        assert!(log.lock().expect("metric log").is_empty());
        (temp, schema, conn, observer, log)
    }

    async fn batch_metric_probe(txn: &DatabaseTransaction, log: &BatchSqlLog) {
        log.lock().expect("metric log").clear();
        let row = txn
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT 1 AS batch_metric_probe".to_owned(),
            ))
            .await
            .expect("known borrowed-transaction SQL")
            .expect("known SQL row");
        assert_eq!(
            row.try_get::<i32>("", "batch_metric_probe").expect("one"),
            1
        );
        let mut entries = log.lock().expect("metric log");
        assert_eq!(
            entries.len(),
            1,
            "transaction must retain installed callback"
        );
        assert_eq!(
            entries[0],
            ("SELECT 1 AS batch_metric_probe".to_owned(), false)
        );
        entries.clear();
    }

    fn batch_assert_queries(entries: &[(String, bool)], expected: [usize; 5]) {
        let mut actual = [0; 5];
        let mut previous_class = 0;
        for (sql, failed) in entries {
            assert!(!failed, "unexpected failed SQL: {sql}");
            let index = if sql.starts_with("SELECT") && sql.contains("\"mega_refs\"") {
                assert!(sql.contains("FOR SHARE"));
                0
            } else if sql.starts_with("SELECT") && sql.contains("\"trace_target\"") {
                if sql.contains("FOR UPDATE") {
                    assert!(sql.contains("ORDER BY (CASE"), "explicit lock order: {sql}");
                    1
                } else {
                    2
                }
            } else if sql.starts_with("SELECT") && sql.contains("\"mega_tree\"") {
                3
            } else if sql.starts_with("UPDATE \"trace_target\"") {
                assert!(
                    sql.contains("NOT EXISTS"),
                    "write-time owner recheck: {sql}"
                );
                4
            } else {
                panic!("unclassified restore SQL: {sql}");
            };
            assert!(
                index >= previous_class,
                "root/all-target/owner/tree/update phase order: {sql}"
            );
            previous_class = index;
            actual[index] += 1;
        }
        assert_eq!(
            actual, expected,
            "root/target/owner/tree/update query counts"
        );
        assert_eq!(entries.len(), expected.iter().sum::<usize>());
    }

    async fn batch_measured_restore(
        mono: &MonoStorage,
        conn: &sea_orm::DatabaseConnection,
        log: &BatchSqlLog,
        ids: &[String],
    ) -> Vec<(String, bool)> {
        let txn = conn.begin().await.expect("measured transaction");
        batch_metric_probe(&txn, log).await;
        let outcome = restore_active_ids_in_txn(mono, &txn, ids).await;
        let entries = log.lock().expect("metric log").clone();
        match outcome {
            Ok(()) => txn.commit().await.expect("commit measured restore"),
            Err(error) => {
                txn.rollback().await.expect("failed measurement rollback");
                panic!("restore failed: {error}");
            }
        }
        entries
    }

    async fn batch_install_root(
        mono: &MonoStorage,
        root: git_internal::internal::object::tree::Tree,
        trees: Vec<git_internal::internal::object::tree::Tree>,
    ) {
        use git_internal::internal::object::commit::Commit;
        let commit = Commit::from_tree_id(root.id, vec![], "batch restore private tree fixture");
        mono.save_mega_trees(trees, commit.id, None)
            .await
            .expect("persist actual tree models");
        mono.save_mega_commits(vec![commit.clone()], None)
            .await
            .expect("persist actual root commit");
        mono.save_refs(
            crate::callisto::mega_refs::Model::new(
                "/",
                crate::common::utils::MEGA_BRANCH_NAME.to_owned(),
                commit.id.to_string(),
                root.id.to_string(),
                false,
            ),
            None,
        )
        .await
        .expect("root ref");
    }

    async fn batch_seed_directories(
        mono: &MonoStorage,
        count: usize,
        distinct: bool,
    ) -> Vec<String> {
        use git_internal::{
            hash::ObjectHash,
            internal::object::{
                ObjectTrait,
                tree::{Tree, TreeItem, TreeItemMode},
                types::ObjectType,
            },
        };
        let leaf = Tree::from_bytes(&[], ObjectHash::from_type_and_data(ObjectType::Tree, &[]))
            .expect("real empty tree");
        let mut trees = vec![leaf.clone()];
        let mut root_items = Vec::new();
        let mut paths = Vec::new();
        for index in 0..count {
            let child = if distinct {
                let child = Tree::from_tree_items(vec![TreeItem {
                    mode: TreeItemMode::Tree,
                    id: leaf.id,
                    name: format!("leaf_{index:04}"),
                }])
                .expect("distinct actual child tree");
                trees.push(child.clone());
                child
            } else {
                leaf.clone()
            };
            let name = format!("d{index:04}");
            paths.push(format!("/{name}"));
            root_items.push(TreeItem {
                mode: TreeItemMode::Tree,
                id: child.id,
                name,
            });
        }
        let root = if count == 0 {
            leaf
        } else {
            Tree::from_tree_items(root_items).expect("real directory root")
        };
        if count > 0 {
            trees.push(root.clone());
        }
        batch_install_root(mono, root, trees).await;
        paths
    }

    async fn batch_seed_inactive(
        conn: &sea_orm::DatabaseConnection,
        paths: &[String],
    ) -> Vec<String> {
        let ids: Vec<String> = (1..=paths.len())
            .map(|number| uuid::Uuid::from_u128(number as u128).to_string())
            .collect();
        let now = chrono::Utc::now().fixed_offset();
        let rows: Vec<_> = paths
            .iter()
            .zip(&ids)
            .map(|(path, id)| trace_target::ActiveModel {
                id: Set(id.clone()),
                target_kind: Set(TARGET_KIND_NATIVE.to_owned()),
                project_path: Set(path.clone()),
                backing_import_repo_id: Set(None),
                active: Set(false),
                created_at: Set(now),
            })
            .collect();
        for chunk in rows.chunks(1000) {
            trace_target::Entity::insert_many(chunk.iter().cloned())
                .exec(conn)
                .await
                .expect("private inactive target rows");
        }
        ids
    }

    #[tokio::test]
    async fn native_target_batch_budget_shared_paths_chunk_boundaries_and_duplicates() {
        use sea_orm::sea_query::Expr;
        let (_temp, _schema, conn, observer, log) = batch_budget_db().await;
        let mono = fence_mono(&conn);
        let paths = batch_seed_directories(&mono, 1001, false).await;
        let ids = batch_seed_inactive(&conn, &paths).await;
        let baseline = fence_snapshot(&observer).await;
        for count in [0_usize, 1, 1000, 1001] {
            trace_target::Entity::update_many()
                .col_expr(trace_target::Column::Active, Expr::value(false))
                .exec(&conn)
                .await
                .expect("reset private fixture");
            let mut input = ids[..count].to_vec();
            input.reverse();
            if count > 0 {
                input.extend([ids[0].clone(), ids[count - 1].clone()]);
            }
            let entries = batch_measured_restore(&mono, &conn, &log, &input).await;
            let chunks = count.div_ceil(1000);
            batch_assert_queries(
                &entries,
                [1, chunks, chunks, usize::from(count > 0) * 2, chunks],
            );
            let mut expected = baseline.clone();
            for row in &mut expected {
                row.active = ids[..count].contains(&row.id);
            }
            assert_eq!(fence_snapshot(&observer).await, expected);
        }
    }

    #[tokio::test]
    async fn native_target_batch_budget_missing_root_and_large_missing_path_frontier() {
        let (_temp, _schema, conn, observer, log) = batch_budget_db().await;
        let mono = fence_mono(&conn);
        let paths: Vec<_> = (0..1001)
            .map(|index| format!("/missing{index:04}"))
            .collect();
        let ids = batch_seed_inactive(&conn, &paths).await;
        let before = fence_snapshot(&observer).await;
        for input in [&[][..], ids.as_slice()] {
            let entries = batch_measured_restore(&mono, &conn, &log, input).await;
            batch_assert_queries(&entries, [1, 0, 0, 0, 0]);
        }
        batch_seed_directories(&mono, 0, false).await;
        let entries = batch_measured_restore(&mono, &conn, &log, &ids).await;
        // p=1001, but only the actual root OID is needed and w=0.
        batch_assert_queries(&entries, [1, 2, 2, 1, 0]);
        assert_eq!(fence_snapshot(&observer).await, before);
    }

    #[tokio::test]
    async fn native_target_batch_budget_distinct_tree_oids_and_winners_cross_chunk() {
        let (_temp, _schema, conn, observer, log) = batch_budget_db().await;
        let mono = fence_mono(&conn);
        let paths = batch_seed_directories(&mono, 1001, true).await;
        let ids = batch_seed_inactive(&conn, &paths).await;
        let before = fence_snapshot(&observer).await;
        let entries = batch_measured_restore(&mono, &conn, &log, &ids).await;
        // Fixture graph: one root and 1001 distinct final child trees, not their leaves.
        batch_assert_queries(&entries, [1, 2, 2, 3, 2]);
        let expected: Vec<_> = before
            .into_iter()
            .map(|mut row| {
                row.active = true;
                row
            })
            .collect();
        assert_eq!(fence_snapshot(&observer).await, expected);
    }

    #[tokio::test]
    async fn native_target_batch_budget_same_path_global_smallest_id_wins() {
        let (_temp, _schema, conn, observer, log) = batch_budget_db().await;
        let mono = fence_mono(&conn);
        let paths = batch_seed_directories(&mono, 1, false).await;
        let ids = batch_seed_inactive(&conn, &vec![paths[0].clone(); 1001]).await;
        let before = fence_snapshot(&observer).await;
        let mut input = ids.clone();
        input.reverse();
        input.extend(ids.clone());
        let entries = batch_measured_restore(&mono, &conn, &log, &input).await;
        batch_assert_queries(&entries, [1, 2, 1, 2, 1]);
        let expected: Vec<_> = before
            .into_iter()
            .map(|mut row| {
                row.active = row.id == ids[0];
                row
            })
            .collect();
        assert_eq!(fence_snapshot(&observer).await, expected);
    }

    #[tokio::test]
    async fn native_target_batch_budget_native_and_import_owner_skip_tree_queries() {
        let (_temp, _schema, conn, observer, log) = batch_budget_db().await;
        let mono = fence_mono(&conn);
        let paths = batch_seed_directories(&mono, 2, false).await;
        let ids = batch_seed_inactive(&conn, &paths).await;
        trace_target_storage::insert_active(&conn, TARGET_KIND_NATIVE, &paths[0], None)
            .await
            .expect("committed native owner before owner query");
        trace_target_storage::insert_active(
            &conn,
            trace_target_storage::TARGET_KIND_IMPORT,
            &paths[1],
            Some("private-import-fixture".to_owned()),
        )
        .await
        .expect("committed import owner before owner query");
        let before = fence_snapshot(&observer).await;
        let entries = batch_measured_restore(&mono, &conn, &log, &ids).await;
        batch_assert_queries(&entries, [1, 1, 1, 0, 0]);
        assert_eq!(fence_snapshot(&observer).await, before);
    }

    #[tokio::test]
    async fn native_target_batch_budget_missing_blob_and_missing_final_tree_are_ineligible() {
        use git_internal::{
            hash::ObjectHash,
            internal::{
                metadata::EntryMeta,
                object::{
                    ObjectTrait,
                    blob::Blob,
                    tree::{Tree, TreeItem, TreeItemMode},
                    types::ObjectType,
                },
            },
        };
        use sea_orm::{ColumnTrait, QueryFilter};

        use crate::jupiter::{service::git_service::GitService, utils::converter::IntoMegaModel};

        let (_temp, _schema, conn, observer, log) = batch_budget_db().await;
        let mono = fence_mono(&conn);
        let blob = Blob::from_content("actual private blob fixture\n");
        let objects = GitService::mock();
        objects
            .put_objects(vec![blob.clone()])
            .await
            .expect("persist actual blob bytes");
        assert_eq!(
            objects
                .get_object_as_bytes(&blob.id.to_string())
                .await
                .expect("read blob"),
            blob.data
        );
        let blob_model: crate::callisto::mega_blob::ActiveModel =
            blob.clone().into_mega_model(EntryMeta::default()).into();
        blob_model
            .insert(&conn)
            .await
            .expect("actual blob metadata");
        let leaf = Tree::from_bytes(&[], ObjectHash::from_type_and_data(ObjectType::Tree, &[]))
            .expect("real empty tree");
        let root = Tree::from_tree_items(vec![
            TreeItem {
                mode: TreeItemMode::Blob,
                id: blob.id,
                name: "file".to_owned(),
            },
            TreeItem {
                mode: TreeItemMode::Tree,
                id: leaf.id,
                name: "directory".to_owned(),
            },
        ])
        .expect("actual mixed root");
        batch_install_root(&mono, root.clone(), vec![root, leaf.clone()]).await;
        let paths = vec![
            "/missing".to_owned(),
            "/file".to_owned(),
            "/directory".to_owned(),
        ];
        let ids = batch_seed_inactive(&conn, &paths).await;
        let before = fence_snapshot(&observer).await;
        let entries = batch_measured_restore(&mono, &conn, &log, &ids).await;
        batch_assert_queries(&entries, [1, 1, 1, 2, 1]);
        let expected: Vec<_> = before
            .iter()
            .cloned()
            .map(|mut row| {
                row.active = row.id == ids[2];
                row
            })
            .collect();
        assert_eq!(fence_snapshot(&observer).await, expected);

        trace_target_storage::deactivate_by_id(&conn, &ids[2])
            .await
            .expect("retire directory");
        let deleted = crate::callisto::mega_tree::Entity::delete_many()
            .filter(crate::callisto::mega_tree::Column::TreeId.eq(leaf.id.to_string()))
            .exec(&conn)
            .await
            .expect("deliberately remove previously persisted final tree");
        assert_eq!(deleted.rows_affected, 1);
        assert_eq!(fence_snapshot(&observer).await, before);
        let entries = batch_measured_restore(&mono, &conn, &log, &ids).await;
        batch_assert_queries(&entries, [1, 1, 1, 2, 0]);
        assert_eq!(fence_snapshot(&observer).await, before);
        assert_eq!(
            objects
                .get_object_as_bytes(&blob.id.to_string())
                .await
                .expect("blob unchanged"),
            blob.data
        );
    }

    #[tokio::test]
    async fn native_target_fence_batch_later_update_chunk_failure_rolls_back_all_rows() {
        let (_temp, _schema, conn, observer, log) = batch_budget_db().await;
        let mono = fence_mono(&conn);
        let paths = batch_seed_directories(&mono, 1001, false).await;
        let ids = batch_seed_inactive(&conn, &paths).await;
        let before = fence_snapshot(&observer).await;
        conn.execute_unprepared("CREATE SEQUENCE batch_activation_probe")
            .await
            .expect("private probe sequence");
        conn.execute_unprepared(&format!(
            "ALTER TABLE trace_target ADD CONSTRAINT batch_reject_last CHECK \
             (NOT active OR CASE WHEN id = '{}' THEN FALSE ELSE nextval('batch_activation_probe') > 0 END)",
            ids[1000],
        )).await.expect("schema-local second-chunk failure");
        let probe = conn.begin().await.expect("callback probe transaction");
        batch_metric_probe(&probe, &log).await;
        probe.rollback().await.expect("probe rollback");
        log.lock().expect("metric log").clear();
        let error = restore_active_ids(&mono, &ids)
            .await
            .expect_err("actual owned wrapper must roll back");
        assert!(error.to_string().contains("batch_reject_last"));
        let entries = log.lock().expect("metric log").clone();
        let updates: Vec<_> = entries
            .iter()
            .filter(|(sql, _)| sql.starts_with("UPDATE \"trace_target\""))
            .collect();
        assert_eq!(updates.len(), 2, "failure must be in a later update chunk");
        assert!(!updates[0].1 && updates[1].1);
        assert_eq!(fence_snapshot(&observer).await, before);
        // Sequences are nontransactional: prove the first chunk really updated 1000 rows.
        let row = observer
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT last_value, is_called FROM batch_activation_probe".to_owned(),
            ))
            .await
            .expect("probe read")
            .expect("probe row");
        assert!(row.try_get::<bool>("", "is_called").expect("called"));
        assert_eq!(
            row.try_get::<i64>("", "last_value")
                .expect("activated rows"),
            1000
        );
    }

    #[tokio::test]
    async fn native_target_fence_batch_noncanonical_inactive_path_errors_without_rewrite() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let valid = fence_inactive(&conn, "/a").await;
        let corrupt = fence_inactive(&conn, "/b").await;
        for path in ["/b/", "b", "/a//b", "/a/./b", "/a/../b"] {
            let mut model: trace_target::ActiveModel = corrupt.clone().into();
            model.project_path = Set(path.to_owned());
            model
                .update(&conn)
                .await
                .expect("private corrupt-row fixture");
            let before = fence_snapshot(&conn).await;
            let error = restore_active_ids(&mono, &[valid.id.clone(), corrupt.id.clone()])
                .await
                .expect_err("stored noncanonical path must fail closed");
            assert!(error.to_string().contains("canonical project path"));
            assert_eq!(
                fence_snapshot(&conn).await,
                before,
                "no path rewrite or partial activation"
            );
        }
    }

    #[tokio::test]
    async fn native_target_fence_batch_malformed_tree_errors_roll_back_borrowed_prior_write() {
        use git_internal::{hash::ObjectHash, internal::object::types::ObjectType};
        use sea_orm::{ColumnTrait, QueryFilter};
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let target = fence_inactive(&conn, "/a").await;
        let empty = ObjectHash::from_type_and_data(ObjectType::Tree, &[]).to_string();
        let leaf = mono
            .get_tree_by_hash(&empty)
            .await
            .expect("load leaf")
            .expect("leaf");
        let mut unknown_mode = b"999999 child\0".to_vec();
        unknown_mode.extend(ObjectHash::from_type_and_data(ObjectType::Tree, &[]).as_ref());
        for bytes in [b"unterminated tree entry".to_vec(), unknown_mode] {
            let mut model: crate::callisto::mega_tree::ActiveModel = leaf.clone().into();
            model.sub_trees = Set(bytes);
            model
                .update(&conn)
                .await
                .expect("private malformed tree fixture");
            let before = fence_snapshot(&conn).await;
            let txn = conn.begin().await.expect("borrowed transaction");
            let prior =
                trace_target_storage::insert_active(&txn, TARGET_KIND_NATIVE, "/prior", None)
                    .await
                    .expect("real prior write in borrowed transaction");
            let error = restore_active_ids_in_txn(&mono, &txn, std::slice::from_ref(&target.id))
                .await
                .expect_err("fallible parse must propagate instead of panicking");
            assert!(error.to_string().contains("invalid native tree data"));
            txn.rollback()
                .await
                .expect("owner rolls back entire transaction");
            assert_eq!(fence_snapshot(&conn).await, before);
            assert!(
                trace_target_storage::find_by_id(&conn, &prior.id)
                    .await
                    .expect("lookup")
                    .is_none()
            );
            let error = restore_active_ids(&mono, std::slice::from_ref(&target.id))
                .await
                .expect_err("owned restore propagates parse error too");
            assert!(error.to_string().contains("invalid native tree data"));
            assert_eq!(fence_snapshot(&conn).await, before);
        }
        // A DB row can be corrupt independently of its bytes: parsing its OID is fallible too.
        let mut model: crate::callisto::mega_tree::ActiveModel = leaf.into();
        model.tree_id = Set("not-a-tree-oid".to_owned());
        model.sub_trees = Set(Vec::new());
        model
            .update(&conn)
            .await
            .expect("private corrupt stored hash");
        crate::callisto::mega_refs::Entity::update_many()
            .col_expr(
                crate::callisto::mega_refs::Column::RefTreeHash,
                sea_orm::sea_query::Expr::value("not-a-tree-oid"),
            )
            .filter(crate::callisto::mega_refs::Column::Path.eq("/"))
            .exec(&conn)
            .await
            .expect("corrupt root points to existing corrupt model");
        let before = fence_snapshot(&conn).await;
        let error = restore_active_ids(&mono, &[target.id])
            .await
            .expect_err("invalid stored hash");
        assert!(error.to_string().contains("invalid native tree hash"));
        assert_eq!(fence_snapshot(&conn).await, before);
    }

    #[tokio::test]
    async fn native_target_batch_budget_empty_ids_preserve_ambiguous_root_error() {
        let (_temp, _schema, conn, observer, log) = batch_budget_db().await;
        let mono = fence_mono(&conn);
        batch_seed_directories(&mono, 0, false).await;
        let root = crate::callisto::mega_refs::Entity::find()
            .one(&observer)
            .await
            .expect("root lookup")
            .expect("root model");
        // Deliberately corrupt only this private fixture's root-ref uniqueness;
        // trace_target_one_active_path remains installed and unchanged.
        conn.execute_unprepared("DROP INDEX uniq_mref_path")
            .await
            .expect("private ambiguous-root fault fixture");
        let mut duplicate: crate::callisto::mega_refs::ActiveModel = root.into();
        duplicate.id = Set(crate::common::utils::generate_id());
        duplicate
            .insert(&conn)
            .await
            .expect("private ambiguous selected root");
        let before = fence_snapshot(&observer).await;
        let txn = conn.begin().await.expect("measured transaction");
        batch_metric_probe(&txn, &log).await;
        let error = restore_active_ids_in_txn(&mono, &txn, &[])
            .await
            .expect_err("n=0 still checks root ambiguity");
        assert!(
            error
                .to_string()
                .contains("selected native ref is ambiguous")
        );
        let entries = log.lock().expect("metric log").clone();
        txn.rollback().await.expect("ambiguous root rollback");
        batch_assert_queries(&entries, [1, 0, 0, 0, 0]);
        assert_eq!(fence_snapshot(&observer).await, before);
    }

    async fn batch_tree_barrier(conn: &sea_orm::DatabaseConnection) -> DatabaseTransaction {
        let txn = conn
            .begin()
            .await
            .expect("private tree barrier transaction");
        let schema: String = txn
            .query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT current_schema() AS schema".to_owned(),
            ))
            .await
            .expect("private schema identity")
            .expect("schema row")
            .try_get("", "schema")
            .expect("schema name");
        assert!(
            schema.starts_with("mega2_test_"),
            "barrier must be private: {schema}"
        );
        txn.execute_unprepared("LOCK TABLE mega_tree IN ACCESS EXCLUSIVE MODE")
            .await
            .expect("test-only metadata-read barrier");
        txn
    }

    async fn batch_waits_at_statement(
        conn: &sea_orm::DatabaseConnection,
        waiter: i32,
        blocker: i32,
        statement_fragment: &str,
    ) -> bool {
        if !fence_waits_on(conn, waiter, blocker).await {
            return false;
        }
        // Each observation uses a fresh autocommit statement, not a cached
        // pg_stat_activity snapshot held by the blocking transaction.
        let row = conn
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT state = 'active' AND wait_event_type = 'Lock'
                AND strpos(query, $3::text) > 0
                AND $2::integer = ANY(pg_blocking_pids(pid)) AS matched
             FROM pg_stat_activity WHERE pid = $1::integer",
                [waiter.into(), blocker.into(), statement_fragment.into()],
            ))
            .await;
        match row {
            Ok(Some(row)) => row.try_get::<bool>("", "matched").unwrap_or(false),
            _ => false,
        }
    }

    #[tokio::test]
    async fn native_target_fence_batch_owner_commits_after_query_before_update_is_preserved() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let target = fence_inactive(&conn, "/a").await;
        let before = fence_snapshot(&conn).await;
        let barrier = batch_tree_barrier(&conn).await;
        let barrier_pid = fence_pid(&barrier).await;
        let restoring = conn.begin().await.expect("restoring transaction");
        let restoring_pid = fence_pid(&restoring).await;
        let id = target.id.clone();
        let task = tokio::spawn(async move {
            match restore_active_ids_in_txn(&mono, &restoring, &[id]).await {
                Ok(()) => restoring.commit().await.map_err(MegaError::from),
                Err(error) => {
                    restoring.rollback().await?;
                    Err(error)
                }
            }
        });
        let blocked =
            batch_waits_at_statement(&conn, restoring_pid, barrier_pid, "FROM \"mega_tree\"").await;
        if !blocked {
            let released = barrier.rollback().await;
            fence_abort_all(vec![task]).await;
            released.expect("release private tree barrier");
            panic!("restore did not reach the tree SELECT after its owner query");
        }
        let owner = trace_target_storage::insert_active(
            &conn,
            trace_target_storage::TARGET_KIND_IMPORT,
            "/a",
            Some("private-import-race".to_owned()),
        )
        .await;
        let owner = match owner {
            Ok(owner) => owner,
            Err(error) => {
                let released = barrier.rollback().await;
                fence_abort_all(vec![task]).await;
                released.expect("release private tree barrier");
                panic!("competing owner insert failed: {error}");
            }
        };
        let released = barrier.commit().await;
        if let Err(error) = released {
            fence_abort_all(vec![task]).await;
            panic!("private tree barrier release failed: {error}");
        }
        fence_join(task).await;
        let mut expected = before;
        expected.push(owner.clone());
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(fence_snapshot(&conn).await, expected);
        assert_eq!(
            trace_target_storage::active_at(&conn, "/a")
                .await
                .expect("active owner"),
            Some(owner)
        );
    }

    #[tokio::test]
    async fn native_target_fence_batch_uncommitted_owner_commit_errors_and_rolls_back_all() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let targets = [
            fence_inactive(&conn, "/a").await,
            fence_inactive(&conn, "/b").await,
        ];
        let before = fence_snapshot(&conn).await;
        let barrier = batch_tree_barrier(&conn).await;
        let barrier_pid = fence_pid(&barrier).await;
        let competing = conn.begin().await.expect("competing owner transaction");
        let competing_pid = fence_pid(&competing).await;
        let restoring = conn.begin().await.expect("restoring transaction");
        let restoring_pid = fence_pid(&restoring).await;
        let prior =
            trace_target_storage::insert_active(&restoring, TARGET_KIND_NATIVE, "/prior", None)
                .await
                .expect("real prior write owned by restore transaction");
        let ids = targets.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        let task = tokio::spawn(async move {
            let outcome = restore_active_ids_in_txn(&mono, &restoring, &ids).await;
            restoring.rollback().await?;
            match outcome {
                Err(error) if error.to_string().contains("trace_target_one_active_path") => Ok(()),
                Err(error) => Err(error),
                Ok(()) => Err(MegaError::Other(
                    "expected unique conflict after competing commit".to_owned(),
                )),
            }
        });
        let blocked =
            batch_waits_at_statement(&conn, restoring_pid, barrier_pid, "FROM \"mega_tree\"").await;
        if !blocked {
            let released = barrier.rollback().await;
            let rolled_back = competing.rollback().await;
            fence_abort_all(vec![task]).await;
            released.expect("release tree barrier");
            rolled_back.expect("release empty competing transaction");
            panic!("restore did not reach tree SELECT after its owner query");
        }
        let owner =
            trace_target_storage::insert_active(&competing, TARGET_KIND_NATIVE, "/a", None).await;
        let owner = match owner {
            Ok(owner) => owner,
            Err(error) => {
                let released = barrier.rollback().await;
                let rolled_back = competing.rollback().await;
                fence_abort_all(vec![task]).await;
                released.expect("release tree barrier");
                rolled_back.expect("release competing transaction");
                panic!("uncommitted competing owner insert failed: {error}");
            }
        };
        if let Err(error) = barrier.commit().await {
            let rolled_back = competing.rollback().await;
            fence_abort_all(vec![task]).await;
            rolled_back.expect("release competing transaction");
            panic!("tree barrier release failed: {error}");
        }
        let blocked = batch_waits_at_statement(
            &conn,
            restoring_pid,
            competing_pid,
            "UPDATE \"trace_target\"",
        )
        .await;
        if !blocked {
            let rolled_back = competing.rollback().await;
            fence_abort_all(vec![task]).await;
            rolled_back.expect("release competing transaction");
            panic!("restore UPDATE did not wait on the uncommitted owner's uniqueness check");
        }
        if let Err(error) = competing.commit().await {
            fence_abort_all(vec![task]).await;
            panic!("competing commit failed: {error}");
        }
        fence_join(task).await;
        let mut expected = before;
        expected.push(owner.clone());
        expected.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(fence_snapshot(&conn).await, expected);
        assert!(
            trace_target_storage::find_by_id(&conn, &prior.id)
                .await
                .expect("prior lookup")
                .is_none()
        );
        assert_eq!(
            trace_target_storage::active_at(&conn, "/a")
                .await
                .expect("active owner"),
            Some(owner)
        );
    }

    #[tokio::test]
    async fn native_target_fence_batch_uncommitted_owner_rollback_allows_restore() {
        let (_temp, conn) = db().await;
        let mono = fence_mono(&conn);
        fence_seed_root(&mono).await;
        let targets = [
            fence_inactive(&conn, "/a").await,
            fence_inactive(&conn, "/b").await,
        ];
        let before = fence_snapshot(&conn).await;
        let barrier = batch_tree_barrier(&conn).await;
        let barrier_pid = fence_pid(&barrier).await;
        let competing = conn.begin().await.expect("competing owner transaction");
        let competing_pid = fence_pid(&competing).await;
        let restoring = conn.begin().await.expect("restoring transaction");
        let restoring_pid = fence_pid(&restoring).await;
        let ids = targets.iter().map(|row| row.id.clone()).collect::<Vec<_>>();
        let task = tokio::spawn(async move {
            match restore_active_ids_in_txn(&mono, &restoring, &ids).await {
                Ok(()) => restoring.commit().await.map_err(MegaError::from),
                Err(error) => {
                    restoring.rollback().await?;
                    Err(error)
                }
            }
        });
        let blocked =
            batch_waits_at_statement(&conn, restoring_pid, barrier_pid, "FROM \"mega_tree\"").await;
        if !blocked {
            let released = barrier.rollback().await;
            let rolled_back = competing.rollback().await;
            fence_abort_all(vec![task]).await;
            released.expect("release tree barrier");
            rolled_back.expect("release empty competing transaction");
            panic!("restore did not reach tree SELECT after its owner query");
        }
        let owner = trace_target_storage::insert_active(
            &competing,
            trace_target_storage::TARGET_KIND_IMPORT,
            "/a",
            Some("private-uncommitted-import".to_owned()),
        )
        .await;
        let owner = match owner {
            Ok(owner) => owner,
            Err(error) => {
                let released = barrier.rollback().await;
                let rolled_back = competing.rollback().await;
                fence_abort_all(vec![task]).await;
                released.expect("release tree barrier");
                rolled_back.expect("release competing transaction");
                panic!("uncommitted competing owner insert failed: {error}");
            }
        };
        if let Err(error) = barrier.commit().await {
            let rolled_back = competing.rollback().await;
            fence_abort_all(vec![task]).await;
            rolled_back.expect("release competing transaction");
            panic!("tree barrier release failed: {error}");
        }
        let blocked = batch_waits_at_statement(
            &conn,
            restoring_pid,
            competing_pid,
            "UPDATE \"trace_target\"",
        )
        .await;
        if !blocked {
            let rolled_back = competing.rollback().await;
            fence_abort_all(vec![task]).await;
            rolled_back.expect("release competing transaction");
            panic!("restore UPDATE did not wait on the uncommitted owner's uniqueness check");
        }
        if let Err(error) = competing.rollback().await {
            fence_abort_all(vec![task]).await;
            panic!("competing rollback failed: {error}");
        }
        fence_join(task).await;
        let expected: Vec<_> = before
            .into_iter()
            .map(|mut row| {
                row.active = true;
                row
            })
            .collect();
        assert_eq!(fence_snapshot(&conn).await, expected);
        assert!(
            trace_target_storage::find_by_id(&conn, &owner.id)
                .await
                .expect("rolled back owner lookup")
                .is_none()
        );
    }
}
