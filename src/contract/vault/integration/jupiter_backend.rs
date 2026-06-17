use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    callisto::vault,
    common::errors::MegaError,
    jupiter::storage::vault_storage::VaultStorage,
    vault::{
        errors::RvError,
        storage::{Backend, BackendEntry},
    },
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
    async fn list(&self, prefix: &str) -> Result<Vec<String>, RvError> {
        self.storage
            .list_keys(prefix)
            .await
            .map_err(|_| RvError::ErrPhysicalBackendKeyInvalid)
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
