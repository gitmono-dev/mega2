//! Stable trace target rows. A path locates the current project. The UUID
//! stays when that path is moved and changes when the path is created again.

use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, Set,
    TransactionTrait, sea_query::LikeExpr,
};
use uuid::Uuid;

pub use crate::callisto::trace_target::Model;
use crate::{
    callisto::trace_target,
    common::{errors::MegaError, utils::escape_like},
};

pub const TARGET_KIND_NATIVE: &str = "native";
pub const TARGET_KIND_IMPORT: &str = "import";

/// Canonical project path: one leading slash, no trailing slash, no `.` or `..`.
pub fn canonical_project_path(raw: &str) -> Result<String, MegaError> {
    if raw.is_empty() || raw.trim().is_empty() {
        return Err(MegaError::Other("target path is empty".into()));
    }
    let with_root = if raw.starts_with('/') {
        raw.to_owned()
    } else {
        format!("/{raw}")
    };
    let mut path = with_root;
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    if path == "/" {
        return Ok(path);
    }
    if path
        .split('/')
        .skip(1)
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(MegaError::Other(
            "target path is not a canonical project path".into(),
        ));
    }
    Ok(path)
}

/// Reject a path that is not already in canonical form. No rewrite is applied.
pub fn strict_canonical_project_path(raw: &str) -> Result<String, MegaError> {
    let path = canonical_project_path(raw)?;
    if raw != path {
        return Err(MegaError::Other(
            "target path is not a canonical project path".into(),
        ));
    }
    Ok(path)
}

fn descendant_like(path: &str) -> String {
    if path == "/" {
        "/%".to_owned()
    } else {
        format!("{}/%", escape_like(path))
    }
}

fn path_is_under(parent: &str, child: &str) -> bool {
    if parent == "/" {
        child != "/"
    } else {
        child.starts_with(&format!("{parent}/"))
    }
}

fn relocated_path(from: &str, to: &str, current: &str) -> String {
    if current == from {
        return to.to_owned();
    }
    let suffix = if from == "/" {
        current.to_owned()
    } else {
        current[from.len()..].to_owned()
    };
    if to == "/" {
        suffix
    } else {
        format!("{to}{suffix}")
    }
}

pub async fn active_at<C: ConnectionTrait>(
    conn: &C,
    path: &str,
) -> Result<Option<Model>, MegaError> {
    let path = canonical_project_path(path)?;
    Ok(trace_target::Entity::find()
        .filter(trace_target::Column::ProjectPath.eq(path))
        .filter(trace_target::Column::Active.eq(true))
        .one(conn)
        .await?)
}

pub async fn find_by_id<C: ConnectionTrait>(
    conn: &C,
    id: &str,
) -> Result<Option<Model>, MegaError> {
    Ok(trace_target::Entity::find_by_id(id.to_owned())
        .one(conn)
        .await?)
}

/// Active row at `path`, plus active rows under that path.
pub async fn active_at_and_below<C: ConnectionTrait>(
    conn: &C,
    path: &str,
) -> Result<Vec<Model>, MegaError> {
    let path = canonical_project_path(path)?;
    let like = descendant_like(&path);
    Ok(trace_target::Entity::find()
        .filter(trace_target::Column::Active.eq(true))
        .filter(
            Condition::any()
                .add(trace_target::Column::ProjectPath.eq(&path))
                .add(trace_target::Column::ProjectPath.like(LikeExpr::new(like).escape('\\'))),
        )
        .all(conn)
        .await?)
}

pub async fn insert_active<C: ConnectionTrait>(
    conn: &C,
    kind: &str,
    path: &str,
    backing_import_repo_id: Option<String>,
) -> Result<Model, MegaError> {
    let path = canonical_project_path(path)?;
    if kind != TARGET_KIND_NATIVE && kind != TARGET_KIND_IMPORT {
        return Err(MegaError::Other(
            "target kind is not native or import".into(),
        ));
    }
    let row = trace_target::ActiveModel {
        id: Set(Uuid::new_v4().to_string()),
        target_kind: Set(kind.to_owned()),
        project_path: Set(path),
        backing_import_repo_id: Set(backing_import_repo_id),
        active: Set(true),
        created_at: Set(chrono::Utc::now().fixed_offset()),
    };
    Ok(row.insert(conn).await?)
}

/// First active binding for a path. A later call returns the same UUID.
pub async fn bind_first_write<C: ConnectionTrait>(
    conn: &C,
    kind: &str,
    path: &str,
) -> Result<Model, MegaError> {
    if let Some(existing) = active_at(conn, path).await? {
        return Ok(existing);
    }
    insert_active(conn, kind, path, None).await
}

/// Move the active target and its descendants. The UUIDs stay.
/// The path itself must already have an active target.
pub async fn move_active_tree<C>(conn: &C, from: &str, to: &str) -> Result<Model, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>,
{
    let from = canonical_project_path(from)?;
    let root = active_at(conn, &from)
        .await?
        .ok_or_else(|| MegaError::Other("active target is missing".into()))?;
    let id = root.id.clone();
    move_covered_targets(conn, &from, to).await?;
    find_by_id(conn, &id)
        .await?
        .ok_or_else(|| MegaError::Other("moved target is missing".into()))
}

/// Move every active target at `from` and below it. An unbound `from` still
/// moves active descendants. An empty match moves nothing. The reads and
/// updates share one transaction, so a conflict leaves every row where it was.
pub async fn move_covered_targets<C>(
    conn: &C,
    from: &str,
    to: &str,
) -> Result<Vec<String>, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>,
{
    let from = canonical_project_path(from)?;
    let to = canonical_project_path(to)?;
    if path_is_under(&from, &to) {
        return Err(MegaError::Other(
            "cannot move a target into its own subtree".into(),
        ));
    }
    let txn = conn.begin().await?;
    let outcome = async {
        let rows = active_at_and_below(&txn, &from).await?;
        if rows.is_empty() || from == to {
            return Ok(rows.into_iter().map(|row| row.id).collect());
        }
        let moving: std::collections::HashSet<String> =
            rows.iter().map(|row| row.id.clone()).collect();
        let mut plan = Vec::with_capacity(rows.len());
        for row in rows {
            let next = relocated_path(&from, &to, &row.project_path);
            if let Some(other) = active_at(&txn, &next).await?
                && !moving.contains(&other.id)
            {
                return Err(MegaError::Other(
                    "destination already has an active target".into(),
                ));
            }
            plan.push((row, next));
        }
        plan.sort_by_key(|(row, _)| std::cmp::Reverse(row.project_path.len()));
        let mut ids = Vec::with_capacity(plan.len());
        for (row, next) in plan {
            ids.push(row.id.clone());
            let mut active: trace_target::ActiveModel = row.into();
            active.project_path = Set(next);
            active.update(&txn).await?;
        }
        Ok(ids)
    }
    .await;
    finish_write(txn, outcome).await
}

/// Deactivate the path and its descendants. Rows stay, with `active = false`.
/// One failed update restores every row in this call.
pub async fn deactivate_tree<C>(conn: &C, path: &str) -> Result<Vec<String>, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>,
{
    let txn = conn.begin().await?;
    let outcome = async {
        let rows = active_at_and_below(&txn, path).await?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.id.clone());
            let mut active: trace_target::ActiveModel = row.into();
            active.active = Set(false);
            active.update(&txn).await?;
        }
        Ok(ids)
    }
    .await;
    finish_write(txn, outcome).await
}

/// Deactivate only these ids, and only while they are still active under `path`.
/// A row created after the id list was taken is left alone.
pub async fn deactivate_ids_still_under<C>(
    conn: &C,
    ids: &[String],
    path: &str,
) -> Result<Vec<String>, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>,
{
    let path = canonical_project_path(path)?;
    let txn = conn.begin().await?;
    let outcome = async {
        let mut retired = Vec::new();
        for id in ids {
            let Some(row) = find_by_id(&txn, id).await? else {
                continue;
            };
            if !row.active {
                continue;
            }
            let still_here = if path == "/" {
                true
            } else {
                row.project_path == path || row.project_path.starts_with(&format!("{path}/"))
            };
            if !still_here {
                continue;
            }
            retired.push(id.clone());
            let mut active: trace_target::ActiveModel = row.into();
            active.active = Set(false);
            active.update(&txn).await?;
        }
        Ok(retired)
    }
    .await;
    finish_write(txn, outcome).await
}

/// Move only these ids while they are still under `from`, active or not.
/// Other rows at the destination stay where they are.
pub async fn move_recorded_ids<C>(
    conn: &C,
    ids: &[String],
    from: &str,
    to: &str,
) -> Result<Vec<String>, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>,
{
    let from = canonical_project_path(from)?;
    let to = canonical_project_path(to)?;
    let txn = conn.begin().await?;
    let outcome = async {
        let mut moved = Vec::new();
        for id in ids {
            let Some(row) = find_by_id(&txn, id).await? else {
                continue;
            };
            if !path_is_at_or_under(&from, &row.project_path) {
                continue;
            }
            let next = relocated_path(&from, &to, &row.project_path);
            if let Some(other) = active_at(&txn, &next).await?
                && other.id != row.id
            {
                continue;
            }
            moved.push(id.clone());
            let mut active: trace_target::ActiveModel = row.into();
            active.project_path = Set(next);
            active.update(&txn).await?;
        }
        Ok(moved)
    }
    .await;
    finish_write(txn, outcome).await
}

fn path_is_at_or_under(parent: &str, child: &str) -> bool {
    child == parent || path_is_under(parent, child)
}

/// Turn these ids active again when their current path has no active target.
/// A path that already has another active row is left unchanged.
pub async fn reactivate_ids_if_path_free<C>(
    conn: &C,
    ids: &[String],
) -> Result<Vec<String>, MegaError>
where
    C: ConnectionTrait + TransactionTrait<Transaction = sea_orm::DatabaseTransaction>,
{
    let txn = conn.begin().await?;
    let outcome = async {
        let mut restored = Vec::new();
        for id in ids {
            let Some(row) = find_by_id(&txn, id).await? else {
                continue;
            };
            if row.active {
                continue;
            }
            if active_at(&txn, &row.project_path).await?.is_some() {
                continue;
            }
            restored.push(id.clone());
            let mut active: trace_target::ActiveModel = row.into();
            active.active = Set(true);
            active.update(&txn).await?;
        }
        Ok(restored)
    }
    .await;
    finish_write(txn, outcome).await
}

async fn finish_write(
    txn: sea_orm::DatabaseTransaction,
    outcome: Result<Vec<String>, MegaError>,
) -> Result<Vec<String>, MegaError> {
    match outcome {
        Ok(ids) => {
            txn.commit().await?;
            Ok(ids)
        }
        Err(err) => {
            let _ = txn.rollback().await;
            Err(err)
        }
    }
}

/// Point one target at a new path. The caller must have shown that the path exists.
pub async fn rebind_existing_path<C: ConnectionTrait>(
    conn: &C,
    target_id: &str,
    new_path: &str,
    path_exists: bool,
) -> Result<Model, MegaError> {
    let new_path = strict_canonical_project_path(new_path)?;
    if !path_exists {
        return Err(MegaError::Other("new canonical path does not exist".into()));
    }
    let Some(row) = find_by_id(conn, target_id).await? else {
        return Err(MegaError::Other("target is missing".into()));
    };
    if row.active
        && let Some(other) = active_at(conn, &new_path).await?
        && other.id != row.id
    {
        return Err(MegaError::Other(
            "destination already has an active target".into(),
        ));
    }
    let id = row.id.clone();
    let mut active: trace_target::ActiveModel = row.into();
    active.project_path = Set(new_path);
    active.update(conn).await?;
    find_by_id(conn, &id)
        .await?
        .ok_or_else(|| MegaError::Other("rebound target is missing".into()))
}

/// Mark one row inactive. Descendants are left alone.
pub async fn deactivate_by_id<C: ConnectionTrait>(conn: &C, id: &str) -> Result<(), MegaError> {
    let Some(row) = find_by_id(conn, id).await? else {
        return Ok(());
    };
    if !row.active {
        return Ok(());
    }
    let mut active: trace_target::ActiveModel = row.into();
    active.active = Set(false);
    active.update(conn).await?;
    Ok(())
}

/// A matching tree OID is not a reason to change the target.
pub fn unchanged_for_tree_oid(target: &Model, tree_oid: &str) -> Model {
    let _ = tree_oid;
    target.clone()
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

    #[test]
    fn target_path_storage_rejects_non_canonical_paths() {
        assert!(canonical_project_path("").is_err());
        assert!(canonical_project_path("/project/../app").is_err());
        assert!(canonical_project_path("/project/./app").is_err());
        assert!(canonical_project_path("/project//app").is_err());
        assert_eq!(
            canonical_project_path("project/app").expect("relative"),
            "/project/app"
        );
        assert_eq!(
            canonical_project_path("/project/app/").expect("slash"),
            "/project/app"
        );
        assert_eq!(canonical_project_path("/").expect("root"), "/");
        assert_eq!(
            canonical_project_path("/project/foo ").expect("trailing space stays"),
            "/project/foo "
        );
        assert!(canonical_project_path("   ").is_err());
    }

    #[tokio::test]
    async fn target_path_storage_first_write_move_delete_and_rebuild() {
        let (_temp, conn) = db().await;
        let first = bind_first_write(&conn, TARGET_KIND_NATIVE, "/app")
            .await
            .expect("first bind");
        let again = bind_first_write(&conn, TARGET_KIND_NATIVE, "app/")
            .await
            .expect("same path");
        assert_eq!(again.id, first.id);
        let child = bind_first_write(&conn, TARGET_KIND_NATIVE, "/app/lib")
            .await
            .expect("child");
        let neighbor = bind_first_write(&conn, TARGET_KIND_NATIVE, "/apple")
            .await
            .expect("neighbor");

        let moved = move_active_tree(&conn, "/app", "/relocated")
            .await
            .expect("move");
        assert_eq!(moved.id, first.id);
        assert_eq!(moved.project_path, "/relocated");
        let moved_child = active_at(&conn, "/relocated/lib")
            .await
            .expect("child query")
            .expect("child moved");
        assert_eq!(moved_child.id, child.id);
        assert!(active_at(&conn, "/app").await.expect("old path").is_none());
        assert_eq!(
            active_at(&conn, "/apple")
                .await
                .expect("neighbor query")
                .expect("neighbor stays")
                .id,
            neighbor.id
        );

        let conflict = bind_first_write(&conn, TARGET_KIND_NATIVE, "/taken")
            .await
            .expect("taken");
        let refused = move_active_tree(&conn, "/relocated", "/taken")
            .await
            .expect_err("destination occupied");
        assert!(refused.to_string().contains("destination"));
        assert_eq!(
            active_at(&conn, "/taken")
                .await
                .expect("taken stays")
                .unwrap()
                .id,
            conflict.id
        );
        assert_eq!(
            active_at(&conn, "/relocated")
                .await
                .expect("source stays")
                .unwrap()
                .id,
            first.id
        );

        let retired = deactivate_tree(&conn, "/relocated")
            .await
            .expect("deactivate");
        assert_eq!(retired.len(), 2);
        assert!(
            active_at(&conn, "/relocated")
                .await
                .expect("gone")
                .is_none()
        );
        assert!(
            active_at(&conn, "/relocated/lib")
                .await
                .expect("child gone")
                .is_none()
        );
        let rows = trace_target::Entity::find()
            .filter(trace_target::Column::Id.is_in([first.id.clone(), child.id.clone()]))
            .all(&conn)
            .await
            .expect("retired rows");
        assert!(rows.iter().all(|row| !row.active));
        assert_eq!(
            active_at(&conn, "/apple").await.expect("apple").unwrap().id,
            neighbor.id
        );

        let rebuilt = bind_first_write(&conn, TARGET_KIND_NATIVE, "/relocated")
            .await
            .expect("rebuild");
        assert_ne!(rebuilt.id, first.id);
        assert!(rebuilt.active);
        let history = trace_target::Entity::find()
            .filter(trace_target::Column::ProjectPath.eq("/relocated"))
            .all(&conn)
            .await
            .expect("history");
        assert_eq!(history.len(), 2);
        assert!(history.iter().any(|row| row.id == first.id && !row.active));
    }

    #[tokio::test]
    async fn target_path_storage_move_descendants_without_root_target() {
        let (_temp, conn) = db().await;
        let child = bind_first_write(&conn, TARGET_KIND_NATIVE, "/project/x/lib")
            .await
            .expect("child");
        let moved = move_covered_targets(&conn, "/project/x", "/project/y")
            .await
            .expect("move descendants");
        assert_eq!(moved, vec![child.id.clone()]);
        let at_dest = active_at(&conn, "/project/y/lib")
            .await
            .expect("dest")
            .expect("child moved");
        assert_eq!(at_dest.id, child.id);
        assert!(
            active_at(&conn, "/project/x/lib")
                .await
                .expect("old")
                .is_none()
        );
    }

    #[tokio::test]
    async fn target_path_storage_reactivate_skips_a_path_that_has_a_new_row() {
        let (_temp, conn) = db().await;
        let first = bind_first_write(&conn, TARGET_KIND_NATIVE, "/project/app")
            .await
            .expect("first");
        deactivate_by_id(&conn, &first.id)
            .await
            .expect("deactivate");
        let restored = reactivate_ids_if_path_free(&conn, std::slice::from_ref(&first.id))
            .await
            .expect("restore");
        assert_eq!(restored, vec![first.id.clone()]);
        assert!(
            active_at(&conn, "/project/app")
                .await
                .expect("active")
                .unwrap()
                .active
        );
        deactivate_by_id(&conn, &first.id)
            .await
            .expect("deactivate again");
        let newer = bind_first_write(&conn, TARGET_KIND_NATIVE, "/project/app")
            .await
            .expect("newer");
        let skipped = reactivate_ids_if_path_free(&conn, std::slice::from_ref(&first.id))
            .await
            .expect("skip");
        assert!(skipped.is_empty());
        assert_eq!(
            active_at(&conn, "/project/app")
                .await
                .expect("newer stays")
                .unwrap()
                .id,
            newer.id
        );
    }

    #[tokio::test]
    async fn target_path_storage_recorded_move_leaves_other_rows() {
        let (_temp, conn) = db().await;
        let mine = bind_first_write(&conn, TARGET_KIND_NATIVE, "/dest/mine")
            .await
            .expect("mine");
        let other = bind_first_write(&conn, TARGET_KIND_NATIVE, "/dest/other")
            .await
            .expect("other");
        let moved = move_recorded_ids(&conn, std::slice::from_ref(&mine.id), "/dest", "/src")
            .await
            .expect("move recorded");
        assert_eq!(moved, vec![mine.id.clone()]);
        assert_eq!(
            active_at(&conn, "/src/mine")
                .await
                .expect("mine dest")
                .unwrap()
                .id,
            mine.id
        );
        assert_eq!(
            active_at(&conn, "/dest/other")
                .await
                .expect("other stays")
                .unwrap()
                .id,
            other.id
        );
        deactivate_by_id(&conn, &mine.id).await.expect("deactivate");
        let again = move_recorded_ids(&conn, std::slice::from_ref(&mine.id), "/src", "/back")
            .await
            .expect("move inactive");
        assert_eq!(again, vec![mine.id.clone()]);
        let parked = find_by_id(&conn, &mine.id).await.expect("load").unwrap();
        assert_eq!(parked.project_path, "/back/mine");
        assert!(!parked.active);
    }

    #[tokio::test]
    async fn target_path_storage_rebind_requires_existing_path_and_ignores_tree_oid() {
        let (_temp, conn) = db().await;
        let target = bind_first_write(&conn, TARGET_KIND_NATIVE, "/app")
            .await
            .expect("bind");
        let tree_oid = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let same = unchanged_for_tree_oid(&target, tree_oid);
        assert_eq!(same.id, target.id);
        assert_eq!(same.project_path, target.project_path);
        assert_eq!(
            active_at(&conn, "/app")
                .await
                .expect("unchanged")
                .unwrap()
                .id,
            target.id
        );

        let missing = rebind_existing_path(&conn, &target.id, "/next", false)
            .await
            .expect_err("path must exist");
        assert!(missing.to_string().contains("does not exist"));
        assert_eq!(
            active_at(&conn, "/app")
                .await
                .expect("no write")
                .unwrap()
                .id,
            target.id
        );
        assert!(
            active_at(&conn, "/next")
                .await
                .expect("dest empty")
                .is_none()
        );

        let bad = rebind_existing_path(&conn, &target.id, "/next/../app", true)
            .await
            .expect_err("canonical");
        assert!(bad.to_string().contains("canonical"));
        let slash = rebind_existing_path(&conn, &target.id, "/next/", true)
            .await
            .expect_err("trailing slash");
        assert!(slash.to_string().contains("canonical"));
        assert_eq!(
            find_by_id(&conn, &target.id)
                .await
                .expect("load")
                .unwrap()
                .project_path,
            "/app"
        );

        let rebound = rebind_existing_path(&conn, &target.id, "/next", true)
            .await
            .expect("rebind");
        assert_eq!(rebound.id, target.id);
        assert_eq!(rebound.project_path, "/next");
        assert!(rebound.active);
        assert!(active_at(&conn, "/app").await.expect("old").is_none());
    }
}
