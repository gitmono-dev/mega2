//! FU-15: rewrite legacy alias `git_repo.repo_path` values (double or
//! trailing slashes, `.` segments written before TP-08) to their canonical
//! form, keeping `repo_id` (plan-20260923).
//!
//! Only the ImportRepo protocol dispatch inserts `git_repo` rows, so each row
//! is judged on its own path; no `monorepo.import_dir` is needed, and any
//! import root (not only `/third-party`) is covered. When the canonical path
//! already belongs to another row (the identity has split), the alias row is
//! left untouched and counted as a collision. Rows are visited by `id`, so the
//! oldest alias of a free canonical path is the one rewritten.
//!
//! The rewrite runs in one transaction that first takes `SHARE ROW
//! EXCLUSIVE` on `git_repo`, so a row inserted by a still-running instance
//! is either committed before the scan (and seen) or waits until the rewrite
//! commits. `down` is a no-op: the alias spellings carry no meaning and
//! cannot be restored.

use std::collections::HashSet;

use sea_orm::{ConnectionTrait, DbBackend, Statement};
use sea_orm_migration::prelude::*;

use crate::common::utils::canonicalize_mono_ref_path;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AliasRewrite {
    pub rewritten: usize,
    pub collisions: usize,
    pub invalid: usize,
}

/// Must run inside a transaction (`LOCK TABLE` refuses to run outside one).
pub(crate) async fn canonicalize_import_repo_paths<C: ConnectionTrait>(
    conn: &C,
) -> Result<AliasRewrite, DbErr> {
    conn.execute_unprepared("LOCK TABLE git_repo IN SHARE ROW EXCLUSIVE MODE")
        .await?;
    let rows = conn
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT id, repo_path FROM git_repo ORDER BY id",
        ))
        .await?
        .iter()
        .map(|row| {
            Ok((
                row.try_get::<i64>("", "id")?,
                row.try_get::<String>("", "repo_path")?,
            ))
        })
        .collect::<Result<Vec<_>, DbErr>>()?;
    let mut taken: HashSet<String> = rows.iter().map(|(_, path)| path.clone()).collect();
    let mut summary = AliasRewrite::default();
    for (id, path) in rows {
        let Ok(canonical) = canonicalize_mono_ref_path(&path) else {
            tracing::warn!(
                repo_id = id,
                "git_repo path cannot be canonicalized; left as is"
            );
            summary.invalid += 1;
            continue;
        };
        if canonical == path {
            continue;
        }
        if taken.contains(&canonical) {
            tracing::warn!(
                repo_id = id,
                canonical = %canonical,
                "ImportRepo alias path left as is: the canonical path belongs to another repository"
            );
            summary.collisions += 1;
            continue;
        }
        conn.execute_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE git_repo SET repo_path = $1, updated_at = now() WHERE id = $2",
            [canonical.clone().into(), id.into()],
        ))
        .await?;
        taken.remove(&path);
        taken.insert(canonical);
        summary.rewritten += 1;
    }
    Ok(summary)
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let summary = canonicalize_import_repo_paths(manager.get_connection()).await?;
        tracing::info!(
            rewritten = summary.rewritten,
            collisions = summary.collisions,
            invalid = summary.invalid,
            "canonicalized ImportRepo alias paths"
        );
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }

    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }
}
