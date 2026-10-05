use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command};
use sea_orm_migration::MigratorTrait;

use crate::{
    commands::{CommandContext, require_config},
    common::errors::{MegaError, MegaResult},
    config::{DbConfig, loader::ConfigSource},
    jupiter::{
        migration::Migrator,
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            init::{postgres_connection, read_only_database_connection},
            mono_storage::MonoStorage,
            native_publication_storage::NativeRoot,
        },
    },
};

#[path = "native_publication_init_preflight.rs"]
mod preflight;
use preflight::InitializationTarget;

pub fn cli() -> Command {
    let mut command = Command::new("native-publication-init")
        .about("Prepare native publication in a stopped, paused and drained SHA-1 deployment");
    for (name, help) in [
        ("instance", "Deployment UUID; must match mst2.instance_uuid"),
        (
            "expected-root-commit",
            "Exact current native root commit object ID",
        ),
        (
            "expected-root-tree",
            "Exact current native root tree object ID",
        ),
    ] {
        command = command.arg(Arg::new(name).long(name).required(true).help(help));
    }
    for (name, help) in [
        ("yes", "Confirm preparation of the native publication head"),
        (
            "writers-stopped",
            "Confirm all old writer processes have been stopped",
        ),
    ] {
        command = command.arg(
            Arg::new(name)
                .long(name)
                .action(ArgAction::SetTrue)
                .required(true)
                .help(help),
        );
    }
    command
}

pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    if !ctx
        .config_summary
        .as_ref()
        .is_some_and(|summary| matches!(summary.source, ConfigSource::Cli | ConfigSource::Env))
    {
        return Err(MegaError::Other(
            "MST2_NATIVE_INIT_CONFIG_REQUIRED: name the deployment with --config or MEGA_CONFIG"
                .into(),
        ));
    }
    if !args.get_flag("yes") || !args.get_flag("writers-stopped") {
        return Err(MegaError::Other(
            "MST2_NATIVE_INIT_CONFIRMATION_REQUIRED: --yes and --writers-stopped are required"
                .into(),
        ));
    }
    let config = require_config(ctx, "service native-publication-init")?;
    config.validate()?;
    if !config.mst2.enabled || !config.mst2.publication_enabled {
        return Err(MegaError::Other(
            "MST2_NATIVE_INIT_DISABLED: mst2.enabled and mst2.publication_enabled must be true"
                .into(),
        ));
    }
    let argument = |name| {
        args.get_one::<String>(name)
            .map(String::as_str)
            .ok_or_else(|| MegaError::Other(format!("--{name} is required")))
    };
    let target = InitializationTarget::parse(
        config.monorepo.object_format.as_str(),
        config.mst2.instance_uuid.as_deref(),
        argument("instance")?,
        argument("expected-root-commit")?,
        argument("expected-root-tree")?,
    )
    .map_err(|error| MegaError::Other(error.to_string()))?;
    ensure_schema_current(&config.database).await?;
    let db = Arc::new(postgres_connection(&config.database).await?);
    let mono = MonoStorage {
        base: BaseStorage::new(db.clone()),
    };
    let result = mono
        .initialize_native_publication_for_maintenance(
            &target.instance,
            &NativeRoot {
                commit: target.commit,
                tree: target.tree,
            },
        )
        .await
        .map_err(|error| MegaError::Other(error.to_string()));
    let close = db.close_by_ref().await;
    result?;
    close?;
    tracing::info!(
        instance = %target.instance,
        state = "INITIALIZING",
        "Native publication initialization prepared; queue remains paused"
    );
    Ok(())
}

async fn ensure_schema_current(config: &DbConfig) -> MegaResult {
    let db = read_only_database_connection(config).await?;
    let pending = Migrator::get_pending_migrations_read_only(&db).await;
    let _ = db.close().await;
    let pending = pending.map_err(|error| {
        MegaError::Other(format!(
            "MST2_NATIVE_INIT_SCHEMA_MISMATCH: cannot confirm the current schema: {error}"
        ))
    })?;
    if !pending.is_empty() {
        return Err(MegaError::Other(format!(
            "MST2_NATIVE_INIT_SCHEMA_MISMATCH: {} migrations are pending; this command does not migrate",
            pending.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{LoadMode, load_mode};

    #[test]
    fn maintenance_cli_requires_each_confirmation_and_fixed_target() {
        let arguments = [
            "native-publication-init",
            "--yes",
            "--writers-stopped",
            "--instance",
            "instance",
            "--expected-root-commit",
            "commit",
            "--expected-root-tree",
            "tree",
        ];
        assert!(cli().try_get_matches_from(arguments).is_ok());
        for remove in [1, 2, 3, 5, 7] {
            let count = if remove <= 2 { 1 } else { 2 };
            let mut incomplete = arguments.to_vec();
            incomplete.drain(remove..remove + count);
            assert!(cli().try_get_matches_from(incomplete).is_err());
        }
        let mut service_arguments = vec!["service"];
        service_arguments.extend(arguments);
        let args = super::super::cli()
            .try_get_matches_from(service_arguments)
            .unwrap();
        assert_eq!(
            load_mode("service", &args),
            Some(LoadMode::ParsedExistingConfig)
        );
    }

    #[tokio::test]
    async fn maintenance_command_refuses_unnamed_config_before_any_database_io() {
        let args = cli()
            .try_get_matches_from([
                "native-publication-init",
                "--yes",
                "--writers-stopped",
                "--instance",
                "instance",
                "--expected-root-commit",
                "commit",
                "--expected-root-tree",
                "tree",
            ])
            .unwrap();
        let error = exec(CommandContext::default(), &args).await.unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("MST2_NATIVE_INIT_CONFIG_REQUIRED")
        );
    }
}
