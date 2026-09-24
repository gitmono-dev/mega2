use std::ops::Deref;

use chrono::Utc;
use idgenerator::IdInstance;
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::Set,
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect,
    prelude::DateTimeWithTimeZone,
    sea_query::{Expr, Value as SeaValue},
};

use crate::{
    callisto::{
        audit_logs,
        sea_orm_active_enums::{ActorTypeEnum, AuditActionEnum, TargetTypeEnum},
    },
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Clone)]
pub struct AuditStorage {
    pub base: BaseStorage,
}

impl Deref for AuditStorage {
    type Target = BaseStorage;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl AuditStorage {
    /// Write an audit log entry for a given actor and target.
    ///
    /// `metadata` is stored as JSON for flexible, structured details per action.
    pub async fn log_audit(
        &self,
        actor_id: i64,
        actor_type: ActorTypeEnum,
        action: AuditActionEnum,
        target_type: TargetTypeEnum,
        target_id: i64,
        metadata: Option<serde_json::Value>,
    ) -> Result<audit_logs::Model, MegaError> {
        Self::log_audit_in_txn(
            self.get_connection(),
            actor_id,
            actor_type,
            action,
            target_type,
            target_id,
            metadata,
        )
        .await
    }

    /// [`Self::log_audit`] on a caller-supplied connection or transaction, so
    /// the audit row commits or rolls back with the write it records
    /// (plan-20260923 ADR-FU-08 item 1, ADR-FU-09 item 3).
    pub async fn log_audit_in_txn<C: ConnectionTrait>(
        conn: &C,
        actor_id: i64,
        actor_type: ActorTypeEnum,
        action: AuditActionEnum,
        target_type: TargetTypeEnum,
        target_id: i64,
        metadata: Option<serde_json::Value>,
    ) -> Result<audit_logs::Model, MegaError> {
        let created_at: DateTimeWithTimeZone = Utc::now().into();
        let model = audit_logs::ActiveModel {
            id: Set(IdInstance::next_id()),
            actor_id: Set(actor_id),
            actor_type: Set(actor_type),
            action: Set(action),
            target_type: Set(target_type),
            target_id: Set(target_id),
            metadata: Set(metadata),
            created_at: Set(created_at),
        };

        let inserted = model.insert(conn).await?;
        Ok(inserted)
    }

    /// Record that ImportRepo `repo_id` is mounted at `path` (its first
    /// successful attach), in the attach transaction (ADR-FU-08 item 1).
    pub async fn log_import_repo_attach_in_txn<C: ConnectionTrait>(
        conn: &C,
        repo_id: i64,
        path: &str,
    ) -> Result<audit_logs::Model, MegaError> {
        Self::log_audit_in_txn(
            conn,
            // Reserved actor: storage-only has no numeric user id.
            0,
            ActorTypeEnum::Human,
            AuditActionEnum::Create,
            TargetTypeEnum::Repository,
            repo_id,
            Some(serde_json::json!({
                "kind": IMPORT_REPO_ATTACH_KIND,
                "path": path,
            })),
        )
        .await
    }

    /// Whether ImportRepo `repo_id` has a mount provenance record for `path`.
    pub async fn has_import_repo_attach_in_txn<C: ConnectionTrait>(
        conn: &C,
        repo_id: i64,
        path: &str,
    ) -> Result<bool, MegaError> {
        let found: Option<i64> = Self::import_repo_attach_query(repo_id, path)
            .into_tuple()
            .one(conn)
            .await?;
        Ok(found.is_some())
    }

    /// The provenance lookup: `idx_audit_logs_target` narrows it to one
    /// repository's audit rows (its lifecycle events: at most one attach record
    /// per path, plus the cleanup phases), the metadata match runs in the
    /// database (PostgreSQL `->>`) and one row is enough.
    fn import_repo_attach_query(repo_id: i64, path: &str) -> sea_orm::Select<audit_logs::Entity> {
        audit_logs::Entity::find()
            .select_only()
            .column(audit_logs::Column::Id)
            .filter(audit_logs::Column::TargetType.eq(TargetTypeEnum::Repository))
            .filter(audit_logs::Column::TargetId.eq(repo_id))
            .filter(audit_logs::Column::Action.eq(AuditActionEnum::Create))
            .filter(Expr::cust_with_values(
                r#"(metadata ->> 'kind') = $1 AND (metadata ->> 'path') = $2"#,
                [
                    SeaValue::String(Some(IMPORT_REPO_ATTACH_KIND.to_owned())),
                    SeaValue::String(Some(path.to_owned())),
                ],
            ))
            .limit(1)
    }
}

/// `metadata.kind` of an ImportRepo mount provenance record.
pub const IMPORT_REPO_ATTACH_KIND: &str = "import_repo.attach";
/// `audit_logs.metadata.kind` of an ImportRepo cleanup (plan-20260923
/// ADR-FU-09 item 3); `phase` tells `detached` from `swept`.
pub const IMPORT_REPO_REMOVE_KIND: &str = "import_repo.remove";

#[cfg(test)]
mod tests {
    use sea_orm::{DbBackend, QueryTrait, Statement, TransactionTrait};

    use super::*;

    #[tokio::test]
    async fn log_audit_in_txn_rolls_back() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let audit = storage.audit_storage();
        let conn = audit.get_connection();

        let txn = conn.begin().await.unwrap();
        AuditStorage::log_import_repo_attach_in_txn(&txn, 7, "/third-party/x")
            .await
            .unwrap();
        assert!(
            AuditStorage::has_import_repo_attach_in_txn(&txn, 7, "/third-party/x")
                .await
                .unwrap()
        );
        txn.rollback().await.unwrap();
        assert!(
            !AuditStorage::has_import_repo_attach_in_txn(conn, 7, "/third-party/x")
                .await
                .unwrap(),
            "the record rolls back with its transaction"
        );

        // Decoys on the same target: another kind, another path, another action.
        for (action, kind, path) in [
            (
                AuditActionEnum::Create,
                "import_repo.remove",
                "/third-party/x",
            ),
            (
                AuditActionEnum::Create,
                IMPORT_REPO_ATTACH_KIND,
                "/third-party/x/sub",
            ),
            (
                AuditActionEnum::Delete,
                IMPORT_REPO_ATTACH_KIND,
                "/third-party/x",
            ),
        ] {
            AuditStorage::log_audit_in_txn(
                conn,
                0,
                ActorTypeEnum::Human,
                action,
                TargetTypeEnum::Repository,
                7,
                Some(serde_json::json!({ "kind": kind, "path": path })),
            )
            .await
            .unwrap();
        }
        assert!(
            !AuditStorage::has_import_repo_attach_in_txn(conn, 7, "/third-party/x")
                .await
                .unwrap(),
            "only an exact kind + path + create row counts"
        );

        let txn = conn.begin().await.unwrap();
        let logged = AuditStorage::log_import_repo_attach_in_txn(&txn, 7, "/third-party/x")
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let row = audit_logs::Entity::find_by_id(logged.id)
            .one(conn)
            .await
            .unwrap()
            .expect("committed record");
        assert_eq!(row.actor_id, 0);
        assert_eq!(row.actor_type, ActorTypeEnum::Human);
        assert_eq!(row.action, AuditActionEnum::Create);
        assert_eq!(row.target_type, TargetTypeEnum::Repository);
        assert_eq!(row.target_id, 7);
        assert_eq!(
            row.metadata,
            Some(serde_json::json!({
                "kind": IMPORT_REPO_ATTACH_KIND,
                "path": "/third-party/x",
            }))
        );
        assert!(
            AuditStorage::has_import_repo_attach_in_txn(conn, 7, "/third-party/x")
                .await
                .unwrap()
        );
        assert!(
            !AuditStorage::has_import_repo_attach_in_txn(conn, 7, "/third-party/y")
                .await
                .unwrap()
        );
        assert!(
            !AuditStorage::has_import_repo_attach_in_txn(conn, 8, "/third-party/x")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn import_repo_attach_lookup_uses_target_index() {
        let temp = tempfile::TempDir::new().unwrap();
        let storage = crate::jupiter::tests::test_storage(temp.path()).await;
        let audit = storage.audit_storage();
        let conn = audit.get_connection();
        // Many same-target decoys and rows of other repositories.
        for i in 0..200i64 {
            AuditStorage::log_audit_in_txn(
                conn,
                0,
                ActorTypeEnum::Human,
                AuditActionEnum::Create,
                TargetTypeEnum::Repository,
                if i % 2 == 0 { 7 } else { 1000 + i },
                Some(serde_json::json!({ "kind": "import_repo.remove", "path": format!("/third-party/{i}") })),
            )
            .await
            .unwrap();
        }
        assert!(
            !AuditStorage::has_import_repo_attach_in_txn(conn, 7, "/third-party/x")
                .await
                .unwrap()
        );
        let stmt =
            AuditStorage::import_repo_attach_query(7, "/third-party/x").build(DbBackend::Postgres);
        let txn = conn.begin().await.unwrap();
        txn.execute_unprepared("SET LOCAL enable_seqscan = off")
            .await
            .unwrap();
        let rows = txn
            .query_all_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                format!("EXPLAIN {}", stmt.sql),
                stmt.values.map(|v| v.0).unwrap_or_default(),
            ))
            .await
            .unwrap();
        txn.rollback().await.unwrap();
        let plan: Vec<String> = rows
            .iter()
            .map(|row| row.try_get::<String>("", "QUERY PLAN").unwrap())
            .collect();
        assert!(
            plan.iter()
                .any(|line| line.contains("idx_audit_logs_target")),
            "the lookup is driven by the target index: {plan:?}"
        );
    }
}
