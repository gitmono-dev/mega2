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

/// Read the scope-authorization manifest of one whole database and run the
/// preflight. This driver never deletes; it returns the aggregate receipt the
/// destructive step must carry.
pub(crate) async fn preflight_scopes_file(
    db: &DatabaseConnection,
    manifest_path: &Path,
) -> Result<crate::jupiter::service::trace_legacy_scope_guard::AggregateScopeReceipt, String> {
    let raw = std::fs::read_to_string(manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    crate::jupiter::service::trace_legacy_scope_guard::preflight_legacy_scopes(db, &raw)
        .await
        .map_err(|error| error.to_string())
}

/// Prepare the legacy writer roles from one explicitly named plan file.
pub(crate) async fn prepare_roles_file(
    db: &DatabaseConnection,
    plan_path: &Path,
) -> Result<crate::jupiter::service::trace_legacy_roles::LegacyRolePreparation, String> {
    let raw = std::fs::read_to_string(plan_path)
        .map_err(|error| format!("cannot read {}: {error}", plan_path.display()))?;
    let plan: crate::jupiter::service::trace_legacy_roles::LegacyRolePlan =
        serde_json::from_str(&raw).map_err(|error| format!("invalid role plan: {error}"))?;
    crate::jupiter::service::trace_legacy_roles::prepare_legacy_roles(db, &plan)
        .await
        .map_err(|error| error.to_string())
}

/// Revoke the old app role's effective INSERT/UPDATE/DELETE from one plan.
pub(crate) async fn revoke_dml_file(
    db: &DatabaseConnection,
    plan_path: &Path,
) -> Result<crate::jupiter::service::trace_legacy_roles::LegacyDmlRevocation, String> {
    let raw = std::fs::read_to_string(plan_path)
        .map_err(|error| format!("cannot read {}: {error}", plan_path.display()))?;
    let plan: crate::jupiter::service::trace_legacy_roles::LegacyRolePlan =
        serde_json::from_str(&raw).map_err(|error| format!("invalid role plan: {error}"))?;
    crate::jupiter::service::trace_legacy_roles::revoke_legacy_dml(db, &plan)
        .await
        .map_err(|error| error.to_string())
}

pub fn prepare_cli() -> clap::Command {
    clap::Command::new("prepare-legacy-capture")
        .about("Prepare legacy Capture maintenance without deleting data")
        .arg(
            clap::Arg::new("action")
                .long("action")
                .required(true)
                .value_parser(["freeze", "preflight", "prepare-roles", "revoke-dml"])
                .help(
                    "Preparation action; role preparation and revocation precede fence activation",
                ),
        )
        .arg(
            clap::Arg::new("input")
                .long("input")
                .value_name("PATH")
                .required(true)
                .help("Provenance manifest, scope manifest or role plan for the selected action"),
        )
}

pub(crate) async fn prepare(
    ctx: crate::commands::CommandContext,
    args: &clap::ArgMatches,
) -> crate::common::errors::MegaResult {
    use crate::common::errors::MegaError;

    let error = |message: String| {
        MegaError::cli_exit(1, format!("traces prepare-legacy-capture: {message}"))
    };
    let source = ctx
        .config_summary
        .as_ref()
        .map(|summary| summary.source)
        .ok_or_else(|| error("pass --config <path> or set MEGA_CONFIG".to_owned()))?;
    require_named_config(source).map_err(error)?;
    let config = crate::commands::require_config(ctx, "traces prepare-legacy-capture")?;
    let action = args
        .get_one::<String>("action")
        .ok_or_else(|| error("--action is required".to_owned()))?;
    let input = args
        .get_one::<String>("input")
        .ok_or_else(|| error("--input is required".to_owned()))?;
    let input = Path::new(input);
    // A preparation command must not migrate the deployment it is inspecting.
    super::ensure_schema_current(&config.database).await?;
    let db = crate::jupiter::storage::init::postgres_connection(&config.database).await?;
    let result = match action.as_str() {
        "freeze" => freeze_manifest_file(&db, &config, input)
            .await
            .map(|receipt| {
                format!(
                    "freeze journal={} digest={}",
                    receipt.journal_id, receipt.digest
                )
            }),
        "preflight" => preflight_scopes_file(&db, input).await.map(|receipt| {
            format!(
                "preflight aggregate={} scopes={} owners={}",
                receipt.digest,
                receipt.scope_count,
                receipt.owners.len()
            )
        }),
        "prepare-roles" => prepare_roles_file(&db, input).await.map(|receipt| {
            format!(
                "prepare-roles tables={} barrier={} app_plain={} cannot_alter={} cannot_disable_trigger={}",
                receipt.tables_transferred,
                receipt.barrier_transferred,
                receipt.app_role_plain,
                receipt.app_cannot_alter,
                receipt.app_cannot_disable_trigger
            )
        }),
        "revoke-dml" => revoke_dml_file(&db, input).await.map(|receipt| {
            format!(
                "revoke-dml tables={} insert={} update={} delete={} public_insert={} public_update={} public_delete={}",
                receipt.tables_revoked,
                receipt.insert_revoked,
                receipt.update_revoked,
                receipt.delete_revoked,
                receipt.public_insert_revoked,
                receipt.public_update_revoked,
                receipt.public_delete_revoked
            )
        }),
        _ => Err("unknown preparation action".to_owned()),
    };
    // Close even when a driver fails; do not emit a success receipt before it.
    let closed = db.close().await;
    let receipt = result.map_err(error)?;
    closed.map_err(|_| error("cannot close the maintenance connection".to_owned()))?;
    println!("{receipt}");
    Ok(())
}

/// `traces purge-legacy-capture`: the public maintenance entry point frozen by
/// MTR-33. `--action fence` re-runs the whole-database preflight and activates
/// the persistent writer fence; the later inventory/delete/drop actions are
/// wired by their own cards and are refused here.
pub fn cli() -> clap::Command {
    clap::Command::new("purge-legacy-capture")
        .about("Legacy Capture purge maintenance (fence/inventory/delete/drop)")
        .arg(
            clap::Arg::new("action")
                .long("action")
                .value_name("ACTION")
                .required(true)
                .value_parser(["fence", "inventory", "delete", "drop"])
                .help("Maintenance action"),
        )
        .arg(
            clap::Arg::new("journal")
                .long("journal")
                .value_name("UUID")
                .required(true)
                .help("Frozen purge journal UUID"),
        )
        .arg(
            clap::Arg::new("maintenance-manifest")
                .long("maintenance-manifest")
                .value_name("PATH")
                .required(true)
                .help("Controlled maintenance manifest file"),
        )
}

pub(crate) async fn exec(
    ctx: crate::commands::CommandContext,
    args: &clap::ArgMatches,
) -> crate::common::errors::MegaResult {
    use crate::common::errors::MegaError;
    let config = crate::commands::require_config(ctx, "traces purge-legacy-capture")?;
    super::ensure_schema_current(&config.database).await?;
    let connection = std::sync::Arc::new(
        crate::jupiter::storage::init::postgres_connection(&config.database).await?,
    );
    let action = args
        .get_one::<String>("action")
        .map(String::as_str)
        .unwrap_or_default();
    let journal = args
        .get_one::<String>("journal")
        .cloned()
        .unwrap_or_default();
    let manifest_path = std::path::PathBuf::from(
        args.get_one::<String>("maintenance-manifest")
            .cloned()
            .unwrap_or_default(),
    );
    match action {
        "fence" => {
            let raw = std::fs::read_to_string(&manifest_path).map_err(|error| {
                MegaError::cli_exit(
                    1,
                    format!("cannot read {}: {error}", manifest_path.display()),
                )
            })?;
            let receipt = crate::jupiter::service::trace_legacy_fence::activate_fence(
                connection.as_ref(),
                &journal,
                &raw,
            )
            .await
            .map_err(|error| MegaError::cli_exit(1, error.to_string()))?;
            println!(
                "fence {} journal {} aggregate {} receipt {} {}",
                receipt.fence_id,
                receipt.journal_id,
                receipt.aggregate_digest,
                receipt.receipt_digest,
                if receipt.already_active {
                    "already-active"
                } else {
                    "activated"
                }
            );
            Ok(())
        }
        "inventory" => {
            let raw = std::fs::read_to_string(&manifest_path).map_err(|error| {
                MegaError::cli_exit(
                    1,
                    format!("cannot read {}: {error}", manifest_path.display()),
                )
            })?;
            let adapter =
                crate::orbit::factory::ObjectStorageFactory::build_adapter(&config.object_storage)
                    .await
                    .map_err(|error| MegaError::cli_exit(1, error.to_string()))?;
            let outcome = crate::jupiter::service::trace_legacy_inventory::freeze_legacy_inventory(
                connection.as_ref(),
                &adapter,
                &journal,
                &raw,
            )
            .await
            .map_err(|error| MegaError::cli_exit(1, error.to_string()))?;
            println!(
                "inventory {} prefixes={} items={} capability={}",
                outcome.journal_id,
                outcome.prefixes.join(","),
                outcome.items,
                outcome.capability
            );
            Ok(())
        }
        "delete" => {
            let raw = std::fs::read_to_string(&manifest_path).map_err(|error| {
                MegaError::cli_exit(
                    1,
                    format!("cannot read {}: {error}", manifest_path.display()),
                )
            })?;
            let adapter =
                crate::orbit::factory::ObjectStorageFactory::build_adapter(&config.object_storage)
                    .await
                    .map_err(|error| MegaError::cli_exit(1, error.to_string()))?;
            let outcome = crate::jupiter::service::trace_legacy_purge::delete_legacy_objects(
                connection.as_ref(),
                &adapter,
                &journal,
                &raw,
            )
            .await
            .map_err(|error| MegaError::cli_exit(1, error.to_string()))?;
            println!(
                "delete {} deleted={} already={} remaining={} rescan_empty={} owners={}",
                outcome.journal_id,
                outcome.deleted,
                outcome.already_deleted,
                outcome.remaining,
                outcome.rescan_empty,
                outcome.owners.join(",")
            );
            Ok(())
        }
        "drop" => {
            // A repeated maintenance call is a no-op even though the legacy
            // tables are already gone.
            if crate::jupiter::migration::traces_maintenance::drop_legacy_capture::
                maintenance_applied(connection.as_ref())
                .await
                .map_err(|error| MegaError::cli_exit(1, error.to_string()))?
            {
                println!("drop {} dropped=false already_applied=true", journal);
                return Ok(());
            }
            let adapter =
                crate::orbit::factory::ObjectStorageFactory::build_adapter(&config.object_storage)
                    .await
                    .map_err(|error| MegaError::cli_exit(1, error.to_string()))?;
            let receipt =
                verify_receipt_file(connection.as_ref(), &adapter, &journal, &manifest_path)
                    .await
                    .map_err(|error| MegaError::cli_exit(1, error))?;
            let outcome =
                crate::jupiter::migration::traces_maintenance::drop_legacy_capture::execute(
                    connection.as_ref(),
                    &receipt,
                )
                .await
                .map_err(|error| MegaError::cli_exit(1, error.to_string()))?;
            println!(
                "drop {} dropped={} already_applied={} receipt={}",
                journal,
                outcome.dropped,
                outcome.already_applied,
                receipt.digest()
            );
            Ok(())
        }
        other => Err(MegaError::cli_exit(
            1,
            format!("purge-legacy-capture --action {other} is not wired in this release"),
        )),
    }
}

/// Verify one journal against one explicitly named maintenance manifest and
/// return the receipt the guarded DROP consumes.
pub(crate) async fn verify_receipt_file(
    db: &DatabaseConnection,
    adapter: &crate::orbit::ObjectStoreAdapter,
    journal: &str,
    manifest_path: &Path,
) -> Result<crate::jupiter::service::trace_legacy_receipt::VerifiedLegacyPurgeReceipt, String> {
    let raw = std::fs::read_to_string(manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    crate::jupiter::service::trace_legacy_receipt::verify_purge_complete(db, adapter, journal, &raw)
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

    #[tokio::test]
    async fn legacy_scope_preflight_driver_reads_one_manifest() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        use sea_orm::ConnectionTrait;
        db.execute_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Postgres,
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind, completeness)
             VALUES ('deploy-a', 'tenant-a', 'repo', 'producer', 'driver-session', 'external_capture', 'complete')",
        ))
        .await
        .expect("legacy session");
        let path = temp_dir.path().join("scopes.json");
        std::fs::write(
            &path,
            r#"{
                "bucket_scope": "mega2-legacy/",
                "owners": [{"owner": "owner-a", "deployment_id": "deploy-a", "tenant_id": "tenant-a", "receipt_id": "receipt-a", "complete": true}],
                "orphan_prefixes": []
            }"#,
        )
        .expect("manifest");
        let receipt = super::preflight_scopes_file(&db, &path)
            .await
            .expect("preflight through the driver");
        assert_eq!(receipt.scope_count, 1, "one enumerated scope");
        assert_eq!(receipt.owners, vec!["owner-a".to_owned()]);
        let missing = super::preflight_scopes_file(&db, &temp_dir.path().join("absent.json"))
            .await
            .expect_err("a missing manifest must fail");
        assert!(missing.contains("cannot read"), "{missing}");
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_role_driver_prepares_from_one_plan() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        use sea_orm::ConnectionTrait;
        let app = format!("mega2_l61d_app_{}", std::process::id());
        let maintenance = format!("mega2_l61d_maint_{}", std::process::id());
        for role in [&app, &maintenance] {
            db.execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
                .await
                .ok();
            db.execute_unprepared(&format!("CREATE ROLE {role} NOLOGIN"))
                .await
                .expect("create role");
        }
        let path = temp_dir.path().join("roles.json");
        std::fs::write(
            &path,
            format!("{{\"app_role\": \"{app}\", \"maintenance_role\": \"{maintenance}\"}}"),
        )
        .expect("plan");
        let prepared = super::prepare_roles_file(&db, &path)
            .await
            .expect("prepare through the driver");
        assert_eq!(prepared.tables_transferred, 12);
        assert!(prepared.app_cannot_alter);
        let missing = super::prepare_roles_file(&db, &temp_dir.path().join("absent.json"))
            .await
            .expect_err("a missing plan must fail");
        assert!(missing.contains("cannot read"), "{missing}");
        for table in crate::jupiter::service::trace_legacy_roles::LEGACY_TABLES {
            let _ = db
                .execute_unprepared(&format!("ALTER TABLE {table} OWNER TO CURRENT_USER"))
                .await;
        }
        let _ = db
            .execute_unprepared("ALTER FUNCTION trace_legacy_write_fence() OWNER TO CURRENT_USER")
            .await;
        for role in [&app, &maintenance] {
            let _ = db
                .execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
                .await;
        }
        db.close().await.ok();
    }

    #[tokio::test]
    async fn legacy_dml_driver_revokes_from_one_plan() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        use sea_orm::ConnectionTrait;
        let app = format!("mega2_l67d_app_{}", std::process::id());
        let maintenance = format!("mega2_l67d_maint_{}", std::process::id());
        for role in [&app, &maintenance] {
            db.execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
                .await
                .ok();
            db.execute_unprepared(&format!("CREATE ROLE {role} NOLOGIN"))
                .await
                .expect("create role");
        }
        let plan = crate::jupiter::service::trace_legacy_roles::LegacyRolePlan {
            app_role: app.clone(),
            maintenance_role: maintenance.clone(),
        };
        crate::jupiter::service::trace_legacy_roles::prepare_legacy_roles(&db, &plan)
            .await
            .expect("ownership");
        db.execute_unprepared("GRANT INSERT, UPDATE, DELETE ON agent_capture_session TO PUBLIC")
            .await
            .expect("grant public");
        let path = temp_dir.path().join("roles.json");
        std::fs::write(
            &path,
            format!("{{\"app_role\": \"{app}\", \"maintenance_role\": \"{maintenance}\"}}"),
        )
        .expect("plan");
        let revoked = super::revoke_dml_file(&db, &path)
            .await
            .expect("revoke through the driver");
        assert_eq!(revoked.tables_revoked, 12);
        let missing = super::revoke_dml_file(&db, &temp_dir.path().join("absent.json"))
            .await
            .expect_err("a missing plan must fail");
        assert!(missing.contains("cannot read"), "{missing}");
        for table in crate::jupiter::service::trace_legacy_roles::LEGACY_TABLES {
            let _ = db
                .execute_unprepared(&format!("ALTER TABLE {table} OWNER TO CURRENT_USER"))
                .await;
        }
        let _ = db
            .execute_unprepared("ALTER FUNCTION trace_legacy_write_fence() OWNER TO CURRENT_USER")
            .await;
        for role in [&app, &maintenance] {
            let _ = db
                .execute_unprepared(&format!("DROP ROLE IF EXISTS {role}"))
                .await;
        }
        db.close().await.ok();
    }

    #[tokio::test]
    async fn purge_receipt_driver_reads_one_manifest() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let root = temp_dir.path().join("objects");
        std::fs::create_dir_all(&root).expect("root");
        let db = test_db_connection(temp_dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        let storage: crate::orbit_api::factory::ObjectStorageConfig = toml::from_str(&format!(
            "storage_type = \"local\"\n[local]\nroot_dir = \"{}\"\n",
            root.display()
        ))
        .expect("storage");
        let adapter = crate::orbit::factory::ObjectStorageFactory::build_adapter(&storage)
            .await
            .expect("adapter");
        let journal = "12121212-1212-4212-8212-121212121212";
        use sea_orm::ConnectionTrait;
        db.execute_unprepared(
            "INSERT INTO agent_capture_session
             (deployment_id, tenant_id, repo_id, producer_id, client_session_id, session_kind, completeness)
             VALUES ('deploy-d', 'tenant-d', 'repo', 'producer', 'driver-receipt', 'external_capture', 'complete')",
        )
        .await
        .expect("session");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge
             (id, backend_fingerprint, storage_location, deployment_id, manifest_digest, state)
             VALUES ('{journal}', 'local:{}', '{}', 'deploy-d', 'sha256:manifest', 'frozen')",
            root.display(),
            root.display()
        ))
        .await
        .expect("journal");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_purge_item (id, purge_id, scope, object_key, deleted_at)
             VALUES ('13131313-1313-4313-8313-131313131313', '{journal}', 'deploy-d', 'agent/raw/aa', now())"
        ))
        .await
        .expect("item");
        db.execute_unprepared(&format!(
            "INSERT INTO trace_legacy_writer_fence (id, purge_id, deployment_id, active)
             VALUES ('14141414-1414-4414-8414-141414141414', '{journal}', 'deploy-d', TRUE)"
        ))
        .await
        .expect("fence");
        let manifest = temp_dir.path().join("manifest.json");
        std::fs::write(
            &manifest,
            format!(
                r#"{{"bucket_scope":"mega2-legacy/","deployment_id":"deploy-d","backend_kind":"local","storage_location":"{}","historical_locations":["local:{}"],"owners":[{{"owner":"owner-a","deployment_id":"deploy-d","tenant_id":"tenant-d","receipt_id":"receipt-a","complete":true}}],"orphan_prefixes":[]}}"#,
                root.display(),
                root.display()
            ),
        )
        .expect("manifest");
        let receipt = super::verify_receipt_file(&db, &adapter, journal, &manifest)
            .await
            .expect("driver receipt");
        assert_eq!(receipt.digest().len(), 64);
        assert_eq!(receipt.journal_id(), journal);
        let missing = super::verify_receipt_file(
            &db,
            &adapter,
            journal,
            &temp_dir.path().join("absent.json"),
        )
        .await
        .expect_err("missing manifest");
        assert!(missing.contains("cannot read"), "{missing}");
        db.close().await.ok();
    }
}
