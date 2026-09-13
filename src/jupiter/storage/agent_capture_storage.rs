use std::ops::Deref;

use sea_orm::{
    ColumnTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, Set, TransactionTrait,
    sea_query::{LockType, OnConflict},
};

use crate::{
    callisto::agent_capture_session,
    common::errors::MegaError,
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
    use sea_orm::PaginatorTrait;

    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

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
}
