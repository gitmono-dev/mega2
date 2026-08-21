use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use libvault::storage::{Backend, BackendEntry};

use crate::{
    callisto::vault,
    common::errors::{MegaError, RvError},
    jupiter::storage::vault_storage::VaultStorage,
};

pub struct JupiterBackend {
    storage: Arc<dyn VaultBackendStorage>,
}

#[async_trait]
pub trait VaultBackendStorage: Send + Sync {
    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, MegaError>;
    async fn load(&self, key: &str) -> Result<Option<vault::Model>, MegaError>;
    async fn save(&self, key: &str, value: Vec<u8>) -> Result<(), MegaError>;
    async fn delete(&self, key: &str) -> Result<(), MegaError>;
}

#[async_trait]
impl VaultBackendStorage for VaultStorage {
    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>, MegaError> {
        VaultStorage::list_keys(self, prefix).await
    }

    async fn load(&self, key: &str) -> Result<Option<vault::Model>, MegaError> {
        VaultStorage::load(self, key).await
    }

    async fn save(&self, key: &str, value: Vec<u8>) -> Result<(), MegaError> {
        VaultStorage::save(self, key, value).await
    }

    async fn delete(&self, key: &str) -> Result<(), MegaError> {
        VaultStorage::delete(self, key).await
    }
}

impl JupiterBackend {
    pub fn new(storage: impl VaultBackendStorage + 'static) -> Self {
        JupiterBackend {
            storage: Arc::new(storage),
        }
    }
}

#[async_trait]
impl Backend for JupiterBackend {
    /// List the immediate children of `prefix` (FIX-04).
    ///
    /// The barrier views walk a backend one level at a time: `list` is expected
    /// to return names *relative* to the prefix, with a subdirectory marked by a
    /// trailing `/`, which is what `physical::file::FileBackend` does and what
    /// `BarrierView::get_keys` is written against. The underlying
    /// `VaultStorage::list_keys` is a `LIKE 'prefix%'` scan, so it returns whole
    /// keys and returns them recursively; handing those back untouched made
    /// every walked key double up its prefix on the following `get`, so
    /// `get_keys` resolved nothing. Projecting to children here is what makes
    /// this backend interchangeable with the others.
    ///
    /// A prefix naming a directory without its trailing `/` is accepted too —
    /// `TokenStore::revoke_tree_salted` passes `parent/<id>` when walking a
    /// token's children — so the separator is stripped from the remainder
    /// before the split. Without that the child name comes back as a bare `/`
    /// and the walk stops one level in.
    ///
    /// The projection also contains the `LIKE` pattern: `_` and `%` are
    /// wildcards to Postgres but ordinary characters in a vault key, so the scan
    /// can return keys that merely resemble the prefix. `strip_prefix` is an
    /// exact match and drops them.
    async fn list(&self, prefix: &str) -> Result<Vec<String>, RvError> {
        let keys = self
            .storage
            .list_keys(prefix)
            .await
            .map_err(|_| RvError::ErrPhysicalBackendKeyInvalid)?;

        let mut children = BTreeSet::new();
        for key in keys {
            let Some(rest) = key.strip_prefix(prefix) else {
                continue;
            };
            let rest = rest.strip_prefix('/').unwrap_or(rest);
            if rest.is_empty() {
                continue;
            }
            match rest.find('/') {
                Some(idx) => children.insert(format!("{}/", &rest[..idx])),
                None => children.insert(rest.to_string()),
            };
        }

        Ok(children.into_iter().collect())
    }

    async fn get(&self, key: &str) -> Result<Option<BackendEntry>, RvError> {
        self.storage
            .load(key)
            .await
            .map(|opt| {
                opt.and_then(|model| {
                    BackendEntry {
                        key: model.key,
                        value: model.value,
                    }
                    .into()
                })
            })
            .map_err(|_| RvError::ErrPhysicalBackendKeyInvalid)
    }

    async fn put(&self, entry: &BackendEntry) -> Result<(), RvError> {
        self.storage
            .save(&entry.key, entry.value.clone())
            .await
            .map(|_| ())
            .map_err(|_| RvError::ErrPhysicalBackendKeyInvalid)
    }

    async fn delete(&self, key: &str) -> Result<(), RvError> {
        self.storage
            .delete(key)
            .await
            .map(|_| ())
            .map_err(|_| RvError::ErrPhysicalBackendKeyInvalid)
    }
}
