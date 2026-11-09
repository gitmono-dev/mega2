//! `mega2 traces` maintenance commands. Later traces maintenance commands
//! register another sibling under this parent. This command never generates a
//! default config.

#[path = "traces_credentials.rs"]
mod traces_credentials;
#[path = "traces_purge.rs"]
mod traces_purge;
#[path = "traces_search_index.rs"]
mod traces_search_index;
#[path = "traces_target.rs"]
mod traces_target;

use clap::{ArgMatches, Command};
use sea_orm_migration::MigratorTrait;

use crate::{
    commands::{CommandContext, require_config, unknown_subcommand},
    common::errors::{MegaError, MegaResult},
    config::{DbConfig, loader::ConfigSource},
    jupiter::{
        migration::Migrator,
        service::{trace_index_worker::TraceIndexWorker, trace_object_service::TraceObjectService},
        storage::{
            init::{postgres_connection, read_only_database_connection},
            object_storage::build_object_storage,
        },
    },
};

pub fn cli() -> Command {
    Command::new("traces")
        .about("Server-side traces maintenance")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(traces_target::cli())
        .subcommand(traces_credentials::cli())
        .subcommand(traces_search_index::cli())
        .subcommand(traces_purge::cli())
        .subcommand(rebuild_index_cli())
}

#[tokio::main(flavor = "current_thread")]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("rebind-target", sub)) => traces_target::rebind(ctx, sub).await,
        Some(("credentials", sub)) => traces_credentials::exec(ctx, sub).await,
        Some(("search-index", sub)) => traces_search_index::exec(ctx, sub).await,
        Some(("rebuild-index", sub)) => rebuild_index(ctx, sub).await,
        Some(("purge-legacy-capture", sub)) => traces_purge::exec(ctx, sub).await,
        Some((cmd, _)) => Err(unknown_subcommand(cmd)),
        None => Err(unknown_subcommand("traces")),
    }
}

fn rebuild_index_cli() -> Command {
    Command::new("rebuild-index")
        .about("Rebuild one stream's indexed prefix into a new fragment index version")
        .arg(
            clap::Arg::new("stream")
                .long("stream")
                .value_name("UUID")
                .required(true)
                .help("Trace stream to rebuild"),
        )
        .arg(
            clap::Arg::new("index-version")
                .long("index-version")
                .value_name("VERSION")
                .value_parser(clap::value_parser!(i64).range(1..))
                .required(true)
                .help("Positive fragment index version to write"),
        )
}

async fn rebuild_index(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let source = ctx.config_summary.as_ref().map(|summary| summary.source);
    let config = require_config(ctx, "traces rebuild-index")?;
    require_named_config(source)?;
    config.validate()?;
    let stream_id = args
        .get_one::<String>("stream")
        .ok_or_else(|| rebuild_index_error("--stream is required"))?;
    let stream_id = uuid::Uuid::parse_str(stream_id)
        .map_err(|_| rebuild_index_error("--stream must be a UUID"))?
        .to_string();
    let index_version = *args
        .get_one::<i64>("index-version")
        .ok_or_else(|| rebuild_index_error("--index-version must be positive"))?;
    ensure_schema_current(&config.database).await?;

    let db = postgres_connection(&config.database).await?;
    let objects = TraceObjectService::new(build_object_storage(&config.object_storage).await?);
    let worker = TraceIndexWorker::new(db.clone(), objects);
    let snapshot = worker.rebuild_snapshot(&stream_id).await?;
    println!(
        "rebuild snapshot: stream={stream_id} index_version={index_version} accepted_seq={} indexed_seq={}",
        snapshot.accepted_seq, snapshot.indexed_seq
    );
    let accepted_seq = snapshot.accepted_seq;
    let indexed_seq = snapshot.indexed_seq;
    let result = worker
        .rebuild_index_snapshot(snapshot, index_version, |rebuilt_seq| {
            println!(
                "rebuild progress: stream={stream_id} index_version={index_version} accepted_seq={accepted_seq} indexed_seq={indexed_seq} rebuilt_seq={rebuilt_seq}"
            );
        })
        .await?;
    println!(
        "rebuild complete: stream={stream_id} index_version={index_version} accepted_seq={} indexed_seq={} rebuilt_seq={}",
        result.accepted_seq, result.indexed_seq, result.rebuilt_seq
    );
    db.close().await?;
    Ok(())
}

fn require_named_config(source: Option<ConfigSource>) -> Result<(), MegaError> {
    match source {
        Some(ConfigSource::Cli | ConfigSource::Env) => Ok(()),
        other => Err(rebuild_index_error(format!(
            "configuration was not named; pass --config <path> or set MEGA_CONFIG (source: {})",
            other.map_or("none", ConfigSource::as_str)
        ))),
    }
}

pub(crate) async fn ensure_schema_current(config: &DbConfig) -> Result<(), MegaError> {
    let connection = read_only_database_connection(config).await.map_err(|_| {
        rebuild_index_error("cannot open the configured database for a read-only schema check")
    })?;
    let pending = Migrator::get_pending_migrations_read_only(&connection).await;
    connection
        .close()
        .await
        .map_err(|_| rebuild_index_error("cannot close the schema-check connection"))?;
    match pending {
        Ok(pending) if pending.is_empty() => Ok(()),
        Ok(_) => Err(rebuild_index_error(
            "database schema is behind this build; this command never migrates",
        )),
        Err(_) => Err(rebuild_index_error(
            "cannot confirm the database schema matches this build",
        )),
    }
}

fn rebuild_index_error(message: impl std::fmt::Display) -> MegaError {
    MegaError::cli_exit(1, format!("traces rebuild-index: {message}"))
}
