use std::{fmt, ops::Deref};

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, IntoActiveModel, ModelTrait,
    QueryFilter, QuerySelect,
};
use sha2::{Digest, Sha256};
use tracing::instrument::WithSubscriber;
use uuid::Uuid;

use crate::{
    callisto::{access_token, ssh_keys},
    common::{errors::MegaError, utils::generate_id},
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

#[derive(Debug, Clone)]
pub struct UserStorage {
    pub base: BaseStorage,
}

/// A credential authenticated against exactly one Mono access-token row.
/// Its binding key identifies a credential, not an account or trace principal.
pub struct AuthenticatedAccessToken {
    id: i64,
    username: String,
    credential_id: String,
    fingerprint: [u8; 32],
    created_at: chrono::NaiveDateTime,
}

impl fmt::Debug for AuthenticatedAccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticatedAccessToken")
            .finish_non_exhaustive()
    }
}

impl AuthenticatedAccessToken {
    pub(crate) fn id(&self) -> i64 {
        self.id
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn credential_id(&self) -> &str {
        &self.credential_id
    }

    pub(crate) fn matches_record(&self, row: &access_token::Model) -> bool {
        self.id == row.id
            && self.username == row.username
            && self.created_at == row.created_at
            && self.fingerprint == <[u8; 32]>::from(Sha256::digest(row.token.as_bytes()))
    }

    pub(crate) async fn is_current<C: ConnectionTrait>(
        &self,
        conn: &C,
        lock: bool,
    ) -> Result<bool, MegaError> {
        let query = access_token::Entity::find_by_id(self.id);
        let query = if lock { query.lock_shared() } else { query };
        Ok(query
            .one(conn)
            .await?
            .is_some_and(|row| self.matches_record(&row)))
    }
}

impl Deref for UserStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl UserStorage {
    pub(crate) async fn authenticate_traces_access_token(
        &self,
        token: &str,
    ) -> Result<Option<AuthenticatedAccessToken>, MegaError> {
        if token.is_empty() {
            return Ok(None);
        }
        // SeaORM debug-print expands bind values; this query contains a secret.
        let rows = access_token::Entity::find()
            .filter(access_token::Column::Token.eq(token))
            .limit(2)
            .all(self.get_connection())
            .with_subscriber(tracing::subscriber::NoSubscriber::default())
            .await?;
        let [row] = rows.as_slice() else {
            return Ok(None);
        };
        if row.username.is_empty() {
            return Ok(None);
        }
        Ok(Some(AuthenticatedAccessToken {
            id: row.id,
            username: row.username.clone(),
            credential_id: format!("access-token:{}", row.id),
            fingerprint: Sha256::digest(row.token.as_bytes()).into(),
            created_at: row.created_at,
        }))
    }

    pub fn mock() -> Self {
        let mock = BaseStorage::mock();
        Self { base: mock }
    }

    pub async fn save_ssh_key(
        &self,
        username: String,
        title: &str,
        ssh_key: &str,
        finger: &str,
    ) -> Result<(), MegaError> {
        let model = ssh_keys::Model {
            id: generate_id(),
            username,
            title: title.to_owned(),
            ssh_key: ssh_key.to_owned(),
            finger: finger.to_owned(),
            created_at: chrono::Utc::now().naive_utc(),
        };
        let a_model = model.into_active_model();
        a_model.insert(self.get_connection()).await.unwrap();
        Ok(())
    }

    pub async fn list_user_ssh(&self, username: String) -> Result<Vec<ssh_keys::Model>, MegaError> {
        let res: Vec<ssh_keys::Model> = ssh_keys::Entity::find()
            .filter(ssh_keys::Column::Username.eq(username))
            .all(self.get_connection())
            .await?;
        Ok(res)
    }

    pub async fn delete_ssh_key(&self, username: String, id: i64) -> Result<(), MegaError> {
        let res = ssh_keys::Entity::find()
            .filter(ssh_keys::Column::Id.eq(id))
            .filter(ssh_keys::Column::Username.eq(username))
            .one(self.get_connection())
            .await?;
        if let Some(model) = res {
            model.delete(self.get_connection()).await?;
        }
        Ok(())
    }

    pub async fn search_ssh_key_finger(
        &self,
        finger_print: &str,
    ) -> Result<Vec<ssh_keys::Model>, MegaError> {
        let res = ssh_keys::Entity::find()
            .filter(ssh_keys::Column::Finger.eq(finger_print))
            .all(self.get_connection())
            .await?;
        Ok(res)
    }

    pub async fn generate_token(&self, username: String) -> Result<String, MegaError> {
        let token_str = Uuid::new_v4().to_string();
        let model = access_token::Model {
            id: generate_id(),
            username,
            token: token_str.clone(),
            created_at: chrono::Utc::now().naive_utc(),
        };
        let a_model = model.into_active_model();
        a_model.insert(self.get_connection()).await.unwrap();
        Ok(token_str.to_owned())
    }

    pub async fn delete_token(&self, username: String, id: i64) -> Result<(), MegaError> {
        let res = access_token::Entity::find()
            .filter(access_token::Column::Id.eq(id))
            .filter(access_token::Column::Username.eq(username))
            .one(self.get_connection())
            .await?;
        if let Some(model) = res {
            model.delete(self.get_connection()).await?;
        }
        Ok(())
    }

    pub async fn list_token(
        &self,
        username: String,
    ) -> Result<Vec<access_token::Model>, MegaError> {
        let res = access_token::Entity::find()
            .filter(access_token::Column::Username.eq(username))
            .all(self.get_connection())
            .await?;
        Ok(res)
    }

    pub async fn check_token(&self, username: &str, token: &str) -> Result<bool, MegaError> {
        let res = access_token::Entity::find()
            .filter(access_token::Column::Username.eq(username))
            .filter(access_token::Column::Token.eq(token))
            .one(self.get_connection())
            .await?;
        match res {
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }

    pub async fn find_user_by_token(&self, token: &str) -> Result<Option<String>, MegaError> {
        let res = access_token::Entity::find()
            .filter(access_token::Column::Token.eq(token))
            .one(self.get_connection())
            .await?;
        match res {
            Some(token_model) => Ok(Some(token_model.username)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod test {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    use sea_orm::{ActiveModelTrait, EntityTrait, Set};
    use uuid::Uuid;

    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    async fn fixture() -> (tempfile::TempDir, UserStorage) {
        let temp = tempfile::TempDir::new().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        (
            temp,
            UserStorage {
                base: BaseStorage::new(Arc::new(db)),
            },
        )
    }

    async fn insert(storage: &UserStorage, id: i64, username: &str, token: &str) {
        access_token::ActiveModel {
            id: Set(id),
            username: Set(username.into()),
            token: Set(token.into()),
            created_at: Set(chrono::Utc::now().naive_utc()),
        }
        .insert(storage.get_connection())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn traces_token_witness_uses_real_row_and_redacts_debug() {
        let (_temp, storage) = fixture().await;
        let secret = "synthetic-traces-token-value";
        insert(&storage, 101, "publisher", secret).await;
        let witness = storage
            .authenticate_traces_access_token(secret)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(witness.id(), 101);
        assert_eq!(witness.username(), "publisher");
        assert_eq!(witness.credential_id(), "access-token:101");
        assert!(
            witness
                .is_current(storage.get_connection(), false)
                .await
                .unwrap()
        );
        assert_eq!(format!("{witness:?}"), "AuthenticatedAccessToken { .. }");
        assert_eq!(
            storage.find_user_by_token(secret).await.unwrap().as_deref(),
            Some("publisher")
        );
        assert!(storage.check_token("publisher", secret).await.unwrap());
        for invalid in ["", "unknown", "publisher", "access-token:101"] {
            assert!(
                storage
                    .authenticate_traces_access_token(invalid)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let row = access_token::Entity::find_by_id(101)
            .one(storage.get_connection())
            .await
            .unwrap()
            .unwrap();
        let mut changed = row.clone();
        changed.token = "synthetic-replacement".into();
        assert!(!witness.matches_record(&changed));
        changed = row.clone();
        changed.username = "other".into();
        assert!(!witness.matches_record(&changed));
        changed = row;
        changed.created_at += chrono::Duration::seconds(1);
        assert!(!witness.matches_record(&changed));
        storage.delete_token("publisher".into(), 101).await.unwrap();
        assert!(
            !witness
                .is_current(storage.get_connection(), false)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn traces_token_witness_rejects_ambiguous_or_empty_owner() {
        let (_temp, storage) = fixture().await;
        insert(&storage, 201, "one", "synthetic-duplicate").await;
        insert(&storage, 202, "two", "synthetic-duplicate").await;
        assert!(
            storage
                .authenticate_traces_access_token("synthetic-duplicate")
                .await
                .unwrap()
                .is_none()
        );
        insert(&storage, 203, "", "synthetic-empty-owner").await;
        assert!(
            storage
                .authenticate_traces_access_token("synthetic-empty-owner")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[derive(Clone)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn traces_token_authentication_does_not_log_secret_bind_values() {
        let (_temp, storage) = fixture().await;
        let secret = "synthetic-sensitive-query-token";
        insert(&storage, 301, "publisher", secret).await;
        let capture = CapturedLogs(Arc::new(Mutex::new(Vec::new())));
        let writer = capture.clone();
        let subscriber = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish(),
        );
        storage
            .find_user_by_token(secret)
            .with_subscriber(subscriber.clone())
            .await
            .unwrap();
        let control = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(
            control.contains(secret),
            "the control must observe SeaORM debug-print bind expansion"
        );
        capture.0.lock().unwrap().clear();
        let witness = async {
            tracing::debug!("credential_query_before");
            let witness = storage
                .authenticate_traces_access_token(secret)
                .await
                .unwrap()
                .unwrap();
            tracing::debug!("credential_query_after");
            witness
        }
        .with_subscriber(subscriber)
        .await;
        assert_eq!(witness.id(), 301);
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains("credential_query_before") && logs.contains("credential_query_after")
        );
        assert!(
            !logs.contains(secret),
            "the sensitive query must not expose its bind values"
        );
    }

    #[test]
    fn token_format() {
        let uuid = Uuid::new_v4().to_string();
        println!("{uuid:?}");
    }
}
