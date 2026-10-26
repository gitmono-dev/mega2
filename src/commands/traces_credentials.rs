//! Credential lifecycle maintenance through an explicitly named configuration.

use clap::{Arg, ArgMatches, Command};
use sea_orm_migration::MigratorTrait;

use crate::{
    commands::{CommandContext, require_config},
    common::errors::{MegaError, MegaResult},
    config::{DbConfig, loader::ConfigSource},
    jupiter::{
        migration::Migrator,
        storage::{
            init::{postgres_connection, read_only_database_connection},
            trace_credential_storage::TraceCredentialStorage,
        },
    },
};

pub fn cli() -> Command {
    let principal = Arg::new("principal")
        .long("principal")
        .value_name("UUID")
        .required(true)
        .help("Explicit stable principal UUID; created if absent");
    let binding = Arg::new("binding")
        .long("binding")
        .value_name("UUID")
        .required(true)
        .help("Existing credential binding UUID");
    let namespace = Arg::new("namespace")
        .long("namespace")
        .value_name("NAMESPACE")
        .required(true)
        .help("static_push_token or mono_account");
    let credential = Arg::new("credential")
        .long("credential")
        .value_name("KEY")
        .required(true)
        .help("Configured token name or access-token:<id>/ssh-key:<id>; never a secret");
    Command::new("credentials")
        .about("Bind, rotate or retire trace credentials")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("bind")
                .about("Bind an existing credential to an explicit principal")
                .arg(principal)
                .arg(namespace.clone())
                .arg(credential.clone()),
        )
        .subcommand(
            Command::new("rotate")
                .about("Replace a binding's credential while retaining its principal")
                .arg(binding.clone())
                .arg(namespace)
                .arg(credential),
        )
        .subcommand(
            Command::new("retire")
                .about("Retire a binding without deleting its history")
                .arg(binding),
        )
}

pub async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let source = ctx.config_summary.as_ref().map(|summary| summary.source);
    let config = require_config(ctx, "traces credentials")?;
    if !matches!(source, Some(ConfigSource::Cli | ConfigSource::Env)) {
        return Err(cli_error(
            "configuration was not named; pass --config <path> or set MEGA_CONFIG",
        ));
    }
    config
        .validate()
        .map_err(|_| cli_error("invalid maintenance configuration; run config validate"))?;
    config
        .monorepo
        .ensure_normal_service_object_format()
        .map_err(|_| cli_error("configuration does not use the supported service object format"))?;
    let Some((operation, options)) = args.subcommand() else {
        return Err(cli_error("choose bind, rotate or retire"));
    };
    let identity = match operation {
        "bind" => argument(options, "principal")?,
        "rotate" | "retire" => argument(options, "binding")?,
        _ => return Err(cli_error("choose bind, rotate or retire")),
    };
    let key = if operation == "retire" {
        None
    } else {
        Some((
            argument(options, "namespace")?,
            argument(options, "credential")?,
        ))
    };
    ensure_schema_current(&config.database).await?;
    let connection = postgres_connection(&config.database)
        .await
        .map_err(|_| cli_error("cannot connect to the configured database"))?;
    let result = {
        let storage = TraceCredentialStorage::new(&connection, &config.git.push_tokens);
        match (operation, key) {
            ("bind", Some((namespace, credential))) => {
                storage.bind(identity, namespace, credential).await
            }
            ("rotate", Some((namespace, credential))) => {
                storage.rotate(identity, namespace, credential).await
            }
            _ => storage.retire(identity).await,
        }
    };
    let closed = connection.close().await;
    let row = result.map_err(|error| cli_error(&error.to_string()))?;
    closed.map_err(|_| {
        cli_error("binding change committed, but database connection cleanup failed")
    })?;
    let state = if row.retired_at.is_some() {
        "retired"
    } else {
        "active"
    };
    println!("{operation} {} {} {state}", row.id, row.principal_id);
    Ok(())
}

fn argument<'a>(args: &'a ArgMatches, name: &str) -> Result<&'a str, MegaError> {
    args.get_one::<String>(name)
        .map(String::as_str)
        .ok_or_else(|| cli_error("a required credential-management argument is missing"))
}

fn cli_error(message: &str) -> MegaError {
    MegaError::cli_exit(1, format!("traces credentials: {message}"))
}

async fn ensure_schema_current(config: &DbConfig) -> Result<(), MegaError> {
    let connection = read_only_database_connection(config).await.map_err(|_| {
        cli_error("cannot open the configured database for a read-only schema check")
    })?;
    let pending = Migrator::get_pending_migrations_read_only(&connection).await;
    connection
        .close()
        .await
        .map_err(|_| cli_error("cannot close the schema-check connection"))?;
    match pending {
        Ok(pending) if pending.is_empty() => Ok(()),
        Ok(_) => Err(cli_error(
            "database schema is behind this build; this command never migrates",
        )),
        Err(_) => Err(cli_error(
            "cannot confirm the database schema matches this build",
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::commands::{CommandContext, LoadMode, builtin, builtin_exec, load_mode};

    #[test]
    fn credential_commands_use_registered_executor_and_existing_config() {
        for operation in ["bind", "rotate", "retire"] {
            let mut arguments = vec!["traces", "credentials", operation];
            arguments.extend([
                if operation == "bind" {
                    "--principal"
                } else {
                    "--binding"
                },
                "00000000-0000-0000-0000-000000000001",
            ]);
            if operation != "retire" {
                arguments.extend([
                    "--namespace",
                    "static_push_token",
                    "--credential",
                    "fixture",
                ]);
            }
            let command = builtin()
                .into_iter()
                .find(|command| command.get_name() == "traces")
                .expect("registered traces parent");
            let parsed = command
                .try_get_matches_from(arguments)
                .expect("credential arguments");
            assert_eq!(
                load_mode("traces", &parsed),
                Some(LoadMode::ParsedExistingConfig)
            );
            let credentials = parsed
                .subcommand_matches("credentials")
                .expect("credentials child");
            assert_eq!(credentials.subcommand_name(), Some(operation));
            let executor = builtin_exec("traces").expect("registered traces executor");
            let error =
                executor(CommandContext::default(), &parsed).expect_err("config is required");
            assert!(error.to_string().contains("traces credentials"));
        }
    }
}
