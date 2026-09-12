//! TP-13: blob_paths appearance index (ADR-TP-11).
//!
//! Queue updates carry a row-level CAS (`indexed_push_id IS NULL OR < $id`).
//! Review updates only `IS NULL` rows. Delete-aware reconciliation clears
//! appearances that left the indexed subtree. A periodic compensator re-scans
//! the current root so a lagging task's re-insert cannot persist.

use std::{collections::HashSet, time::Duration};

use git_internal::internal::object::tree::Tree;
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, Statement,
    sea_query::LikeExpr,
};
use tokio_util::sync::CancellationToken;

use crate::{
    callisto::{blob_paths, mega_blob, push_queue, sea_orm_active_enums::PushQueueStatusEnum},
    common::{errors::MegaError, utils::escape_like},
    jupiter::{
        storage::{base_storage::StorageConnector, mono_storage::MonoStorage},
        utils::converter::FromMegaModel,
    },
};

/// Default compensation interval (not a `[monorepo]` key; TP-15 owns config).
pub const DEFAULT_COMPENSATE_INTERVAL: Duration = Duration::from_secs(60);

/// How an index task writes `indexed_push_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobPathIndexMode {
    /// Queue C-segment: may overwrite NULL and older queue watermarks.
    Queue { push_id: i64 },
    /// Review morphology: insert new rows; never overwrite a queue watermark.
    Review,
    /// Compensator: insert missing rows, delete extras, never change watermarks.
    Compensate,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobPathIndexStats {
    pub upserted: u64,
    pub deleted: u64,
    pub skipped: bool,
}

/// One `(blob_id, path)` appearance collected from a tree walk.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobPathAppearance {
    pub blob_id: String,
    pub path: String,
}

/// Periodic re-scan of the current root tree against `blob_paths`.
#[derive(Clone)]
pub struct BlobPathCompensator {
    mono: MonoStorage,
}

impl BlobPathCompensator {
    pub fn new(mono: MonoStorage) -> Self {
        Self { mono }
    }

    pub async fn compensate_once(&self) -> Result<BlobPathIndexStats, MegaError> {
        self.mono.compensate_blob_paths().await
    }

    pub fn spawn_background(self, interval: Duration, shutdown: CancellationToken) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if shutdown.is_cancelled() {
                            break;
                        }
                        if let Err(e) = self.compensate_once().await {
                            tracing::error!(error = %e, "blob_path compensator cycle failed");
                        }
                    }
                }
            }
        });
    }
}

pub fn normalize_index_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return "/".to_owned();
    }
    let mut p = trimmed.to_owned();
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

fn join_blob_path(parent: &str, name: &str) -> String {
    if parent.is_empty() || parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

impl MonoStorage {
    /// Walk `tree_hash` (the subtree at `path_prefix`) and collect blob appearances.
    pub async fn collect_blob_appearances(
        &self,
        tree_hash: &str,
        path_prefix: &str,
    ) -> Result<Vec<BlobPathAppearance>, MegaError> {
        let prefix = normalize_index_prefix(path_prefix);
        let mut appearances = Vec::new();
        let mut stack = vec![(tree_hash.to_owned(), prefix)];
        while let Some((hash, path)) = stack.pop() {
            let Some(model) = self.get_tree_by_hash(&hash).await? else {
                tracing::debug!(tree = %hash, "blob_path walk: tree missing, skip subtree");
                continue;
            };
            let tree = Tree::from_mega_model(model);
            for item in tree.tree_items {
                let child = join_blob_path(&path, &item.name);
                if item.is_tree() {
                    stack.push((item.id.to_string(), child));
                } else {
                    appearances.push(BlobPathAppearance {
                        blob_id: item.id.to_string(),
                        path: child,
                    });
                }
            }
        }
        Ok(appearances)
    }

    /// C-segment entry: skip if a later same-path Done row exists, else index
    /// the current `main@path` tree.
    pub async fn index_blob_paths_c_segment(
        &self,
        path: &str,
        mode: BlobPathIndexMode,
    ) -> Result<BlobPathIndexStats, MegaError> {
        if let BlobPathIndexMode::Queue { push_id } = mode
            && self.same_path_has_later_done(path, push_id).await?
        {
            return Ok(BlobPathIndexStats {
                skipped: true,
                ..BlobPathIndexStats::default()
            });
        }
        let Some(r) = self.get_main_ref(path).await? else {
            return Ok(BlobPathIndexStats::default());
        };
        self.index_tree_blob_paths(&r.ref_tree_hash, path, mode)
            .await
    }

    /// Index a known tree under `path_prefix` (review / tests / compensator).
    pub async fn index_tree_blob_paths(
        &self,
        tree_hash: &str,
        path_prefix: &str,
        mode: BlobPathIndexMode,
    ) -> Result<BlobPathIndexStats, MegaError> {
        if let BlobPathIndexMode::Queue { push_id } = mode
            && self.same_path_has_later_done(path_prefix, push_id).await?
        {
            return Ok(BlobPathIndexStats {
                skipped: true,
                ..BlobPathIndexStats::default()
            });
        }
        let appearances = self
            .collect_blob_appearances(tree_hash, path_prefix)
            .await?;
        self.apply_blob_path_index(&appearances, path_prefix, mode)
            .await
    }

    /// Rescan `main@/` and converge `blob_paths` to the current tip subtree.
    pub async fn compensate_blob_paths(&self) -> Result<BlobPathIndexStats, MegaError> {
        let Some(root) = self.get_main_ref("/").await? else {
            return Ok(BlobPathIndexStats::default());
        };
        self.index_tree_blob_paths(&root.ref_tree_hash, "/", BlobPathIndexMode::Compensate)
            .await
    }

    pub async fn same_path_has_later_done(
        &self,
        path: &str,
        push_id: i64,
    ) -> Result<bool, MegaError> {
        let normalized = normalize_index_prefix(path);
        let found = push_queue::Entity::find()
            .filter(push_queue::Column::Path.eq(normalized))
            .filter(push_queue::Column::Id.gt(push_id))
            .filter(push_queue::Column::Status.eq(PushQueueStatusEnum::Done))
            .one(self.get_connection())
            .await?;
        Ok(found.is_some())
    }

    pub async fn list_blob_paths(&self) -> Result<Vec<blob_paths::Model>, MegaError> {
        Ok(blob_paths::Entity::find()
            .all(self.get_connection())
            .await?)
    }

    pub async fn apply_blob_path_index(
        &self,
        appearances: &[BlobPathAppearance],
        path_prefix: &str,
        mode: BlobPathIndexMode,
    ) -> Result<BlobPathIndexStats, MegaError> {
        let prefix = normalize_index_prefix(path_prefix);
        let pairs: Vec<(String, String)> = appearances
            .iter()
            .map(|appearance| (appearance.blob_id.clone(), appearance.path.clone()))
            .collect();
        self.update_blob_filepaths(pairs).await?;
        let mut upserted = 0u64;
        for appearance in appearances {
            if self
                .upsert_blob_path(&appearance.blob_id, &appearance.path, mode)
                .await?
            {
                upserted += 1;
            }
        }
        let deleted = self
            .reconcile_blob_paths_under_prefix(&prefix, appearances, mode)
            .await?;
        Ok(BlobPathIndexStats {
            upserted,
            deleted,
            skipped: false,
        })
    }

    /// Row-level CAS / isolation insert. Returns true when a row was written.
    pub async fn upsert_blob_path(
        &self,
        blob_id: &str,
        path: &str,
        mode: BlobPathIndexMode,
    ) -> Result<bool, MegaError> {
        let path = normalize_index_prefix(path);
        // File paths must not collapse to the prefix root.
        let path = if path == "/" {
            return Ok(false);
        } else {
            path
        };
        let sql = match mode {
            BlobPathIndexMode::Queue { push_id } => {
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    r#"INSERT INTO blob_paths (blob_id, path, indexed_push_id)
                       VALUES ($1, $2, $3)
                       ON CONFLICT (blob_id, path) DO UPDATE
                       SET indexed_push_id = EXCLUDED.indexed_push_id
                       WHERE blob_paths.indexed_push_id IS NULL
                          OR blob_paths.indexed_push_id < EXCLUDED.indexed_push_id"#,
                    [blob_id.into(), path.clone().into(), push_id.into()],
                );
                self.get_connection().execute_raw(stmt).await?
            }
            BlobPathIndexMode::Review => {
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    r#"INSERT INTO blob_paths (blob_id, path, indexed_push_id)
                       VALUES ($1, $2, NULL)
                       ON CONFLICT (blob_id, path) DO UPDATE
                       SET indexed_push_id = blob_paths.indexed_push_id
                       WHERE blob_paths.indexed_push_id IS NULL"#,
                    [blob_id.into(), path.clone().into()],
                );
                self.get_connection().execute_raw(stmt).await?
            }
            BlobPathIndexMode::Compensate => {
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    r#"INSERT INTO blob_paths (blob_id, path, indexed_push_id)
                       VALUES ($1, $2, NULL)
                       ON CONFLICT (blob_id, path) DO NOTHING"#,
                    [blob_id.into(), path.clone().into()],
                );
                self.get_connection().execute_raw(stmt).await?
            }
        };
        Ok(sql.rows_affected() > 0)
    }

    /// TP-15 / 4.1 ⑥: morphology switch resets every watermark to NULL.
    pub async fn reset_blob_path_index_watermarks(&self) -> Result<u64, MegaError> {
        let stmt = Statement::from_string(
            DbBackend::Postgres,
            "UPDATE blob_paths SET indexed_push_id = NULL".to_owned(),
        );
        let res = self.get_connection().execute_raw(stmt).await?;
        Ok(res.rows_affected())
    }

    async fn reconcile_blob_paths_under_prefix(
        &self,
        prefix: &str,
        appearances: &[BlobPathAppearance],
        mode: BlobPathIndexMode,
    ) -> Result<u64, MegaError> {
        // Review must not delete queue-indexed rows (isolation). Compensator and
        // queue tasks both clear extras in the indexed subtree.
        let keep: HashSet<(String, String)> = appearances
            .iter()
            .map(|a| (a.blob_id.clone(), normalize_index_prefix(&a.path)))
            .collect();
        let existing = self.blob_paths_under_prefix(prefix).await?;
        let mut deleted = 0u64;
        for row in existing {
            if keep.contains(&(row.blob_id.clone(), row.path.clone())) {
                continue;
            }
            if matches!(mode, BlobPathIndexMode::Review) && row.indexed_push_id.is_some() {
                continue;
            }
            let res = blob_paths::Entity::delete_many()
                .filter(blob_paths::Column::BlobId.eq(row.blob_id.clone()))
                .filter(blob_paths::Column::Path.eq(row.path.clone()))
                .exec(self.get_connection())
                .await?;
            deleted += res.rows_affected;
        }
        Ok(deleted)
    }

    pub async fn blob_paths_under_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<blob_paths::Model>, MegaError> {
        let prefix = normalize_index_prefix(prefix);
        let like = if prefix == "/" {
            "/%".to_owned()
        } else {
            format!("{}/%", escape_like(&prefix))
        };
        Ok(blob_paths::Entity::find()
            .filter(
                Condition::any()
                    .add(blob_paths::Column::Path.eq(prefix.clone()))
                    .add(blob_paths::Column::Path.like(LikeExpr::new(like).escape('\\'))),
            )
            .all(self.get_connection())
            .await?)
    }

    /// Display-path fallback: latest indexed path, else `mega_blob.file_path`.
    pub async fn latest_blob_display_path(
        &self,
        blob_id: &str,
    ) -> Result<Option<String>, MegaError> {
        if let Some(row) = blob_paths::Entity::find()
            .filter(blob_paths::Column::BlobId.eq(blob_id))
            .one(self.get_connection())
            .await?
        {
            return Ok(Some(row.path));
        }
        Ok(mega_blob::Entity::find()
            .filter(mega_blob::Column::BlobId.eq(blob_id))
            .one(self.get_connection())
            .await?
            .map(|m| m.file_path))
    }
}
