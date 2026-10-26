//! Explicit credential ownership and transactional binding lifecycle.

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseBackend, DatabaseConnection,
    DatabaseTransaction, DbErr, EntityTrait, FromQueryResult, QueryFilter, QueryOrder, QuerySelect,
    Set, SqlErr, Statement, TransactionTrait,
};
use uuid::Uuid;

use crate::{callisto::trace_credential_binding, config::PushTokenConfig};

type Binding = trace_credential_binding::Model;

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("a principal or binding ID must be a UUID")]
    InvalidUuid,
    #[error("use a configured static token name or a canonical Mono credential record ID")]
    InvalidCredential,
    #[error("the credential source does not exist")]
    SourceMissing,
    #[error("the credential already has an active binding or changed concurrently")]
    Conflict,
    #[error("the binding does not exist")]
    BindingMissing,
    #[error("the binding has already been retired")]
    BindingRetired,
    #[error("rotation requires a different credential")]
    SameCredential,
    #[error("credential storage failed; inspect database state before retrying")]
    Database(#[from] DbErr),
}

enum CredentialKey {
    Static(String),
    AccessToken(i64),
    SshKey(i64),
}

impl CredentialKey {
    fn parse(
        namespace: &str,
        key: &str,
        tokens: &[PushTokenConfig],
    ) -> Result<Self, CredentialError> {
        match namespace {
            "static_push_token" if !key.is_empty() => {
                if tokens.iter().any(|token| token.name == key) {
                    Ok(Self::Static(key.to_owned()))
                } else {
                    Err(CredentialError::SourceMissing)
                }
            }
            "mono_account" => {
                let (kind, raw_id) = key
                    .split_once(':')
                    .ok_or(CredentialError::InvalidCredential)?;
                let id: i64 = raw_id
                    .parse()
                    .map_err(|_| CredentialError::InvalidCredential)?;
                if id.to_string() != raw_id {
                    return Err(CredentialError::InvalidCredential);
                }
                match kind {
                    "access-token" => Ok(Self::AccessToken(id)),
                    "ssh-key" => Ok(Self::SshKey(id)),
                    _ => Err(CredentialError::InvalidCredential),
                }
            }
            _ => Err(CredentialError::InvalidCredential),
        }
    }

    fn namespace(&self) -> &'static str {
        match self {
            Self::Static(_) => "static_push_token",
            Self::AccessToken(_) | Self::SshKey(_) => "mono_account",
        }
    }

    fn identifier(&self) -> String {
        match self {
            Self::Static(name) => name.clone(),
            Self::AccessToken(id) => format!("access-token:{id}"),
            Self::SshKey(id) => format!("ssh-key:{id}"),
        }
    }

    async fn lock_source(&self, txn: &DatabaseTransaction) -> Result<(), CredentialError> {
        let (sql, id) = match self {
            Self::Static(_) => return Ok(()),
            Self::AccessToken(id) => ("SELECT id FROM access_token WHERE id = $1 FOR SHARE", *id),
            Self::SshKey(id) => ("SELECT id FROM ssh_keys WHERE id = $1 FOR SHARE", *id),
        };
        let source = txn
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                sql,
                [id.into()],
            ))
            .await?;
        source.ok_or(CredentialError::SourceMissing)?;
        Ok(())
    }
}

pub struct TraceCredentialStorage<'a> {
    connection: &'a DatabaseConnection,
    tokens: &'a [PushTokenConfig],
}

impl<'a> TraceCredentialStorage<'a> {
    pub fn new(connection: &'a DatabaseConnection, tokens: &'a [PushTokenConfig]) -> Self {
        Self { connection, tokens }
    }

    pub async fn bind(
        &self,
        principal: &str,
        namespace: &str,
        credential: &str,
    ) -> Result<Binding, CredentialError> {
        let principal = canonical_uuid(principal)?;
        let key = CredentialKey::parse(namespace, credential, self.tokens)?;
        let txn = self.connection.begin().await?;
        let result = async {
            key.lock_source(&txn).await?;
            txn.execute_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "INSERT INTO trace_principal (id) VALUES ($1) ON CONFLICT (id) DO NOTHING",
                [principal.clone().into()],
            ))
            .await?;
            if let Some(row) = insert_binding(&txn, &principal, &key, true).await? {
                return Ok(row);
            }
            let existing = trace_credential_binding::Entity::find()
                .filter(trace_credential_binding::Column::Namespace.eq(key.namespace()))
                .filter(trace_credential_binding::Column::CredentialId.eq(key.identifier()))
                .filter(trace_credential_binding::Column::RetiredAt.is_null())
                .lock_exclusive()
                .one(&txn)
                .await?;
            match existing {
                Some(row) if row.principal_id == principal => Ok(row),
                _ => Err(CredentialError::Conflict),
            }
        }
        .await;
        finish(txn, result).await
    }

    pub async fn rotate(
        &self,
        binding: &str,
        namespace: &str,
        credential: &str,
    ) -> Result<Binding, CredentialError> {
        let binding = canonical_uuid(binding)?;
        let key = CredentialKey::parse(namespace, credential, self.tokens)?;
        let txn = self.connection.begin().await?;
        let result = async {
            key.lock_source(&txn).await?;
            let rows = trace_credential_binding::Entity::find()
                .filter(
                    Condition::any()
                        .add(trace_credential_binding::Column::Id.eq(&binding))
                        .add(
                            Condition::all()
                                .add(
                                    trace_credential_binding::Column::Namespace.eq(key.namespace()),
                                )
                                .add(
                                    trace_credential_binding::Column::CredentialId
                                        .eq(key.identifier()),
                                )
                                .add(trace_credential_binding::Column::RetiredAt.is_null()),
                        ),
                )
                .order_by_asc(trace_credential_binding::Column::Id)
                .lock_exclusive()
                .all(&txn)
                .await?;
            let old = rows
                .iter()
                .find(|row| row.id == binding)
                .ok_or(CredentialError::BindingMissing)?;
            if old.retired_at.is_some() {
                return Err(CredentialError::BindingRetired);
            }
            if old.namespace == key.namespace() && old.credential_id == key.identifier() {
                return Err(CredentialError::SameCredential);
            }
            if rows.iter().any(|row| row.id != binding) {
                return Err(CredentialError::Conflict);
            }
            let replacement = insert_binding(&txn, &old.principal_id, &key, false)
                .await?
                .ok_or(CredentialError::Conflict)?;
            let mut retired: trace_credential_binding::ActiveModel = old.clone().into();
            retired.retired_at = Set(Some(Utc::now().fixed_offset()));
            retired.update(&txn).await?;
            Ok(replacement)
        }
        .await;
        finish(txn, result).await
    }

    pub async fn retire(&self, binding: &str) -> Result<Binding, CredentialError> {
        let binding = canonical_uuid(binding)?;
        let txn = self.connection.begin().await?;
        let result = async {
            let row = trace_credential_binding::Entity::find_by_id(binding)
                .lock_exclusive()
                .one(&txn)
                .await?
                .ok_or(CredentialError::BindingMissing)?;
            if row.retired_at.is_some() {
                return Ok(row);
            }
            let mut retired: trace_credential_binding::ActiveModel = row.into();
            retired.retired_at = Set(Some(Utc::now().fixed_offset()));
            Ok(retired.update(&txn).await?)
        }
        .await;
        finish(txn, result).await
    }
}

fn canonical_uuid(raw: &str) -> Result<String, CredentialError> {
    Uuid::parse_str(raw)
        .map(|id| id.to_string())
        .map_err(|_| CredentialError::InvalidUuid)
}

async fn insert_binding(
    txn: &DatabaseTransaction,
    principal: &str,
    key: &CredentialKey,
    idempotent: bool,
) -> Result<Option<Binding>, CredentialError> {
    let conflict = if idempotent {
        "ON CONFLICT (namespace, credential_id) WHERE retired_at IS NULL DO NOTHING"
    } else {
        ""
    };
    let row = txn
        .query_one_raw(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            format!(
                "INSERT INTO trace_credential_binding (id, principal_id, namespace, credential_id) \
                 VALUES ($1, $2, $3, $4) {conflict} \
                 RETURNING id, principal_id, namespace, credential_id, created_at, retired_at"
            ),
            [
                Uuid::new_v4().to_string().into(),
                principal.to_owned().into(),
                key.namespace().into(),
                key.identifier().into(),
            ],
        ))
        .await
        .map_err(|error| match error.sql_err() {
            Some(SqlErr::UniqueConstraintViolation(_)) => CredentialError::Conflict,
            _ => CredentialError::Database(error),
        })?;
    row.map(|row| Binding::from_query_result(&row, ""))
        .transpose()
        .map_err(CredentialError::Database)
}

async fn finish(
    txn: DatabaseTransaction,
    result: Result<Binding, CredentialError>,
) -> Result<Binding, CredentialError> {
    match result {
        Ok(row) => {
            txn.commit().await?;
            Ok(row)
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sea_orm::PaginatorTrait;

    use super::*;
    use crate::{
        callisto::{access_token, ssh_keys, trace_principal},
        ceres::traces::auth::{
            TracesAction, TracesAuthDecision, TracesAuthRequest, TracesCaller, authorize_traces,
        },
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    fn tokens() -> Vec<PushTokenConfig> {
        ["first", "second", "third", "access-token:31"]
            .into_iter()
            .map(|name| PushTokenConfig {
                name: name.to_owned(),
                token: format!("synthetic-secret-{name}"),
                paths: Some(vec!["/project".to_owned()]),
            })
            .collect()
    }

    async fn migrated() -> (tempfile::TempDir, DatabaseConnection) {
        let temp = tempfile::tempdir().expect("temporary database owner");
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        (temp, db)
    }

    async fn sources(db: &DatabaseConnection) {
        access_token::ActiveModel {
            id: Set(31),
            username: Set("same-synthetic-account".to_owned()),
            token: Set("synthetic-access-secret".to_owned()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(db)
        .await
        .expect("access token source");
        ssh_keys::ActiveModel {
            id: Set(31),
            username: Set("same-synthetic-account".to_owned()),
            title: Set("synthetic key".to_owned()),
            ssh_key: Set("synthetic-public-key".to_owned()),
            finger: Set("synthetic-fingerprint".to_owned()),
            created_at: Set(Utc::now().naive_utc()),
        }
        .insert(db)
        .await
        .expect("SSH key source");
    }

    async fn row(db: &DatabaseConnection, id: &str) -> Binding {
        trace_credential_binding::Entity::find_by_id(id)
            .one(db)
            .await
            .expect("load binding")
            .expect("retained binding")
    }

    async fn backend_pid(txn: &DatabaseTransaction) -> i32 {
        txn.query_one_raw(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid",
        ))
        .await
        .expect("backend PID")
        .expect("PID row")
        .try_get("", "pid")
        .expect("PID")
    }

    async fn wait_for_blocked(db: &DatabaseConnection, blocker: i32, minimum: i64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let count: i64 = db
                .query_one_raw(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "WITH RECURSIVE waits(pid, blocker) AS ( \
                     SELECT pid, unnest(pg_blocking_pids(pid)) FROM pg_stat_activity \
                     WHERE datname = current_database() UNION \
                     SELECT waits.pid, unnest(pg_blocking_pids(waits.blocker)) FROM waits) \
                     SELECT count(DISTINCT pid)::bigint AS n FROM waits WHERE blocker = $1",
                    [blocker.into()],
                ))
                .await
                .expect("observe real lock waiters")
                .expect("count row")
                .try_get("", "n")
                .expect("waiter count");
            if count >= minimum {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "did not observe {minimum} lock waiters"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn hold_source(db: &DatabaseConnection) -> DatabaseTransaction {
        let txn = db.begin().await.expect("barrier transaction");
        assert!(
            txn.query_one_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT id FROM access_token WHERE id = 31 FOR UPDATE",
            ))
            .await
            .expect("source row lock")
            .is_some()
        );
        txn
    }

    async fn hold_binding(db: &DatabaseConnection, id: &str) -> DatabaseTransaction {
        let txn = db.begin().await.expect("barrier transaction");
        assert!(
            trace_credential_binding::Entity::find_by_id(id)
                .lock_exclusive()
                .one(&txn)
                .await
                .expect("binding row lock")
                .is_some()
        );
        txn
    }

    #[test]
    fn key_parsing_requires_names_and_canonical_record_ids() {
        let configured = tokens();
        assert!(CredentialKey::parse("static_push_token", "first", &configured).is_ok());
        assert!(matches!(
            CredentialKey::parse("static_push_token", "synthetic-secret-first", &configured),
            Err(CredentialError::SourceMissing)
        ));
        for key in [
            "user",
            "access-token:01",
            "access-token:+1",
            "access-token: 1",
            "access-token:1:2",
            "access-token:9223372036854775808",
            "ssh-key:",
            "username:31",
        ] {
            assert!(
                CredentialKey::parse("mono_account", key, &configured).is_err(),
                "{key}"
            );
        }
        assert!(CredentialKey::parse("unknown", "first", &configured).is_err());
        assert!(CredentialKey::parse("static_push_token", "", &configured).is_err());
        for key in [
            "access-token:31",
            "ssh-key:31",
            "access-token:-9223372036854775808",
        ] {
            assert_eq!(
                CredentialKey::parse("mono_account", key, &configured)
                    .expect("canonical ID")
                    .identifier(),
                key
            );
        }
    }

    #[test]
    fn uuid_forms_normalize_and_error_display_never_reflects_inputs() {
        let id = Uuid::new_v4();
        for form in [
            id.to_string(),
            id.to_string().to_uppercase(),
            id.simple().to_string(),
            id.braced().to_string(),
            id.urn().to_string(),
        ] {
            assert_eq!(canonical_uuid(&form).expect("UUID form"), id.to_string());
        }
        let canary = "synthetic-private-input";
        assert!(
            !canonical_uuid(canary)
                .expect_err("bad UUID")
                .to_string()
                .contains(canary)
        );
        let error = CredentialError::Database(DbErr::Custom(canary.to_owned()));
        assert!(!error.to_string().contains(canary));
        assert!(!error.to_string().contains("no changes"));
    }

    #[tokio::test]
    async fn bind_uses_explicit_principal_and_rolls_back_owner_conflicts() {
        let (_temp, db) = migrated().await;
        let configured = tokens();
        let storage = TraceCredentialStorage::new(&db, &configured);
        let principal = Uuid::new_v4();
        let first = storage
            .bind(
                &principal.braced().to_string(),
                "static_push_token",
                "first",
            )
            .await
            .expect("bind");
        assert_eq!(first.principal_id, principal.to_string());
        let again = storage
            .bind(&principal.to_string(), "static_push_token", "first")
            .await
            .expect("repeat bind");
        assert_eq!(first, again);
        let other = Uuid::new_v4().to_string();
        assert!(matches!(
            storage.bind(&other, "static_push_token", "first").await,
            Err(CredentialError::Conflict)
        ));
        assert!(
            trace_principal::Entity::find_by_id(&other)
                .one(&db)
                .await
                .expect("principal lookup")
                .is_none()
        );
        assert_eq!(row(&db, &first.id).await, first);
        assert_eq!(
            trace_principal::Entity::find()
                .count(&db)
                .await
                .expect("principal count"),
            1
        );
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn mono_sources_and_namespace_keys_do_not_infer_account_ownership() {
        let (_temp, db) = migrated().await;
        sources(&db).await;
        let configured = tokens();
        let storage = TraceCredentialStorage::new(&db, &configured);
        let mut rows = Vec::new();
        for (namespace, key) in [
            ("mono_account", "access-token:31"),
            ("mono_account", "ssh-key:31"),
            ("static_push_token", "access-token:31"),
        ] {
            let owner = Uuid::new_v4().to_string();
            let binding = storage
                .bind(&owner, namespace, key)
                .await
                .expect("bind real source");
            assert_eq!(binding.principal_id, owner);
            assert_eq!(binding.namespace, namespace);
            assert_eq!(binding.credential_id, key);
            rows.push(binding);
        }
        assert_ne!(rows[0].principal_id, rows[1].principal_id);
        assert_ne!(rows[0].principal_id, rows[2].principal_id);
        let missing = Uuid::new_v4().to_string();
        for key in ["access-token:99", "ssh-key:99"] {
            assert!(matches!(
                storage.bind(&missing, "mono_account", key).await,
                Err(CredentialError::SourceMissing)
            ));
        }
        assert!(
            trace_principal::Entity::find_by_id(missing)
                .one(&db)
                .await
                .expect("principal lookup")
                .is_none()
        );
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn rotate_preserves_principal_and_retire_preserves_history() {
        let (_temp, db) = migrated().await;
        let configured = tokens();
        let storage = TraceCredentialStorage::new(&db, &configured);
        let owner = Uuid::new_v4().to_string();
        let old = storage
            .bind(&owner, "static_push_token", "first")
            .await
            .expect("old binding");
        let replacement = storage
            .rotate(&old.id, "static_push_token", "second")
            .await
            .expect("rotation");
        assert_eq!(replacement.principal_id, owner);
        assert_ne!(replacement.id, old.id);
        assert_eq!(replacement.credential_id, "second");
        assert!(replacement.retired_at.is_none());
        let retired_old = row(&db, &old.id).await;
        assert_eq!(retired_old.credential_id, old.credential_id);
        assert_eq!(retired_old.created_at, old.created_at);
        assert!(retired_old.retired_at.is_some());
        assert!(matches!(
            storage.rotate(&old.id, "static_push_token", "third").await,
            Err(CredentialError::BindingRetired)
        ));
        let retired = storage.retire(&replacement.id).await.expect("retire");
        assert!(retired.retired_at.is_some());
        assert_eq!(
            storage
                .retire(&replacement.id)
                .await
                .expect("repeat retire"),
            retired
        );
        assert_eq!(row(&db, &old.id).await, retired_old);
        let next_owner = Uuid::new_v4().to_string();
        let rebound = storage
            .bind(&next_owner, "static_push_token", "second")
            .await
            .expect("explicit rebind after retirement");
        assert_eq!(rebound.principal_id, next_owner);
        assert_ne!(rebound.id, retired.id);
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn rejected_rotation_and_update_failure_roll_back_every_write() {
        let (_temp, db) = migrated().await;
        let configured = tokens();
        let storage = TraceCredentialStorage::new(&db, &configured);
        let old = storage
            .bind(&Uuid::new_v4().to_string(), "static_push_token", "first")
            .await
            .expect("old");
        let other = storage
            .bind(&Uuid::new_v4().to_string(), "static_push_token", "second")
            .await
            .expect("other");
        assert!(matches!(
            storage.rotate(&old.id, "static_push_token", "first").await,
            Err(CredentialError::SameCredential)
        ));
        assert!(matches!(
            storage.rotate(&old.id, "static_push_token", "second").await,
            Err(CredentialError::Conflict)
        ));
        assert!(matches!(
            storage
                .rotate(&old.id, "mono_account", "access-token:99")
                .await,
            Err(CredentialError::SourceMissing)
        ));
        assert!(matches!(
            storage
                .rotate(&Uuid::new_v4().to_string(), "static_push_token", "third")
                .await,
            Err(CredentialError::BindingMissing)
        ));
        assert!(matches!(
            storage.retire(&Uuid::new_v4().to_string()).await,
            Err(CredentialError::BindingMissing)
        ));
        db.execute_unprepared("CREATE FUNCTION reject_fixture_retire() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic-database-secret'; END $$; CREATE TRIGGER reject_fixture_retire BEFORE UPDATE ON trace_credential_binding FOR EACH ROW EXECUTE FUNCTION reject_fixture_retire()")
            .await.expect("transaction failure fixture");
        let error = storage
            .rotate(&old.id, "static_push_token", "third")
            .await
            .expect_err("retirement update fails");
        assert!(matches!(error, CredentialError::Database(_)));
        assert!(!error.to_string().contains("synthetic-database-secret"));
        assert_eq!(row(&db, &old.id).await, old);
        assert_eq!(row(&db, &other.id).await, other);
        assert_eq!(
            trace_credential_binding::Entity::find()
                .count(&db)
                .await
                .expect("binding count"),
            2
        );
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn retire_revokes_subsequent_real_traces_authorization() {
        let (_temp, db) = migrated().await;
        let configured = tokens();
        let storage = TraceCredentialStorage::new(&db, &configured);
        let owner = Uuid::new_v4().to_string();
        let binding = storage
            .bind(&owner, "static_push_token", "first")
            .await
            .expect("bind");
        let request = TracesAuthRequest {
            caller: TracesCaller::StaticPushToken {
                credential_id: "first",
                token: &configured[0],
            },
            action: TracesAction::Update {
                owner_principal_id: &owner,
            },
            canonical_target_path: "/project/example",
            commit_author: None,
        };
        assert_eq!(
            authorize_traces(&db, &request)
                .await
                .expect("active authorization"),
            TracesAuthDecision::Allow {
                principal_id: owner.clone()
            }
        );
        storage.retire(&binding.id).await.expect("retire");
        assert_eq!(
            authorize_traces(&db, &request)
                .await
                .expect("retired authorization"),
            TracesAuthDecision::NotFound
        );
        assert!(row(&db, &binding.id).await.retired_at.is_some());
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn concurrent_binds_for_a_new_principal_return_the_same_binding() {
        let (_temp, db) = migrated().await;
        sources(&db).await;
        let storage = TraceCredentialStorage::new(&db, &[]);
        let owner = Uuid::new_v4().to_string();
        let barrier = hold_source(&db).await;
        let pid = backend_pid(&barrier).await;
        let (left, right, ()) = tokio::join!(
            storage.bind(&owner, "mono_account", "access-token:31"),
            storage.bind(&owner, "mono_account", "access-token:31"),
            async {
                wait_for_blocked(&db, pid, 2).await;
                barrier.rollback().await.expect("release source");
            }
        );
        assert_eq!(
            left.expect("first bind"),
            right.expect("idempotent concurrent bind")
        );
        assert_eq!(
            trace_principal::Entity::find()
                .count(&db)
                .await
                .expect("principal count"),
            1
        );
        assert_eq!(
            trace_credential_binding::Entity::find()
                .count(&db)
                .await
                .expect("binding count"),
            1
        );
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn concurrent_different_principals_leave_no_losing_principal() {
        let (_temp, db) = migrated().await;
        sources(&db).await;
        let storage = TraceCredentialStorage::new(&db, &[]);
        let first = Uuid::new_v4().to_string();
        let second = Uuid::new_v4().to_string();
        let barrier = hold_source(&db).await;
        let pid = backend_pid(&barrier).await;
        let (left, right, ()) = tokio::join!(
            storage.bind(&first, "mono_account", "access-token:31"),
            storage.bind(&second, "mono_account", "access-token:31"),
            async {
                wait_for_blocked(&db, pid, 2).await;
                barrier.rollback().await.expect("release source");
            }
        );
        let (winner, loser) = match (left, right) {
            (Ok(row), Err(CredentialError::Conflict)) => (row, second),
            (Err(CredentialError::Conflict), Ok(row)) => (row, first),
            other => panic!("expected one winner and one conflict: {other:?}"),
        };
        assert_ne!(winner.principal_id, loser);
        assert!(
            trace_principal::Entity::find_by_id(loser)
                .one(&db)
                .await
                .expect("loser lookup")
                .is_none()
        );
        assert_eq!(
            trace_credential_binding::Entity::find()
                .count(&db)
                .await
                .expect("binding count"),
            1
        );
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn concurrent_rotations_of_one_binding_have_one_winner() {
        let (_temp, db) = migrated().await;
        let configured = tokens();
        let storage = TraceCredentialStorage::new(&db, &configured);
        let old = storage
            .bind(&Uuid::new_v4().to_string(), "static_push_token", "first")
            .await
            .expect("old");
        let barrier = hold_binding(&db, &old.id).await;
        let pid = backend_pid(&barrier).await;
        let (left, right, ()) = tokio::join!(
            storage.rotate(&old.id, "static_push_token", "second"),
            storage.rotate(&old.id, "static_push_token", "third"),
            async {
                wait_for_blocked(&db, pid, 2).await;
                barrier.rollback().await.expect("release binding");
            }
        );
        let winner = match (left, right) {
            (Ok(row), Err(CredentialError::BindingRetired))
            | (Err(CredentialError::BindingRetired), Ok(row)) => row,
            other => panic!("expected one rotation winner: {other:?}"),
        };
        assert_eq!(winner.principal_id, old.principal_id);
        assert!(row(&db, &old.id).await.retired_at.is_some());
        assert_eq!(
            trace_credential_binding::Entity::find()
                .count(&db)
                .await
                .expect("binding count"),
            2
        );
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn opposite_rotations_reject_without_deadlock_or_ownership_change() {
        let (_temp, db) = migrated().await;
        let configured = tokens();
        let storage = TraceCredentialStorage::new(&db, &configured);
        let first = storage
            .bind(&Uuid::new_v4().to_string(), "static_push_token", "first")
            .await
            .expect("first");
        let second = storage
            .bind(&Uuid::new_v4().to_string(), "static_push_token", "second")
            .await
            .expect("second");
        let (left, right) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                storage.rotate(&first.id, "static_push_token", "second"),
                storage.rotate(&second.id, "static_push_token", "first")
            )
        })
        .await
        .expect("rotations finish without deadlock");
        assert!(matches!(left, Err(CredentialError::Conflict)));
        assert!(matches!(right, Err(CredentialError::Conflict)));
        assert_eq!(row(&db, &first.id).await, first);
        assert_eq!(row(&db, &second.id).await, second);
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn concurrent_retire_and_rotate_preserve_atomic_lifecycle() {
        let (_temp, db) = migrated().await;
        let configured = tokens();
        let storage = TraceCredentialStorage::new(&db, &configured);
        let old = storage
            .bind(&Uuid::new_v4().to_string(), "static_push_token", "first")
            .await
            .expect("old");
        let barrier = hold_binding(&db, &old.id).await;
        let pid = backend_pid(&barrier).await;
        let (retired, rotated, ()) = tokio::join!(
            storage.retire(&old.id),
            storage.rotate(&old.id, "static_push_token", "second"),
            async {
                wait_for_blocked(&db, pid, 2).await;
                barrier.rollback().await.expect("release binding");
            }
        );
        let retired = retired.expect("retirement succeeds");
        assert!(retired.retired_at.is_some());
        let expected_count = match rotated {
            Ok(replacement) => {
                assert_eq!(replacement.principal_id, old.principal_id);
                2
            }
            Err(CredentialError::BindingRetired) => 1,
            other => panic!("unexpected rotation outcome: {other:?}"),
        };
        assert_eq!(
            storage
                .retire(&old.id)
                .await
                .expect("idempotent retirement"),
            retired
        );
        assert_eq!(
            trace_credential_binding::Entity::find()
                .count(&db)
                .await
                .expect("binding count"),
            expected_count
        );
        db.close().await.expect("close database");
    }

    #[tokio::test]
    async fn source_share_lock_blocks_deletion_until_transaction_ends() {
        let (_temp, db) = migrated().await;
        sources(&db).await;
        for (key, sql, credential) in [
            (
                CredentialKey::AccessToken(31),
                "DELETE FROM access_token WHERE id = 31",
                "access-token:31",
            ),
            (
                CredentialKey::SshKey(31),
                "DELETE FROM ssh_keys WHERE id = 31",
                "ssh-key:31",
            ),
        ] {
            let txn = db.begin().await.expect("binding transaction");
            key.lock_source(&txn).await.expect("lock real source");
            let pid = backend_pid(&txn).await;
            let (deleted, ()) = tokio::join!(
                db.execute_raw(Statement::from_string(DatabaseBackend::Postgres, sql)),
                async {
                    wait_for_blocked(&db, pid, 1).await;
                    txn.commit().await.expect("release share lock");
                }
            );
            assert_eq!(deleted.expect("delete after release").rows_affected(), 1);
            let storage = TraceCredentialStorage::new(&db, &[]);
            assert!(matches!(
                storage
                    .bind(&Uuid::new_v4().to_string(), "mono_account", credential)
                    .await,
                Err(CredentialError::SourceMissing)
            ));
        }
        assert_eq!(
            trace_principal::Entity::find()
                .count(&db)
                .await
                .expect("principal count"),
            0
        );
        db.close().await.expect("close database");
    }
}
