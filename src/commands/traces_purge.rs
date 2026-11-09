//! Internal driver for the trusted legacy-Capture maintenance manifest
//! (MTR-56). It reads one explicitly named manifest file, verifies every
//! provenance source through [`trace_legacy_manifest`]`::verify_and_freeze`
//! and stores the frozen digest in the purge journal. The public
//! `purge-legacy-capture` command is wired by MTR-33, not here.

use std::path::Path;

use sea_orm::DatabaseConnection;

use crate::{
    config::{Config, loader::ConfigSource},
    jupiter::service::trace_legacy_manifest::{
        FrozenLegacyManifest, current_backend_from_config, verify_and_freeze,
    },
};

/// Read and freeze one manifest file. The database connection is supplied by
/// the caller so this driver never opens a pool or runs migrations.
pub(crate) async fn freeze_manifest_file(
    db: &DatabaseConnection,
    config: &Config,
    manifest_path: &Path,
) -> Result<FrozenLegacyManifest, String> {
    let raw = std::fs::read_to_string(manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    let current = current_backend_from_config(&config.object_storage);
    verify_and_freeze(db, &current, &raw)
        .await
        .map_err(|error| error.to_string())
}

/// The maintenance driver refuses a config that was found implicitly: only an
/// explicitly named `--config`/`MEGA_CONFIG` may authorize a purge manifest.
pub(crate) fn require_named_config(source: ConfigSource) -> Result<(), String> {
    match source {
        ConfigSource::Cli | ConfigSource::Env => Ok(()),
        other => Err(format!(
            "the legacy purge manifest needs an explicitly named config, got {other:?}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{freeze_manifest_file, require_named_config};
    use crate::{
        config::{Config, loader::ConfigSource},
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    fn manifest() -> String {
        r#"{
            "deployment_id": "deploy-driver",
            "backend_kind": "local",
            "storage_location": "/srv/mega2/objects",
            "historical_locations": ["local:/srv/mega2/objects"],
            "scopes": [{"deployment_id": "deploy-driver", "tenant_id": "tenant-a"}],
            "operator": {"identity": "operator@example.invalid", "statement_digest": "sha256:statement"}
        }"#
        .to_owned()
    }

    fn config() -> Config {
        let sample = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config/config.toml"),
        )
        .expect("sample config");
        let mut config: Config = toml::from_str(&sample).expect("config");
        config.object_storage.storage_type = crate::orbit_api::factory::ObjectStorageBackend::Local;
        config.object_storage.local.root_dir = "/srv/mega2/objects".to_owned();
        config
    }

    #[tokio::test]
    async fn legacy_manifest_driver_freezes_and_blocks() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let path = temp_dir.path().join("manifest.json");
        std::fs::write(&path, manifest()).expect("manifest");

        let frozen = freeze_manifest_file(&db, &config(), &path)
            .await
            .expect("freeze through the driver");
        assert!(frozen.digest.starts_with("sha256:"));

        let missing = freeze_manifest_file(&db, &config(), &temp_dir.path().join("absent.json"))
            .await
            .expect_err("a missing manifest must fail");
        assert!(missing.contains("cannot read"), "{missing}");

        let implicit = require_named_config(ConfigSource::DefaultGenerated)
            .expect_err("an implicit config must be refused");
        assert!(implicit.contains("explicitly named"), "{implicit}");
        require_named_config(ConfigSource::Cli).expect("named config");
        db.close().await.ok();
    }
}
