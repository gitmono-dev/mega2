use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{
    error::{IoOrbitError, OrbitResult},
    log_storage::LogStorage,
    object_storage::MegaObjectStorage,
};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct S3Config {
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub endpoint_url: String,
}

impl std::fmt::Debug for S3Config {
    /// Custom `Debug` that redacts `secret_access_key` so credentials never leak
    /// through `{:?}` output, tracing, or panic messages.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"[REDACTED]")
            .field("endpoint_url", &self.endpoint_url)
            .finish()
    }
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct GcsConfig {
    /// GCS bucket name
    pub bucket: String,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct LocalConfig {
    /// Root directory for object storage
    pub root_dir: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "lowercase")]
pub enum ObjectStorageBackend {
    S3,
    S3Compatible,
    Gcs,
    #[default]
    Local,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ObjectStorageConfig {
    /// Global backend for Git blobs, Git LFS, and artifact protocol objects.
    #[serde(default)]
    pub storage_type: ObjectStorageBackend,
    /// S3 / S3-compatible credentials. Required when `storage_type` is `s3` or
    /// `s3compatible`; omit or leave empty for `local` / `gcs`.
    #[serde(default)]
    pub s3: S3Config,

    /// GCS credentials. Required when `storage_type` is `gcs`; omit or leave
    /// empty for `local` / `s3` / `s3compatible`.
    #[serde(default)]
    pub gcs: GcsConfig,

    /// Local filesystem root. Required when `storage_type` is `local`; omit or
    /// leave empty for cloud backends.
    #[serde(default)]
    pub local: LocalConfig,
}

impl ObjectStorageConfig {
    /// Validates that the fields required by the selected backend are present.
    /// Call this at `ObjectStorageFactory::build` entry so invalid configuration
    /// fails fast with a clear message instead of at first I/O.
    pub fn validate(&self) -> OrbitResult<()> {
        match self.storage_type {
            ObjectStorageBackend::S3 => self.validate_s3(false),
            ObjectStorageBackend::S3Compatible => self.validate_s3(true),
            ObjectStorageBackend::Gcs => require(&self.gcs.bucket, "gcs.bucket"),
            ObjectStorageBackend::Local => require(&self.local.root_dir, "local.root_dir"),
        }
    }

    fn validate_s3(&self, compatible: bool) -> OrbitResult<()> {
        require(&self.s3.region, "s3.region")?;
        require(&self.s3.bucket, "s3.bucket")?;
        require(&self.s3.access_key_id, "s3.access_key_id")?;
        require(&self.s3.secret_access_key, "s3.secret_access_key")?;
        if compatible {
            // S3-compatible backends (MinIO/RustFS/…) must set an endpoint,
            // otherwise the client would silently target real AWS.
            require(&self.s3.endpoint_url, "s3.endpoint_url")?;
        }
        Ok(())
    }
}

/// Rejects an empty/whitespace-only required config field with a clear,
/// secret-free error (only the field name is included, never the value).
fn require(value: &str, field: &str) -> OrbitResult<()> {
    if value.trim().is_empty() {
        return Err(IoOrbitError::Other(format!(
            "invalid object storage config: `{field}` must not be empty"
        )));
    }
    Ok(())
}

pub trait MegaObjectStorageWithLog: MegaObjectStorage + LogStorage {}

impl<T: MegaObjectStorage + LogStorage> MegaObjectStorageWithLog for T {}

#[derive(Clone)]
pub struct MegaObjectStorageWrapper {
    pub inner: Arc<dyn MegaObjectStorageWithLog>,
}

impl MegaObjectStorageWrapper {
    pub fn new(inner: Arc<dyn MegaObjectStorageWithLog>) -> Self {
        Self { inner }
    }

    pub fn supports_presigned_urls(&self) -> bool {
        MegaObjectStorage::supports_presigned_urls(&*self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_debug_redacts_secret() {
        let cfg = S3Config {
            region: "us-east-1".into(),
            bucket: "b".into(),
            access_key_id: "AKIA_EXAMPLE".into(),
            secret_access_key: "super-secret-value".into(),
            endpoint_url: String::new(),
        };
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("super-secret-value"),
            "secret leaked in Debug: {dbg}"
        );
        assert!(dbg.contains("[REDACTED]"));
    }

    #[test]
    fn s3_secret_not_leaked_via_enclosing_config_debug() {
        let cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3,
            s3: S3Config {
                secret_access_key: "top-secret".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!format!("{cfg:?}").contains("top-secret"));
    }

    #[test]
    fn omitted_unused_backend_sections_deserialize_and_validate() {
        let local_only: ObjectStorageConfig = toml::from_str(
            r#"
storage_type = "local"
[local]
root_dir = "/tmp/objects"
"#,
        )
        .expect("local may omit s3/gcs");
        assert!(local_only.validate().is_ok());
        assert!(local_only.s3.bucket.is_empty());
        assert!(local_only.gcs.bucket.is_empty());

        let s3_compat: ObjectStorageConfig = toml::from_str(
            r#"
storage_type = "s3compatible"
[s3]
region = "us-east-1"
bucket = "mega2"
access_key_id = "ak"
secret_access_key = "sk"
endpoint_url = "http://127.0.0.1:9000"
"#,
        )
        .expect("s3compatible may omit gcs/local");
        assert!(s3_compat.validate().is_ok());
        assert!(s3_compat.gcs.bucket.is_empty());
        assert!(s3_compat.local.root_dir.is_empty());

        let gcs_only: ObjectStorageConfig = toml::from_str(
            r#"
storage_type = "gcs"
[gcs]
bucket = "mega2"
"#,
        )
        .expect("gcs may omit s3/local");
        assert!(gcs_only.validate().is_ok());
        assert!(gcs_only.s3.bucket.is_empty());
    }

    #[test]
    fn validate_rejects_empty_required_fields() {
        // S3 requires region/bucket/access_key_id/secret_access_key (no endpoint).
        let mut cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3,
            ..Default::default()
        };
        assert!(cfg.validate().is_err(), "empty S3 config must be rejected");
        cfg.s3 = S3Config {
            region: "r".into(),
            bucket: "b".into(),
            access_key_id: "a".into(),
            secret_access_key: "s".into(),
            endpoint_url: String::new(),
        };
        assert!(
            cfg.validate().is_ok(),
            "plain S3 does not require an endpoint"
        );

        // S3Compatible additionally requires endpoint_url.
        cfg.storage_type = ObjectStorageBackend::S3Compatible;
        assert!(
            cfg.validate().is_err(),
            "s3-compatible must require an endpoint"
        );
        cfg.s3.endpoint_url = "http://localhost:9000".into();
        assert!(cfg.validate().is_ok());

        // Gcs requires bucket.
        let gcs_bad = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Gcs,
            ..Default::default()
        };
        assert!(gcs_bad.validate().is_err());
        let gcs_ok = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Gcs,
            gcs: GcsConfig { bucket: "b".into() },
            ..Default::default()
        };
        assert!(gcs_ok.validate().is_ok());

        // Local requires root_dir.
        let local_bad = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            ..Default::default()
        };
        assert!(local_bad.validate().is_err());
        let local_ok = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: "/tmp/orbit-test".into(),
            },
            ..Default::default()
        };
        assert!(local_ok.validate().is_ok());
    }
}
