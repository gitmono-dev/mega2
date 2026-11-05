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
                &stream.id,
                checkpoint,
                schema,
                projected.code_commit.as_deref(),
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

    async fn write_checkpoint(
        &self,
        txn: &impl ConnectionTrait,
        stream_id: &str,
        checkpoint: &CheckpointParse,
        schema: &CaptureSchema,
        code_commit: Option<&str>,
    ) -> Result<(), MegaError> {
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
                self.ensure_resource(txn, stream_id, &checkpoint_id).await?;
                self.ensure_code_link(txn, stream_id, &checkpoint_id, code_commit)
                    .await?;
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
    ) -> Result<(), MegaError> {
        let existing = trace_resource::Entity::find()
            .filter(trace_resource::Column::CheckpointId.eq(checkpoint_id))
            .filter(trace_resource::Column::Ordinal.eq(0))
            .one(txn)
            .await?;
        if existing.is_none() {
            trace_resource::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                stream_id: Set(stream_id.to_owned()),
                checkpoint_id: Set(checkpoint_id.to_owned()),
                ordinal: Set(0),
                byte_start: Set(0),
                byte_end: Set(0),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(txn)
            .await?;
        }
        Ok(())
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
