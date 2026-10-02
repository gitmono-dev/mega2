use std::{
    fs::{create_dir_all, exists},
    sync::Arc,
};

use object_store::{
    aws::{AmazonS3, AmazonS3Builder},
    gcp::GoogleCloudStorageBuilder,
    local::LocalFileSystem,
};

use super::{
    adapter::{BackendStore, ObjectStoreAdapter, UploadStrategy},
    error::{IoOrbitError, OrbitResult},
};
pub use crate::orbit_api::factory::{
    GcsConfig, LocalConfig, MegaObjectStorageWithLog, MegaObjectStorageWrapper,
    ObjectStorageBackend, ObjectStorageConfig, S3Config,
};

pub struct ObjectStorageFactory;

impl ObjectStorageFactory {
    /// Builds object storage from [`ObjectStorageConfig::storage_type`] and nested credentials/paths.
    pub async fn build(cfg: &ObjectStorageConfig) -> OrbitResult<MegaObjectStorageWrapper> {
        cfg.validate()?;
        match cfg.storage_type {
            ObjectStorageBackend::S3 => build_s3_like(cfg, false).await,
            ObjectStorageBackend::S3Compatible => build_s3_like(cfg, true).await,
            ObjectStorageBackend::Gcs => build_gcs(cfg).await,
            ObjectStorageBackend::Local => build_local(cfg).await,
        }
    }
}

/// Shared S3 / S3-compatible construction (differs only by endpoint and upload strategy).
async fn build_s3_like(
    cfg: &ObjectStorageConfig,
    compatible: bool,
) -> OrbitResult<MegaObjectStorageWrapper> {
    let adapter = Arc::new(s3_adapter(&cfg.s3, compatible)?);

    Ok(MegaObjectStorageWrapper::new(adapter))
}

/// Data reads and writes use `endpoint_url`. On S3-compatible backends a
/// non-empty `public_endpoint_url` adds a store used only to sign presigned
/// URLs; real AWS S3 ignores the field and signs with its regional endpoint.
fn s3_adapter(s3_cfg: &S3Config, compatible: bool) -> OrbitResult<ObjectStoreAdapter> {
    let endpoint = compatible.then_some(s3_cfg.endpoint_url.as_str());
    let store = BackendStore::S3(Arc::new(s3_client(s3_cfg, endpoint)?));
    let presign_store = if compatible && !s3_cfg.public_endpoint_url.is_empty() {
        Some(Arc::new(s3_client(
            s3_cfg,
            Some(&s3_cfg.public_endpoint_url),
        )?))
    } else {
        None
    };
    let upload_strategy = if compatible {
        UploadStrategy::SinglePut
    } else {
        UploadStrategy::Multipart
    };
    Ok(ObjectStoreAdapter {
        store,
        upload_strategy,
        presign_store,
    })
}

/// Builds one `AmazonS3` client; `endpoint` is `Some` for S3-compatible stores.
fn s3_client(s3_cfg: &S3Config, endpoint: Option<&str>) -> OrbitResult<AmazonS3> {
    let mut builder = AmazonS3Builder::new()
        .with_region(&s3_cfg.region)
        .with_bucket_name(&s3_cfg.bucket)
        .with_access_key_id(&s3_cfg.access_key_id)
        .with_secret_access_key(&s3_cfg.secret_access_key);
    if let Some(endpoint) = endpoint {
        builder = builder
            .with_endpoint(endpoint)
            .with_allow_http(true)
            .with_virtual_hosted_style_request(false);
    }
    builder
        .build()
        .map_err(|e| IoOrbitError::Other(e.to_string()))
}

async fn build_gcs(cfg: &ObjectStorageConfig) -> OrbitResult<MegaObjectStorageWrapper> {
    let gcp_cfg = cfg.gcs.clone();
    let gcs = GoogleCloudStorageBuilder::from_env()
        .with_bucket_name(&gcp_cfg.bucket)
        .build()
        .map_err(|e| IoOrbitError::Other(e.to_string()))?;
    let store = BackendStore::Gcs(Arc::new(gcs));
    let adapter = Arc::new(ObjectStoreAdapter {
        store,
        upload_strategy: UploadStrategy::SinglePut,
        presign_store: None,
    });

    Ok(MegaObjectStorageWrapper::new(adapter))
}

async fn build_local(cfg: &ObjectStorageConfig) -> OrbitResult<MegaObjectStorageWrapper> {
    if !exists(&cfg.local.root_dir)? {
        create_dir_all(&cfg.local.root_dir)?
    }
    let fs = LocalFileSystem::new_with_prefix(&cfg.local.root_dir)
        .map_err(|e| IoOrbitError::Other(e.to_string()))?;

    let store = BackendStore::Local(Arc::new(fs));
    let adapter = Arc::new(ObjectStoreAdapter {
        store,
        upload_strategy: UploadStrategy::SinglePut,
        presign_store: None,
    });

    Ok(MegaObjectStorageWrapper::new(adapter))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use object_store::{path::Path, signer::Signer};
    use reqwest::Method;
    use url::Url;

    use super::*;
    use crate::orbit_api::object_storage::{ObjectKey, ObjectNamespace};

    fn pub_ep_sign_cfg(
        storage_type: ObjectStorageBackend,
        public_endpoint_url: &str,
    ) -> ObjectStorageConfig {
        ObjectStorageConfig {
            storage_type,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: "mega2".to_string(),
                access_key_id: "ak".to_string(),
                secret_access_key: "sk-secret".to_string(),
                endpoint_url: "http://rustfs:9000".to_string(),
                public_endpoint_url: public_endpoint_url.to_string(),
            },
            ..Default::default()
        }
    }

    async fn pub_ep_sign_origin(cfg: &ObjectStorageConfig) -> String {
        let storage = ObjectStorageFactory::build(cfg)
            .await
            .expect("build storage");
        let key = ObjectKey {
            namespace: ObjectNamespace::Artifact,
            key: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        };
        let url = storage
            .inner
            .signed_url(&key, Method::PUT, Duration::from_secs(60))
            .await
            .expect("sign")
            .expect("S3 backends sign URLs");
        Url::parse(&url)
            .expect("signed URL")
            .origin()
            .ascii_serialization()
    }

    async fn pub_ep_sign_store_origin(store: &AmazonS3) -> String {
        store
            .signed_url(Method::GET, &Path::from("probe"), Duration::from_secs(60))
            .await
            .expect("sign")
            .origin()
            .ascii_serialization()
    }

    #[tokio::test]
    async fn pub_ep_sign_uses_public_endpoint() {
        let cfg = pub_ep_sign_cfg(ObjectStorageBackend::S3Compatible, "http://127.0.0.1:29000");
        assert_eq!(pub_ep_sign_origin(&cfg).await, "http://127.0.0.1:29000");
    }

    #[tokio::test]
    async fn pub_ep_sign_data_store_keeps_endpoint_url() {
        let cfg = pub_ep_sign_cfg(ObjectStorageBackend::S3Compatible, "http://127.0.0.1:29000");
        let adapter = s3_adapter(&cfg.s3, true).expect("adapter");
        let BackendStore::S3(data) = &adapter.store else {
            panic!("S3-compatible adapter must hold an S3 store");
        };
        assert_eq!(pub_ep_sign_store_origin(data).await, "http://rustfs:9000");
        let presign = adapter.presign_store.as_ref().expect("presign store");
        assert_eq!(
            pub_ep_sign_store_origin(presign).await,
            "http://127.0.0.1:29000"
        );
    }

    #[tokio::test]
    async fn pub_ep_sign_defaults_to_endpoint() {
        let cfg = pub_ep_sign_cfg(ObjectStorageBackend::S3Compatible, "");
        assert!(
            s3_adapter(&cfg.s3, true)
                .expect("adapter")
                .presign_store
                .is_none()
        );
        assert_eq!(pub_ep_sign_origin(&cfg).await, "http://rustfs:9000");
    }

    #[tokio::test]
    async fn pub_ep_sign_s3_backend_ignores_public_endpoint() {
        let cfg = pub_ep_sign_cfg(ObjectStorageBackend::S3, "http://127.0.0.1:29000");
        assert!(
            s3_adapter(&cfg.s3, false)
                .expect("adapter")
                .presign_store
                .is_none()
        );
        assert_eq!(
            pub_ep_sign_origin(&cfg).await,
            "https://s3.us-east-1.amazonaws.com"
        );
    }

    #[test]
    fn pub_ep_sign_debug_includes_field() {
        let cfg = pub_ep_sign_cfg(ObjectStorageBackend::S3Compatible, "http://127.0.0.1:29000");
        let debug = format!("{:?}", cfg.s3);
        assert!(
            debug.contains("public_endpoint_url: \"http://127.0.0.1:29000\""),
            "{debug}"
        );
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(!debug.contains("sk-secret"), "{debug}");
    }
}
