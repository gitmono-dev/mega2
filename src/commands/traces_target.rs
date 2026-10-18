//! `mega2 traces rebind-target`: point one existing target at a directory that
//! is already canonical and already present. A rejected call does not write.

use std::sync::Arc;

use clap::{Arg, ArgMatches, Command};
use sea_orm::TransactionTrait;
use sea_orm_migration::MigratorTrait;

use crate::{
    commands::{CommandContext, require_config},
    common::errors::{MegaError, MegaResult},
    config::{Config, DbConfig, loader::ConfigSource},
    jupiter::{
        migration::Migrator,
        service::trace_target_service::rebind_if_path_exists,
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            init::{postgres_connection, read_only_database_connection},
            mono_storage::MonoStorage,
            trace_target_storage::strict_canonical_project_path,
        },
    },
};

pub fn cli() -> Command {
    Command::new("rebind-target")
        .about("Point an existing trace target at a canonical directory")
        .arg(
            Arg::new("target")
                .long("target")
                .value_name("UUID")
                .required(true)
                .help("Existing trace target UUID"),
        )
        .arg(
            Arg::new("path")
                .long("path")
                .value_name("PATH")
                .required(true)
                .help("Canonical project path that is already a directory"),
        )
}

pub async fn rebind(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let source = ctx.config_summary.as_ref().map(|summary| summary.source);
    let config = require_config(ctx, "traces rebind-target")?;
    require_named_config(source)?;
    let raw_target = args
        .get_one::<String>("target")
        .ok_or_else(|| MegaError::cli_exit(1, "traces rebind-target: --target is required"))?;
    let target_id = parse_target_id(raw_target)?;
    let raw_path = args
        .get_one::<String>("path")
        .ok_or_else(|| MegaError::cli_exit(1, "traces rebind-target: --path is required"))?;
    let path = strict_canonical_project_path(raw_path).map_err(|_| {
        MegaError::cli_exit(
            1,
            format!("traces rebind-target: path `{raw_path}` is not a canonical project path"),
        )
    })?;
    preflight(&config)?;
    ensure_schema_current(&config.database).await?;
    let connection = Arc::new(postgres_connection(&config.database).await?);
    let mono = MonoStorage {
        base: BaseStorage::new(connection.clone()),
    };
    let txn = connection.begin().await?;
    let exists = directory_exists(&mono, &txn, &path).await?;
    let rebound = match rebind_if_path_exists(&txn, &target_id, &path, exists).await {
        Ok(row) => row,
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(rebind_error(error));
        }
    };
    txn.commit().await?;
    println!("rebound {} {}", rebound.id, rebound.project_path);
    Ok(())
}

fn parse_target_id(raw: &str) -> Result<String, MegaError> {
    let id = uuid::Uuid::parse_str(raw).map_err(|_| {
        MegaError::cli_exit(
            1,
            format!("traces rebind-target: target `{raw}` is not a UUID"),
        )
    })?;
    Ok(id.to_string())
}

/// A maintenance run must name its config. A file found in the working
/// directory, or the global default, is not a named operator config.
fn require_named_config(source: Option<ConfigSource>) -> Result<(), MegaError> {
    match source {
        Some(ConfigSource::Cli | ConfigSource::Env) => Ok(()),
        other => Err(MegaError::cli_exit(
            1,
            format!(
                "traces rebind-target: refusing a config that was not named (source: {}); pass \
                 --config <path> or set MEGA_CONFIG",
                other.map_or("none", ConfigSource::as_str)
            ),
        )),
    }
}

fn preflight(config: &Config) -> Result<(), MegaError> {
    config.validate()?;
    config.monorepo.ensure_normal_service_object_format()?;
    Ok(())
}

async fn ensure_schema_current(db: &DbConfig) -> Result<(), MegaError> {
    let read_only = read_only_database_connection(db).await?;
    let pending = Migrator::get_pending_migrations_read_only(&read_only).await;
    let _ = read_only.close().await;
    match pending {
        Ok(pending) if pending.is_empty() => Ok(()),
        Ok(pending) => Err(MegaError::cli_exit(
            1,
            format!(
                "traces rebind-target: the database schema is behind this mega2 build ({} pending \
                 migrations); this command never migrates",
                pending.len()
            ),
        )),
        Err(error) => Err(MegaError::cli_exit(
            1,
            format!(
                "traces rebind-target: cannot confirm the database schema matches this mega2 \
                 build: {error}"
            ),
        )),
    }
}

async fn directory_exists(
    mono: &MonoStorage,
    txn: &sea_orm::DatabaseTransaction,
    path: &str,
) -> Result<bool, MegaError> {
    let Some(root) = mono.get_main_ref_in_txn("/", txn).await? else {
        return Ok(false);
    };
    Ok(mono
        .resolve_path_tree_hash_in_txn(&root.ref_tree_hash, path, txn)
        .await?
        .is_some())
}

fn rebind_error(error: MegaError) -> MegaError {
    let text = error.to_string();
    if text.contains("target is missing") {
        MegaError::cli_exit(1, "traces rebind-target: target does not exist")
    } else if text.contains("new canonical path does not exist") {
        MegaError::cli_exit(
            1,
            "traces rebind-target: canonical path is not an existing directory",
        )
    } else if text.contains("destination already has an active target") {
        MegaError::cli_exit(
            1,
            "traces rebind-target: destination already has an active target",
        )
    } else if text.contains("not a canonical project path") {
        MegaError::cli_exit(
            1,
            "traces rebind-target: path is not a canonical project path",
        )
    } else {
        MegaError::cli_exit(1, format!("traces rebind-target: {text}"))
    }
}
