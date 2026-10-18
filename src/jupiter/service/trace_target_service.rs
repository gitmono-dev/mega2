//! Target path lifecycle. Callers pass the connection that owns the code write
//! when they already have a transaction.

use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseBackend, DatabaseTransaction, EntityTrait, Set,
    Statement, TransactionTrait,
};

use crate::{
    callisto::trace_target,
    common::errors::MegaError,
    jupiter::storage::{
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
}
