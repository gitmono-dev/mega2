use std::ops::Deref;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder,
    QuerySelect, Set, TransactionTrait,
    sea_query::{LockType, OnConflict},
};

use crate::{
    callisto::{agent_capture_event, agent_capture_session, agent_capture_source_stream},
    common::{canonical_json, errors::MegaError},
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionNaturalKey {
    pub deployment_id: String,
    pub tenant_id: String,
    pub repo_id: String,
    pub producer_id: String,
    pub session_kind: String,
    pub client_session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InsertEvent {
    pub capture_id: i64,
    pub event_uid: String,
    pub event_kind: String,
    pub native_id: Option<String>,
    pub lifecycle_seq: Option<i64>,
    pub payload: serde_json::Value,
}

#[derive(Clone)]
pub struct AgentCaptureStorage {
    pub base: BaseStorage,
}

impl Deref for AgentCaptureStorage {
    type Target = BaseStorage;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl AgentCaptureStorage {
    fn session_by_natural_key(
        key: &SessionNaturalKey,
    ) -> sea_orm::Select<agent_capture_session::Entity> {
        agent_capture_session::Entity::find()
            .filter(agent_capture_session::Column::DeploymentId.eq(key.deployment_id.clone()))
            .filter(agent_capture_session::Column::TenantId.eq(key.tenant_id.clone()))
            .filter(agent_capture_session::Column::RepoId.eq(key.repo_id.clone()))
            .filter(agent_capture_session::Column::ProducerId.eq(key.producer_id.clone()))
            .filter(agent_capture_session::Column::SessionKind.eq(key.session_kind.clone()))
            .filter(
                agent_capture_session::Column::ClientSessionId.eq(key.client_session_id.clone()),
            )
    }

    /// Allocate or return the server `capture_id` for a session natural key.
    ///
    /// Locks the natural-key row when it exists; otherwise inserts. A unique
    /// conflict (concurrent first PUT) re-reads the winner. Receipts are not
    /// consulted and do not mint `capture_id`.
    pub async fn upsert_session(&self, key: SessionNaturalKey) -> Result<i64, MegaError> {
        let txn = self.get_connection().begin().await?;
        if let Some(existing) = Self::session_by_natural_key(&key)
            .lock(LockType::Update)
            .one(&txn)
            .await?
        {
            txn.commit().await?;
            return Ok(existing.id);
        }

        let insert = agent_capture_session::ActiveModel {
            deployment_id: Set(key.deployment_id.clone()),
            tenant_id: Set(key.tenant_id.clone()),
            repo_id: Set(key.repo_id.clone()),
            producer_id: Set(key.producer_id.clone()),
            session_kind: Set(key.session_kind.clone()),
            client_session_id: Set(key.client_session_id.clone()),
            completeness: Set("empty".to_owned()),
            ..Default::default()
        };

        let result = agent_capture_session::Entity::insert(insert)
            .on_conflict(
                OnConflict::columns([
                    agent_capture_session::Column::DeploymentId,
                    agent_capture_session::Column::TenantId,
                    agent_capture_session::Column::RepoId,
                    agent_capture_session::Column::ProducerId,
                    agent_capture_session::Column::SessionKind,
                    agent_capture_session::Column::ClientSessionId,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec(&txn)
            .await;

        let capture_id = match result {
            Ok(inserted) => inserted.last_insert_id,
            Err(err)
                if matches!(err, DbErr::RecordNotInserted) || is_unique_constraint_error(&err) =>
            {
                Self::session_by_natural_key(&key)
                    .lock(LockType::Update)
                    .one(&txn)
                    .await?
                    .ok_or_else(|| {
                        MegaError::Other(
                            "agent_capture_session unique conflict but row missing".to_owned(),
                        )
                    })?
                    .id
            }
            Err(err) => return Err(err.into()),
        };

        if capture_id == 0 {
            let existing = Self::session_by_natural_key(&key)
                .lock(LockType::Update)
                .one(&txn)
                .await?
                .ok_or_else(|| {
                    MegaError::Other(
                        "agent_capture_session insert reported no capture_id".to_owned(),
                    )
                })?;
            txn.commit().await?;
            return Ok(existing.id);
        }

        txn.commit().await?;
        Ok(capture_id)
    }

    /// Insert an event by `(capture_id, event_uid)`. Same payload fingerprint
    /// is a no-op; a different payload is a conflict. Ordinary inserts never
    /// downgrade `complete` / `truncated` session completeness.
    pub async fn insert_event(&self, event: InsertEvent) -> Result<i64, MegaError> {
        let fingerprint = canonical_json::fingerprint(&event.payload.to_string())?;
        let txn = self.get_connection().begin().await?;
        let session = agent_capture_session::Entity::find_by_id(event.capture_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!(
                    "agent_capture_session {} does not exist",
                    event.capture_id
                ))
            })?;

        if let Some(existing) = agent_capture_event::Entity::find()
            .filter(agent_capture_event::Column::CaptureId.eq(event.capture_id))
            .filter(agent_capture_event::Column::EventUid.eq(event.event_uid.clone()))
            .lock(LockType::Update)
            .one(&txn)
            .await?
        {
            if existing.payload_fingerprint == fingerprint {
                tracing::debug!(
                    capture_id = event.capture_id,
                    event_uid = %event.event_uid,
                    payload_fingerprint = %fingerprint,
                    "agent capture event idempotent"
                );
                txn.commit().await?;
                return Ok(existing.id);
            }
            txn.rollback().await?;
            return Err(MegaError::Other(format!(
                "agent_capture_event uid conflict for capture_id {}",
                event.capture_id
            )));
        }

        let inserted = agent_capture_event::Entity::insert(agent_capture_event::ActiveModel {
            capture_id: Set(event.capture_id),
            event_uid: Set(event.event_uid.clone()),
            event_kind: Set(event.event_kind),
            native_id: Set(event.native_id),
            lifecycle_seq: Set(event.lifecycle_seq),
            payload: Set(event.payload),
            payload_fingerprint: Set(fingerprint.clone()),
            ..Default::default()
        })
        .exec(&txn)
        .await?;

        if session.completeness == "empty" {
            let mut session = session.into_active_model();
            session.completeness = Set("incomplete".to_owned());
            session.update(&txn).await?;
        }

        tracing::debug!(
            capture_id = event.capture_id,
            event_uid = %event.event_uid,
            payload_fingerprint = %fingerprint,
            "agent capture event inserted"
        );
        txn.commit().await?;
        Ok(inserted.last_insert_id)
    }

    /// Advance a source-stream watermark. `(generation, byte_offset)` must
    /// not move backwards; a greater generation may reset offset.
    pub async fn advance_source_stream(
        &self,
        capture_id: i64,
        stream_kind: &str,
        generation: i64,
        byte_offset: i64,
    ) -> Result<(), MegaError> {
        let txn = self.get_connection().begin().await?;
        agent_capture_session::Entity::find_by_id(capture_id)
            .lock(LockType::Update)
            .one(&txn)
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("agent_capture_session {capture_id} does not exist"))
            })?;

        let current = agent_capture_source_stream::Entity::find()
            .filter(agent_capture_source_stream::Column::CaptureId.eq(capture_id))
            .filter(agent_capture_source_stream::Column::StreamKind.eq(stream_kind))
            .order_by_desc(agent_capture_source_stream::Column::Generation)
            .lock(LockType::Update)
            .one(&txn)
            .await?;

        if let Some(current) = current {
            if generation < current.generation
                || (generation == current.generation && byte_offset < current.byte_offset)
            {
                txn.rollback().await?;
                return Err(MegaError::Other(format!(
                    "agent_capture_source_stream watermark regression for capture_id {capture_id}"
                )));
            }
            if generation == current.generation {
                if byte_offset > current.byte_offset {
                    let mut current = current.into_active_model();
                    current.byte_offset = Set(byte_offset);
                    current.update(&txn).await?;
                }
                txn.commit().await?;
                return Ok(());
            }
        }

        let insert =
            agent_capture_source_stream::Entity::insert(agent_capture_source_stream::ActiveModel {
                capture_id: Set(capture_id),
                stream_kind: Set(stream_kind.to_owned()),
                generation: Set(generation),
                byte_offset: Set(byte_offset),
                ..Default::default()
            })
            .exec(&txn)
            .await;

        match insert {
            Ok(_) => {}
            Err(err)
                if matches!(err, DbErr::RecordNotInserted) || is_unique_constraint_error(&err) =>
            {
                let latest = agent_capture_source_stream::Entity::find()
                    .filter(agent_capture_source_stream::Column::CaptureId.eq(capture_id))
                    .filter(agent_capture_source_stream::Column::StreamKind.eq(stream_kind))
                    .filter(agent_capture_source_stream::Column::Generation.eq(generation))
                    .lock(LockType::Update)
                    .one(&txn)
                    .await?
                    .ok_or_else(|| {
                        MegaError::Other(
                            "agent_capture_source_stream unique conflict but row missing"
                                .to_owned(),
                        )
                    })?;
                if byte_offset < latest.byte_offset {
                    txn.rollback().await?;
                    return Err(MegaError::Other(format!(
                        "agent_capture_source_stream watermark regression for capture_id {capture_id}"
                    )));
                }
                if byte_offset > latest.byte_offset {
                    let mut latest = latest.into_active_model();
                    latest.byte_offset = Set(byte_offset);
                    latest.update(&txn).await?;
                }
            }
            Err(err) => return Err(err.into()),
        }
        txn.commit().await?;
        Ok(())
    }
}

fn is_unique_constraint_error(err: &sea_orm::DbErr) -> bool {
    let msg = err.to_string().to_lowercase();
    msg.contains("unique constraint")
        || msg.contains("unique violation")
        || msg.contains("duplicate key")
        || msg.contains("is not unique")
}

#[cfg(test)]
mod tests {
    use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait};

    use super::*;
    use crate::{
        callisto::{agent_capture_event, agent_capture_source_stream},
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    fn sample_key() -> SessionNaturalKey {
        SessionNaturalKey {
            deployment_id: "default".to_owned(),
            tenant_id: "default".to_owned(),
            repo_id: "/third-part/mega".to_owned(),
            producer_id: "hook".to_owned(),
            session_kind: "external_capture".to_owned(),
            client_session_id: "provider__abc".to_owned(),
        }
    }

    async fn storage() -> (tempfile::TempDir, AgentCaptureStorage) {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true)
            .await
            .expect("migrations should apply");
        (
            temp_dir,
            AgentCaptureStorage {
                base: BaseStorage::new(std::sync::Arc::new(db)),
            },
        )
    }

    #[tokio::test]
    async fn upsert_session_returns_capture_id() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage
            .upsert_session(sample_key())
            .await
            .expect("upsert session");
        assert!(capture_id > 0, "capture_id must be a positive i64");
    }

    #[tokio::test]
    async fn upsert_session_idempotent() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let first = storage
            .upsert_session(key.clone())
            .await
            .expect("first upsert");
        let second = storage
            .upsert_session(key.clone())
            .await
            .expect("second upsert");
        assert_eq!(first, second);
        let rows = AgentCaptureStorage::session_by_natural_key(&key)
            .count(storage.get_connection())
            .await
            .expect("count sessions");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn upsert_session_concurrent_first_insert_is_idempotent() {
        let (_temp_dir, storage) = storage().await;
        let key = sample_key();
        let left = storage.clone();
        let right = storage.clone();
        let (first, second) = tokio::join!(
            left.upsert_session(key.clone()),
            right.upsert_session(key.clone())
        );
        let first = first.expect("concurrent upsert left");
        let second = second.expect("concurrent upsert right");
        assert_eq!(first, second);
        assert!(first > 0);
        let rows = AgentCaptureStorage::session_by_natural_key(&key)
            .count(storage.get_connection())
            .await
            .expect("count sessions");
        assert_eq!(rows, 1);
    }

    fn sample_event(capture_id: i64, payload: serde_json::Value) -> InsertEvent {
        InsertEvent {
            capture_id,
            event_uid: "0:0".to_owned(),
            event_kind: "message".to_owned(),
            native_id: None,
            lifecycle_seq: None,
            payload,
        }
    }

    #[tokio::test]
    async fn event_uid_idempotent() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        let payload = serde_json::json!({"role": "user", "n": 1});
        let first = storage
            .insert_event(sample_event(capture_id, payload.clone()))
            .await
            .expect("first event");
        let second = storage
            .insert_event(sample_event(capture_id, payload))
            .await
            .expect("second event");
        assert_eq!(first, second);
        let rows = agent_capture_event::Entity::find()
            .filter(agent_capture_event::Column::CaptureId.eq(capture_id))
            .count(storage.get_connection())
            .await
            .expect("count events");
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn event_uid_conflict_different_payload() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .insert_event(sample_event(
                capture_id,
                serde_json::json!({"role": "user"}),
            ))
            .await
            .expect("first event");
        let err = storage
            .insert_event(sample_event(
                capture_id,
                serde_json::json!({"role": "assistant"}),
            ))
            .await
            .expect_err("different payload must conflict");
        assert!(err.to_string().contains("uid conflict"));
    }

    #[tokio::test]
    async fn source_stream_watermark_monotonic() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .advance_source_stream(capture_id, "jsonl", 0, 10)
            .await
            .expect("first watermark");
        storage
            .advance_source_stream(capture_id, "jsonl", 0, 5)
            .await
            .expect_err("offset regression");
        storage
            .advance_source_stream(capture_id, "jsonl", 1, 0)
            .await
            .expect("new generation may reset offset");
    }

    #[tokio::test]
    async fn source_stream_concurrent_generation_transition() {
        let (_temp_dir, storage) = storage().await;
        let capture_id = storage.upsert_session(sample_key()).await.expect("session");
        storage
            .advance_source_stream(capture_id, "jsonl", 0, 10)
            .await
            .expect("seed generation 0");
        let left = storage.clone();
        let right = storage.clone();
        let (first, second) = tokio::join!(
            left.advance_source_stream(capture_id, "jsonl", 1, 200),
            right.advance_source_stream(capture_id, "jsonl", 1, 200)
        );
        first.expect("concurrent generation left");
        second.expect("concurrent generation right");
        let row = agent_capture_source_stream::Entity::find()
            .filter(agent_capture_source_stream::Column::CaptureId.eq(capture_id))
            .filter(agent_capture_source_stream::Column::StreamKind.eq("jsonl"))
            .filter(agent_capture_source_stream::Column::Generation.eq(1))
            .one(storage.get_connection())
            .await
            .expect("load generation 1")
            .expect("generation 1 exists");
        assert_eq!(row.byte_offset, 200);
    }

    #[tokio::test]
    async fn event_requires_session() {
        let (_temp_dir, storage) = storage().await;
        storage
            .insert_event(sample_event(9_999_999, serde_json::json!({"ok": true})))
            .await
            .expect_err("missing session");
    }
}
