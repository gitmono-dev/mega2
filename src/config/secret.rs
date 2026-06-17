use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as DeError};
use tokio::sync::RwLock;

use crate::{
    common::errors::MegaError,
    contract::vault::integration::vault_core::{VaultCore, VaultCoreInterface},
};

const SECRET_REF_PREFIX: &str = "vault://secret/";
const REDACTED_SECRET: &str = "***";
const REDACTED_SECRET_REF: &str = "vault://secret/***#***";

#[derive(Clone, Eq, PartialEq, Hash)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretString")
            .field(&REDACTED_SECRET)
            .finish()
    }
}

impl Serialize for SecretString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(REDACTED_SECRET)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Eq, PartialEq, Hash)]
pub struct SecretRef {
    uri: String,
    secret_name: String,
    field: String,
}

impl SecretRef {
    pub fn parse(input: impl AsRef<str>) -> Result<Self, MegaError> {
        let input = input.as_ref();
        let path_and_field = input.strip_prefix(SECRET_REF_PREFIX).ok_or_else(|| {
            MegaError::Other("secret ref must use vault://secret/<name>#<field> format".to_string())
        })?;
        let (secret_name, field) = path_and_field.split_once('#').ok_or_else(|| {
            MegaError::Other(
                "secret ref must include a #field suffix, for example vault://secret/config/prod/mail/password#value".to_string(),
            )
        })?;

        if field.is_empty() || field.contains('#') || field.contains('/') {
            return Err(MegaError::Other(
                "secret ref field must be a non-empty single field name".to_string(),
            ));
        }
        validate_secret_name(secret_name)?;

        Ok(Self {
            uri: format!("{SECRET_REF_PREFIX}{secret_name}#{field}"),
            secret_name: secret_name.to_string(),
            field: field.to_string(),
        })
    }

    pub fn from_parts(
        secret_name: impl AsRef<str>,
        field: impl AsRef<str>,
    ) -> Result<Self, MegaError> {
        Self::parse(format!(
            "{SECRET_REF_PREFIX}{}#{}",
            secret_name.as_ref(),
            field.as_ref()
        ))
    }

    pub fn as_uri(&self) -> &str {
        &self.uri
    }

    pub fn redacted(&self) -> &'static str {
        REDACTED_SECRET_REF
    }

    pub fn secret_name(&self) -> &str {
        &self.secret_name
    }

    pub fn field(&self) -> &str {
        &self.field
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SecretRef").field(&self.redacted()).finish()
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.redacted())
    }
}

impl Serialize for SecretRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.uri)
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(D::Error::custom)
    }
}

#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, secret_ref: &SecretRef) -> Result<String, MegaError>;
    async fn evict(&self, secret_ref: &SecretRef);
    async fn evict_all(&self);
}

#[derive(Clone)]
pub struct VaultSecretResolver {
    vault: VaultCore,
    ttl: Duration,
    cache: Arc<RwLock<HashMap<String, CachedSecret>>>,
}

#[derive(Clone)]
struct CachedSecret {
    value: String,
    expires_at: Instant,
}

impl VaultSecretResolver {
    pub fn new(vault: VaultCore, ttl: Duration) -> Self {
        Self {
            vault,
            ttl,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl SecretResolver for VaultSecretResolver {
    async fn resolve(&self, secret_ref: &SecretRef) -> Result<String, MegaError> {
        if !self.ttl.is_zero() {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.get(secret_ref.as_uri())
                && cached.expires_at > Instant::now()
            {
                return Ok(cached.value.clone());
            }
        }

        let secret = self
            .vault
            .read_secret(secret_ref.secret_name())
            .await?
            .ok_or_else(|| {
                MegaError::Other(format!("secret not found for {}", secret_ref.redacted()))
            })?;
        let value = secret.get(secret_ref.field()).ok_or_else(|| {
            MegaError::Other(format!(
                "secret field not found for {}",
                secret_ref.redacted()
            ))
        })?;
        let value = value.as_str().ok_or_else(|| {
            MegaError::Other(format!(
                "secret field is not a string for {}",
                secret_ref.redacted()
            ))
        })?;
        let value = value.to_string();

        if !self.ttl.is_zero() {
            let mut cache = self.cache.write().await;
            cache.insert(
                secret_ref.as_uri().to_string(),
                CachedSecret {
                    value: value.clone(),
                    expires_at: Instant::now() + self.ttl,
                },
            );
        }

        Ok(value)
    }

    async fn evict(&self, secret_ref: &SecretRef) {
        self.cache.write().await.remove(secret_ref.as_uri());
    }

    async fn evict_all(&self) {
        self.cache.write().await.clear();
    }
}

fn validate_secret_name(name: &str) -> Result<(), MegaError> {
    if name.is_empty()
        || name.starts_with('/')
        || name == "secret"
        || name.starts_with("secret/")
        || name
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(MegaError::Other(
            "invalid vault secret name in secret ref".to_string(),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Map, Value};

    use super::*;
    use crate::{
        contract::vault::integration::vault_core::VaultCore,
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    #[test]
    fn secret_ref_parses_canonical_uri() {
        let secret_ref =
            SecretRef::parse("vault://secret/config/prod/mail/password#value").unwrap();

        assert_eq!(secret_ref.secret_name(), "config/prod/mail/password");
        assert_eq!(secret_ref.field(), "value");
        assert_eq!(
            secret_ref.as_uri(),
            "vault://secret/config/prod/mail/password#value"
        );
    }

    #[test]
    fn secret_ref_debug_and_display_are_redacted() {
        let secret_ref =
            SecretRef::parse("vault://secret/config/prod/mail/password#value").unwrap();

        assert_eq!(
            secret_ref.as_uri(),
            "vault://secret/config/prod/mail/password#value"
        );
        assert_eq!(secret_ref.redacted(), "vault://secret/***#***");
        assert_eq!(secret_ref.to_string(), "vault://secret/***#***");
        assert_eq!(
            format!("{secret_ref:?}"),
            "SecretRef(\"vault://secret/***#***\")"
        );
    }

    #[test]
    fn secret_ref_rejects_secret_prefixed_name() {
        let err = SecretRef::parse("vault://secret/secret/config/mail/password#value")
            .expect_err("secret/secret paths should be rejected");

        assert!(err.to_string().contains("invalid vault secret name"));
    }

    #[test]
    fn secret_string_debug_and_serialize_are_redacted() {
        let secret = SecretString::new("plain-text-password");

        assert_eq!(secret.expose_secret(), "plain-text-password");
        assert_eq!(format!("{secret:?}"), "SecretString(\"***\")");
        assert_eq!(serde_json::to_string(&secret).unwrap(), "\"***\"");
    }

    #[test]
    fn secret_string_deserializes_plaintext() {
        let secret: SecretString = serde_json::from_str("\"plain-text-password\"").unwrap();

        assert_eq!(secret.expose_secret(), "plain-text-password");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn vault_secret_resolver_reads_and_evicts_cached_value() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let connection = Arc::new(test_db_connection(temp_dir.path()).await);
        apply_migrations(&connection, true).await.unwrap();
        let vault =
            VaultCore::from_database_connection(connection, temp_dir.path().join("core_key.json"))
                .await
                .unwrap();

        let mut data = Map::new();
        data.insert("value".to_string(), Value::String("first".to_string()));
        vault
            .write_secret("config/test/mail/password", Some(data))
            .await
            .unwrap();

        let secret_ref =
            SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap();
        let resolver = VaultSecretResolver::new(vault.clone(), Duration::from_secs(60));
        assert_eq!(resolver.resolve(&secret_ref).await.unwrap(), "first");

        let mut data = Map::new();
        data.insert("value".to_string(), Value::String("second".to_string()));
        vault
            .write_secret("config/test/mail/password", Some(data))
            .await
            .unwrap();

        assert_eq!(resolver.resolve(&secret_ref).await.unwrap(), "first");
        resolver.evict(&secret_ref).await;
        assert_eq!(resolver.resolve(&secret_ref).await.unwrap(), "second");
        resolver.evict_all().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn vault_secret_resolver_reports_missing_secret_without_leaking_ref() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let connection = Arc::new(test_db_connection(temp_dir.path()).await);
        apply_migrations(&connection, true).await.unwrap();
        let vault =
            VaultCore::from_database_connection(connection, temp_dir.path().join("core_key.json"))
                .await
                .unwrap();

        let secret_ref =
            SecretRef::parse("vault://secret/config/test/mail/missing-password#value").unwrap();
        let resolver = VaultSecretResolver::new(vault, Duration::ZERO);

        let err = resolver
            .resolve(&secret_ref)
            .await
            .expect_err("missing vault secret should fail");
        let message = err.to_string();

        assert!(message.contains("secret not found"));
        assert!(message.contains("vault://secret/***#***"));
        assert!(!message.contains("config/test/mail/missing-password"));
        assert!(!message.contains("#value"));
    }
}
