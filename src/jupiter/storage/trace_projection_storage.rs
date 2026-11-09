//! Durable projection rows for one indexed trace journal entry.
//!
//! The publication tip stays on `trace_stream.tip_oid`. This module never
//! writes that column. The indexed tip is the journal row that reached
//! `indexed`.

use std::time::Duration;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    EntityTrait, QueryFilter, QueryOrder, Set, Statement, TransactionTrait,
};
use uuid::Uuid;

use crate::{
    callisto::{
        trace_checkpoint, trace_code_link, trace_ref_update, trace_resource, trace_session,
        trace_stream,
    },
    ceres::traces::parser::{CaptureSchema, CheckpointParse},
    common::errors::MegaError,
    jupiter::service::{
        trace_object_service::TraceObjectService,
        trace_projection_service::{ProjectionControl, TraceProjectionService},
    },
};

struct CheckpointWrite<'a> {
    stream_id: &'a str,
    hash_kind: &'a str,
    code_commit: Option<&'a str>,
    index_version: i64,
}

#[derive(Debug)]
pub struct TraceIndexRebuildSnapshot {
    pub stream_id: String,
    pub accepted_seq: i64,
    pub indexed_seq: i64,
    accepted_tip: Option<String>,
    rows: Vec<trace_ref_update::Model>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceIndexRebuildResult {
    pub accepted_seq: i64,
    pub indexed_seq: i64,
    pub rebuilt_seq: i64,
}

/// One pass over the durable ref journal.
pub struct TraceProjectionStore<'a> {
    db: &'a DatabaseConnection,
    objects: &'a TraceObjectService,
}

impl<'a> TraceProjectionStore<'a> {
    pub fn new(db: &'a DatabaseConnection, objects: &'a TraceObjectService) -> Self {
        Self { db, objects }
    }

    /// Index every currently claimable journal row.
    ///
    /// A row is claimable only when every lower `ref_sequence` for that stream
    /// is already `indexed`. The accepted tip is not moved.
    pub async fn index_available(&self) -> Result<usize, MegaError> {
        let now = Utc::now().fixed_offset();
        let rows = trace_ref_update::Entity::find()
            .filter(
                Condition::any()
                    .add(trace_ref_update::Column::State.eq("pending"))
                    .add(
                        Condition::all()
                            .add(trace_ref_update::Column::State.eq("claimed"))
                            .add(trace_ref_update::Column::ClaimUntil.lt(now)),
                    ),
            )
            .order_by_asc(trace_ref_update::Column::StreamId)
            .order_by_asc(trace_ref_update::Column::RefSequence)
            .all(self.db)
            .await?;
        let mut indexed = 0;
        for row in rows {
            if !self.prefix_is_indexed(&row).await? {
                continue;
            }
            if self.claim(&row).await? {
                self.project_claimed(&row).await?;
                indexed += 1;
            }
        }
        Ok(indexed)
    }

    async fn prefix_is_indexed(&self, row: &trace_ref_update::Model) -> Result<bool, MegaError> {
        if row.ref_sequence <= 1 {
            return Ok(true);
        }
        let previous = trace_ref_update::Entity::find()
            .filter(trace_ref_update::Column::StreamId.eq(row.stream_id.clone()))
            .filter(trace_ref_update::Column::RefSequence.eq(row.ref_sequence - 1))
            .one(self.db)
            .await?;
        Ok(previous
            .as_ref()
            .is_some_and(|found| found.state == "indexed"))
    }

    async fn claim(&self, row: &trace_ref_update::Model) -> Result<bool, MegaError> {
        let backend = self.db.get_database_backend();
        let result = self
            .db
            .execute_raw(Statement::from_sql_and_values(
                backend,
                "UPDATE trace_ref_update
                 SET state = 'claimed',
                     claim_until = now() + interval '30 seconds',
                     attempt_count = attempt_count + 1
                 WHERE id = $1
                   AND (state = 'pending'
                        OR (state = 'claimed' AND claim_until < now()))",
                [row.id.clone().into()],
            ))
            .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn project_claimed(&self, row: &trace_ref_update::Model) -> Result<(), MegaError> {
        let stream = trace_stream::Entity::find_by_id(row.stream_id.clone())
            .one(self.db)
            .await?
            .ok_or_else(|| MegaError::Other("trace journal names a missing stream".into()))?;
        let accepted = stream.tip_oid.clone();
        let projected = TraceProjectionService::new(self.objects.clone())
            .checkpoints_at_commit(
                self.db,
                &stream.id,
                &row.new_tip,
                &ProjectionControl::new(Duration::from_secs(30)),
            )
            .await?;
        let txn = self.db.begin().await?;
        for (checkpoint, schema) in projected.checkpoints.iter().zip(projected.schemas.iter()) {
            self.write_checkpoint(
                &txn,
                checkpoint,
                schema,
                &CheckpointWrite {
                    stream_id: &stream.id,
                    hash_kind: &stream.hash_kind,
                    code_commit: projected.code_commit.as_deref(),
                    index_version: crate::ceres::traces::fragments::FRAGMENT_INDEX_VERSION,
                },
            )
            .await?;
        }
        let updated = txn
            .execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE trace_ref_update
                 SET state = 'indexed', claim_until = NULL
                 WHERE id = $1 AND state = 'claimed'",
                [row.id.clone().into()],
            ))
            .await?;
        if updated.rows_affected() != 1 {
            txn.rollback().await?;
            return Ok(());
        }
        txn.commit().await?;
        let after = trace_stream::Entity::find_by_id(stream.id)
            .one(self.db)
            .await?
            .ok_or_else(|| MegaError::Other("trace stream disappeared during indexing".into()))?;
        if after.tip_oid != accepted {
            return Err(MegaError::Other(
                "trace index worker changed the accepted tip".into(),
            ));
        }
        Ok(())
    }

    /// Capture the accepted and contiguous indexed watermarks with one
    /// repeatable-read snapshot.
    pub async fn rebuild_snapshot(
        &self,
        stream_id: &str,
    ) -> Result<TraceIndexRebuildSnapshot, MegaError> {
        let txn = self.db.begin().await?;
        txn.execute_unprepared("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await?;
        let stream = trace_stream::Entity::find_by_id(stream_id.to_owned())
            .one(&txn)
            .await?
            .ok_or_else(|| MegaError::Other("trace stream does not exist".into()))?;
        let rows = trace_ref_update::Entity::find()
            .filter(trace_ref_update::Column::StreamId.eq(stream_id))
            .order_by_asc(trace_ref_update::Column::RefSequence)
            .all(&txn)
            .await?;
        let accepted_seq = rows.last().map_or(0, |row| row.ref_sequence);
        let mut indexed_seq = 0;
        let mut indexed_rows = Vec::new();
        for row in rows {
            if row.ref_sequence != indexed_seq + 1 || row.state != "indexed" {
                break;
            }
            indexed_seq = row.ref_sequence;
            indexed_rows.push(row);
        }
        txn.commit().await?;
        Ok(TraceIndexRebuildSnapshot {
            stream_id: stream_id.to_owned(),
            accepted_seq,
            indexed_seq,
            accepted_tip: stream.tip_oid,
            rows: indexed_rows,
        })
    }

    /// Replay the fixed indexed prefix into a new fragment version.
    ///
    /// Each journal row commits atomically. Replaying a completed row is
    /// idempotent, so a later invocation can resume after an interruption.
    pub async fn rebuild_indexed<F>(
        &self,
        snapshot: TraceIndexRebuildSnapshot,
        index_version: i64,
        mut progress: F,
    ) -> Result<TraceIndexRebuildResult, MegaError>
    where
        F: FnMut(i64),
    {
        if index_version < 1 {
            return Err(MegaError::Other(
                "trace fragment index version must be positive".into(),
            ));
        }
        let stream_id = snapshot.stream_id.as_str();
        let mut rebuilt_seq = 0;
        for row in snapshot.rows {
            let stream = trace_stream::Entity::find_by_id(row.stream_id.clone())
                .one(self.db)
                .await?
                .ok_or_else(|| MegaError::Other("trace journal names a missing stream".into()))?;
            if stream.tip_oid != snapshot.accepted_tip {
                return Err(MegaError::Other(
                    "accepted trace tip changed during index rebuild".into(),
                ));
            }
            let projected = TraceProjectionService::new(self.objects.clone())
                .checkpoints_at_commit(
                    self.db,
                    &stream.id,
                    &row.new_tip,
                    &ProjectionControl::new(Duration::from_secs(30)),
                )
                .await?;
            let txn = self.db.begin().await?;
            for (checkpoint, schema) in projected.checkpoints.iter().zip(projected.schemas.iter()) {
                self.write_checkpoint(
                    &txn,
                    checkpoint,
                    schema,
                    &CheckpointWrite {
                        stream_id: &stream.id,
                        hash_kind: &stream.hash_kind,
                        code_commit: projected.code_commit.as_deref(),
                        index_version,
                    },
                )
                .await?;
            }
            txn.commit().await?;
            let after = trace_stream::Entity::find_by_id(stream.id)
                .one(self.db)
                .await?
                .ok_or_else(|| {
                    MegaError::Other("trace stream disappeared during version rebuild".into())
                })?;
            if after.tip_oid != snapshot.accepted_tip {
                return Err(MegaError::Other(
                    "trace index rebuild changed the accepted tip".into(),
                ));
            }
            rebuilt_seq = row.ref_sequence;
            progress(rebuilt_seq);
        }
        let after = trace_stream::Entity::find_by_id(stream_id.to_owned())
            .one(self.db)
            .await?
            .ok_or_else(|| {
                MegaError::Other("trace stream disappeared during version rebuild".into())
            })?;
        if after.tip_oid != snapshot.accepted_tip {
            return Err(MegaError::Other(
                "accepted trace tip changed during index rebuild".into(),
            ));
        }
        Ok(TraceIndexRebuildResult {
            accepted_seq: snapshot.accepted_seq,
            indexed_seq: snapshot.indexed_seq,
            rebuilt_seq,
        })
    }

    async fn write_checkpoint(
        &self,
        txn: &impl ConnectionTrait,
        checkpoint: &CheckpointParse,
        schema: &CaptureSchema,
        write: &CheckpointWrite<'_>,
    ) -> Result<(), MegaError> {
        let stream_id = write.stream_id;
        let hash_kind = write.hash_kind;
        let code_commit = write.code_commit;
        let index_version = write.index_version;
        let (key, tree) = match checkpoint {
            CheckpointParse::Parsed(parsed) => (
                parsed.location.checkpoint_id.clone(),
                parsed.location.tree_oid.clone(),
            ),
            CheckpointParse::Invalid { key, tree_oid, .. } => (key.clone(), tree_oid.clone()),
        };
        let existing = trace_checkpoint::Entity::find()
            .filter(trace_checkpoint::Column::StreamId.eq(stream_id))
            .filter(trace_checkpoint::Column::CheckpointKey.eq(key.clone()))
            .one(txn)
            .await?;
        let checkpoint_id = if let Some(existing) = existing {
            existing.id
        } else {
            let id = Uuid::new_v4().to_string();
            trace_checkpoint::ActiveModel {
                id: Set(id.clone()),
                stream_id: Set(stream_id.to_owned()),
                checkpoint_key: Set(key.clone()),
                original_tree: Set(tree),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(txn)
            .await?;
            id
        };
        match schema {
            CaptureSchema::Supported {
                agent_kind: _,
                created_at: _,
                redacted: _,
                missing_raw: _,
            } => {
                self.ensure_session(txn, stream_id, &key).await?;
                let resource_id = self.ensure_resource(txn, stream_id, &checkpoint_id).await?;
                self.ensure_code_link(txn, stream_id, &checkpoint_id, code_commit)
                    .await?;
                if let CheckpointParse::Parsed(parsed) = checkpoint {
                    let fragments = self
                        .transcript_fragments(stream_id, hash_kind, &parsed.manifest_oid)
                        .await?;
                    crate::jupiter::storage::trace_fragment_storage::store_fragments(
                        txn,
                        stream_id,
                        &checkpoint_id,
                        &resource_id,
                        index_version,
                        &fragments,
                    )
                    .await?;
                }
            }
            CaptureSchema::Unsupported | CaptureSchema::Invalid { .. } => {
                self.ensure_session(txn, stream_id, &format!("unsupported:{key}"))
                    .await?;
            }
        }
        Ok(())
    }

    async fn ensure_session(
        &self,
        txn: &impl ConnectionTrait,
        stream_id: &str,
        session_key: &str,
    ) -> Result<(), MegaError> {
        let existing = trace_session::Entity::find()
            .filter(trace_session::Column::StreamId.eq(stream_id))
            .filter(trace_session::Column::SessionKey.eq(session_key))
            .one(txn)
            .await?;
        if existing.is_none() {
            trace_session::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                stream_id: Set(stream_id.to_owned()),
                session_key: Set(session_key.to_owned()),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(txn)
            .await?;
        }
        Ok(())
    }

    async fn ensure_resource(
        &self,
        txn: &impl ConnectionTrait,
        stream_id: &str,
        checkpoint_id: &str,
    ) -> Result<String, MegaError> {
        let existing = trace_resource::Entity::find()
            .filter(trace_resource::Column::CheckpointId.eq(checkpoint_id))
            .filter(trace_resource::Column::Ordinal.eq(0))
            .one(txn)
            .await?;
        if let Some(existing) = existing {
            return Ok(existing.id);
        }
        let id = Uuid::new_v4().to_string();
        trace_resource::ActiveModel {
            id: Set(id.clone()),
            stream_id: Set(stream_id.to_owned()),
            checkpoint_id: Set(checkpoint_id.to_owned()),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(0),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(txn)
        .await?;
        Ok(id)
    }

    async fn transcript_fragments(
        &self,
        stream_id: &str,
        hash_kind: &str,
        manifest_oid: &str,
    ) -> Result<Vec<crate::ceres::traces::fragments::TextFragment>, MegaError> {
        let Some(manifest) = self
            .objects
            .get_raw(stream_id, hash_kind, manifest_oid)
            .await?
        else {
            return Ok(Vec::new());
        };
        let value: serde_json::Value = match serde_json::from_slice(&manifest) {
            Ok(value) => value,
            Err(_) => return Ok(Vec::new()),
        };
        let Some(transcript) = value.pointer("/entries/transcript") else {
            return Ok(Vec::new());
        };
        let mut parts = Vec::new();
        if let Some(declared) = transcript
            .get("parts")
            .and_then(serde_json::Value::as_array)
        {
            for part in declared {
                let Some(oid) = part.get("oid").and_then(serde_json::Value::as_str) else {
                    return Ok(Vec::new());
                };
                let Some(bytes) = self.objects.get_raw(stream_id, hash_kind, oid).await? else {
                    return Ok(Vec::new());
                };
                parts.push(bytes);
            }
        } else if let Some(oid) = transcript.get("oid").and_then(serde_json::Value::as_str) {
            let Some(bytes) = self.objects.get_raw(stream_id, hash_kind, oid).await? else {
                return Ok(Vec::new());
            };
            parts.push(bytes);
        }
        let logical = crate::ceres::traces::fragments::reassemble_parts(&parts);
        Ok(crate::ceres::traces::fragments::split_utf8(&logical).unwrap_or_default())
    }

    async fn ensure_code_link(
        &self,
        txn: &impl ConnectionTrait,
        stream_id: &str,
        checkpoint_id: &str,
        code_commit: Option<&str>,
    ) -> Result<(), MegaError> {
        let existing = trace_code_link::Entity::find()
            .filter(trace_code_link::Column::CheckpointId.eq(checkpoint_id))
            .one(txn)
            .await?;
        if existing.is_some() {
            return Ok(());
        }
        let (code_oid, link_state) =
            match code_commit.map(str::trim).filter(|value| !value.is_empty()) {
                Some(oid) => (Some(oid.to_owned()), "resolved"),
                None => (None, "unresolved"),
            };
        trace_code_link::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.to_owned()),
            checkpoint_id: Set(checkpoint_id.to_owned()),
            code_oid: Set(code_oid),
            link_state: Set(link_state.to_owned()),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(txn)
        .await?;
        Ok(())
    }
}
