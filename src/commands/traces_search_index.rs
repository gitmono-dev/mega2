//! `mega2 traces search-index`: operator inspect and controlled repair for the
//! canonical trace fragment GIN index.
//!
//! The driver opens a maintenance connection directly from
//! `MEGA2_TRACES_PG_ADMIN_URL`. It never constructs a `Context`/`Storage`, so
//! ordinary migrations and legacy capture maintenance do not run.

use std::{io::Write, path::Path, time::Duration};

use clap::{Arg, ArgMatches, Command, ValueEnum};

use crate::{
    commands::CommandContext,
    common::errors::{MegaError, MegaResult},
    config::DbConfig,
    jupiter::{
        service::trace_search_index_service::{IndexState, run_operator_maintenance},
        storage::{
            init::postgres_connection,
            trace_search_index_storage::{
                CatalogValidation, RepairDisposition, TraceGinReceipt, assess_recovery,
                validate_catalog,
            },
        },
    },
};

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
enum Action {
    Inspect,
    Repair,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Repair => "repair",
        }
    }
}

pub fn cli() -> Command {
    Command::new("search-index")
        .about("Inspect or repair the canonical trace fragment GIN index")
        .arg(
            Arg::new("action")
                .long("action")
                .value_name("ACTION")
                .value_parser(["inspect", "repair"])
                .required(true)
                .help("inspect writes a descriptor receipt; repair builds or rebuilds it"),
        )
        .arg(
            Arg::new("descriptor-file")
                .long("descriptor-file")
                .value_name("PATH")
                .required(true)
                .help("Descriptor receipt written atomically with mode 0600"),
        )
        .arg(
            Arg::new("execution-timeout")
                .long("execution-timeout")
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(u64).range(60..=7200))
                .default_value("1800")
                .help("Per-statement execution budget in seconds (60..7200)"),
        )
}

pub async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let action = match args.get_one::<String>("action").map(String::as_str) {
        Some("inspect") => Action::Inspect,
        Some("repair") => Action::Repair,
        Some(other) => {
            return Err(MegaError::cli_exit(
                1,
                format!("traces search-index: unsupported action {other}"),
            ));
        }
        None => {
            return Err(MegaError::cli_exit(
                1,
                "traces search-index: --action is required",
            ));
        }
    };
    let descriptor = args.get_one::<String>("descriptor-file").ok_or_else(|| {
        MegaError::cli_exit(1, "traces search-index: --descriptor-file is required")
    })?;
    let seconds = *args.get_one::<u64>("execution-timeout").unwrap_or(&1800);
    if !(60..=7200).contains(&seconds) {
        return Err(MegaError::cli_exit(
            1,
            "traces search-index: --execution-timeout must be between 60 and 7200",
        ));
    }
    let _ = ctx;
    let admin_url = std::env::var("MEGA2_TRACES_PG_ADMIN_URL").map_err(|_| {
        MegaError::cli_exit(
            1,
            "traces search-index: MEGA2_TRACES_PG_ADMIN_URL is required",
        )
    })?;
    let config = DbConfig {
        db_type: "postgres".to_owned(),
        db_url: admin_url,
        max_connection: 2,
        min_connection: 1,
        acquire_timeout: 30,
        connect_timeout: 30,
        sqlx_logging: false,
    };
    let db = postgres_connection(&config).await?;
    let assessment = assess_recovery(&db).await?;
    if let RepairDisposition::Rejected(reason) = &assessment.disposition {
        return Err(MegaError::cli_exit(
            1,
            format!("traces search-index: repair refused: {reason}"),
        ));
    }
    match action {
        Action::Inspect => {
            write_descriptor(Path::new(descriptor), &assessment.receipt)?;
            println!(
                "inspect ok: index_version={} index_oid={}",
                assessment.receipt.index_version,
                assessment
                    .receipt
                    .index_oid
                    .map_or_else(|| "null".to_owned(), |oid| oid.to_string())
            );
        }
        Action::Repair => {
            let state = match assessment.disposition {
                RepairDisposition::AlreadyReady => IndexState::Ready,
                RepairDisposition::BuildOnly => {
                    run_operator_maintenance(&db, Duration::from_secs(seconds), false).await?
                }
                RepairDisposition::DropAndBuild { .. } => {
                    run_operator_maintenance(&db, Duration::from_secs(seconds), true).await?
                }
                RepairDisposition::Rejected(reason) => {
                    return Err(MegaError::cli_exit(1, reason));
                }
            };
            if state != IndexState::Ready {
                return Err(MegaError::cli_exit(
                    1,
                    format!("traces search-index: repair did not reach Ready: {state:?}"),
                ));
            }
            // Re-read the catalog so the receipt carries the new index OID.
            if !matches!(validate_catalog(&db).await?, CatalogValidation::Ready(_)) {
                return Err(MegaError::cli_exit(
                    1,
                    "traces search-index: rebuilt catalog is not Valid",
                ));
            }
            let refreshed = assess_recovery(&db).await?;
            write_descriptor(Path::new(descriptor), &refreshed.receipt)?;
            println!(
                "repair ok: index_version={} index_oid={}",
                refreshed.receipt.index_version,
                refreshed
                    .receipt
                    .index_oid
                    .map_or_else(|| "null".to_owned(), |oid| oid.to_string())
            );
        }
    }
    Ok(())
}

/// Atomically replace the descriptor receipt with mode 0600.
fn write_descriptor(path: &Path, receipt: &TraceGinReceipt) -> MegaResult {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temporary = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("descriptor")
    ));
    let payload = serde_json::to_vec_pretty(receipt)
        .map_err(|error| MegaError::Other(format!("serialize descriptor: {error}")))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| MegaError::Other(format!("open descriptor temp file: {error}")))?;
    file.write_all(&payload)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| MegaError::Other(format!("write descriptor: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| MegaError::Other(format!("set descriptor mode: {error}")))?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| MegaError::Other(format!("replace descriptor: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gin_recovery_cli_args_default_and_bounds() {
        let matches = cli()
            .try_get_matches_from([
                "search-index",
                "--action",
                "inspect",
                "--descriptor-file",
                "/tmp/descriptor.json",
            ])
            .unwrap();
        assert_eq!(matches.get_one::<u64>("execution-timeout"), Some(&1800));
        for value in ["59", "7201", "0", "-1"] {
            assert!(
                cli()
                    .try_get_matches_from([
                        "search-index",
                        "--action",
                        "repair",
                        "--descriptor-file",
                        "/tmp/descriptor.json",
                        "--execution-timeout",
                        value,
                    ])
                    .is_err(),
                "timeout {value} must be rejected"
            );
        }
        for value in ["60", "7200"] {
            assert!(
                cli()
                    .try_get_matches_from([
                        "search-index",
                        "--action",
                        "repair",
                        "--descriptor-file",
                        "/tmp/descriptor.json",
                        "--execution-timeout",
                        value,
                    ])
                    .is_ok()
            );
        }
        assert!(
            cli()
                .try_get_matches_from([
                    "search-index",
                    "--action",
                    "drop",
                    "--descriptor-file",
                    "/tmp/x"
                ])
                .is_err()
        );
    }
}
