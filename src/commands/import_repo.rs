//! `mega2 import-repo remove` (plan-20260923 ADR-FU-11, FU-21 addendum): the
//! server-side operator entry for ImportRepo cleanup. It assembles storage
//! from the named config alone, never touching the key file under the base
//! dir, and drives the FU-17 cleanup entry in-process.

use std::{io::Write, sync::Arc, time::Duration};

use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use sea_orm::{ConnectionTrait, DatabaseConnection, TransactionTrait};
use sea_orm_migration::MigratorTrait;
use tokio::sync::watch;

use crate::{
    ceres::{
        api_service::cache::GitObjectCache,
        pack::{
            import_repo::{RemoveOutcome, remove_import_repo},
            path_policy::strict_import_repo_leaf_input,
        },
    },
    commands::{CommandContext, require_config, unknown_subcommand},
    common::errors::{MegaError, MegaResult},
    config::{Config, DbConfig, loader::ConfigSource, secret::is_secret_ref_value},
    context::object_storage_needs_vault,
    jupiter::{
        migration::Migrator,
        redis::init_connection,
        storage::{
            Storage,
            base_storage::StorageConnector,
            init::{postgres_connection, read_only_database_connection},
            object_storage::build_object_storage,
            push_queue_storage::MONO_WRITE_LOCK_SQL,
        },
    },
};

const OPERATOR_REQUESTER: &str = "operator-cli";
const DEFAULT_MAX_ROUNDS: u32 = 1000;
const DEFAULT_CACHE_PREFIX: &str = "git-object-rkyv:v1";
const EXIT_PENDING: i32 = 3;
const EXIT_INTERRUPTED: i32 = 130;
const HANDLE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

pub fn cli() -> Command {
    Command::new("import-repo")
        .about("Server-side ImportRepo operations")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("remove")
                .about(
                    "Remove the ImportRepo at --path (detach it, sweep its rows) and drain the \
                     path's pending cleanups",
                )
                .arg(
                    Arg::new("path")
                        .long("path")
                        .value_name("PATH")
                        .required(true)
                        .help("Registered ImportRepo path, canonical and below import_dir"),
                )
                .arg(
                    Arg::new("cleanup-id")
                        .long("cleanup-id")
                        .value_name("ID")
                        .value_parser(value_parser!(i64).range(1..))
                        .help("Continue this cleanup of --path instead of starting a new one"),
                )
                .arg(
                    Arg::new("max-rounds")
                        .long("max-rounds")
                        .value_name("N")
                        .value_parser(value_parser!(u32).range(1..))
                        .default_value("1000")
                        .help("Cleanup rounds to run before stopping with exit code 3"),
                )
                .arg(
                    Arg::new("yes")
                        .long("yes")
                        .action(ArgAction::SetTrue)
                        .required(true)
                        .help("Confirm the removal"),
                ),
        )
}

#[tokio::main(flavor = "current_thread")]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("remove", sub)) => remove(ctx, sub).await,
        Some((cmd, _)) => Err(unknown_subcommand(cmd)),
        None => Err(unknown_subcommand("import-repo")),
    }
}

async fn remove(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let source = ctx.config_summary.as_ref().map(|summary| summary.source);
    let config = require_config(ctx, "import-repo")?;
    require_named_config(source)?;
    let raw = args
        .get_one::<String>("path")
        .ok_or_else(|| MegaError::Other("--path is required".to_owned()))?;
    let path = preflight(&config, raw)?;
    let max_rounds = args
        .get_one::<u32>("max-rounds")
        .copied()
        .unwrap_or(DEFAULT_MAX_ROUNDS);
    let start = args
        .get_one::<i64>("cleanup-id")
        .copied()
        .map_or(Start::Path, Start::Cleanup);
    let db_config = config.database.clone();
    let mut sigint = crate::cli::service_sigint_receiver();

    let op = tokio::select! {
        biased;
        () = sigint_recorded(&mut sigint) => {
            return emit(&path, Finish::Interrupted(Resume::Nothing));
        }
        assembled = assemble(config) => assembled?,
    };

    let mut progress = Progress::new(start);
    let driven = tokio::select! {
        biased;
        driven = drive(&op, &path, max_rounds, &mut progress) => Some(driven),
        () = sigint_recorded(&mut sigint) => None,
    };
    let finish = match driven {
        None => Finish::Interrupted(resume_handle(&op, &db_config, &path, &progress).await),
        Some(Ok(finish)) => finish,
        Some(Err(error @ MegaError::ImportRepo(_))) => return Err(error),
        Some(Err(error)) => match resume_handle(&op, &db_config, &path, &progress).await {
            Resume::Known(repo_id, cleanup_id) => Finish::Pending {
                repo_id,
                cleanup_id,
                cause: PendingCause::Failed(error.to_string()),
            },
            Resume::Nothing => return Err(error),
            Resume::Unknown => return Err(ledger_unreadable(&error)),
        },
    };
    emit(&path, finish)
}

/// A failure after which the ledger could not be read: whether the ImportRepo
/// was detached is unknown.
fn ledger_unreadable(error: &MegaError) -> MegaError {
    MegaError::cli_exit(
        1,
        format!(
            "{error}\nimport-repo remove: the cleanup ledger could not be read; check \
             import_repo_cleanups for this path before rerunning"
        ),
    )
}

/// A destructive run must name its config: a `./config/config.toml` found in
/// the working directory, or the global default, is refused.
fn require_named_config(source: Option<ConfigSource>) -> Result<(), MegaError> {
    match source {
        Some(ConfigSource::Cli | ConfigSource::Env) => Ok(()),
        other => Err(MegaError::cli_exit(
            1,
            format!(
                "import-repo remove: refusing a config that was not named (source: {}); pass \
                 --config <path> or set MEGA_CONFIG",
                other.map_or("none", ConfigSource::as_str)
            ),
        )),
    }
}

/// Every pure check, before any I/O: config validity, the storage-only
/// morphology, literal credentials, then the strict ImportRepo path.
fn preflight(config: &Config, raw: &str) -> Result<String, MegaError> {
    config.validate()?;
    config.monorepo.ensure_normal_service_object_format()?;
    if !config.git.storage_only() {
        return Err(MegaError::cli_exit(
            1,
            "import-repo remove: ImportRepo cleanup needs a storage-only deployment \
             (git.push_auth = \"token\" or \"none\")",
        ));
    }
    if is_secret_ref_value(config.redis.url.trim_start()) {
        return Err(MegaError::cli_exit(
            1,
            "import-repo remove: redis.url is a vault:// SecretRef and this command never \
             opens the vault; supply a literal value for this run (MEGA_REDIS__URL)",
        ));
    }
    if object_storage_needs_vault(&config.object_storage) {
        return Err(MegaError::cli_exit(
            1,
            "import-repo remove: object_storage.s3 credentials are vault:// SecretRefs and \
             this command never opens the vault; supply literal values for this run \
             (MEGA_OBJECT_STORAGE__S3__ACCESS_KEY_ID, MEGA_OBJECT_STORAGE__S3__SECRET_ACCESS_KEY)",
        ));
    }
    Ok(strict_import_repo_leaf_input(&config.monorepo, raw)?)
}

/// The command never migrates: a read-only connection confirms that the
/// database schema is exactly the one this build expects.
async fn ensure_schema_current(db: &DbConfig) -> Result<(), MegaError> {
    let read_only = read_only_database_connection(db).await?;
    let pending = Migrator::get_pending_migrations_read_only(&read_only).await;
    let _ = read_only.close().await;
    match pending {
        Ok(pending) if pending.is_empty() => Ok(()),
        Ok(pending) => Err(MegaError::cli_exit(
            1,
            format!(
                "import-repo remove: the database schema is behind this mega2 build ({} pending \
                 migrations); this command never migrates — run it with the service's build",
                pending.len()
            ),
        )),
        Err(error) => Err(MegaError::cli_exit(
            1,
            format!(
                "import-repo remove: cannot confirm the database schema matches this mega2 \
                 build: {error}"
            ),
        )),
    }
}

struct Operator {
    storage: Storage,
    db: Arc<DatabaseConnection>,
    cache: Arc<GitObjectCache>,
}

/// Storage for the cleanup entry: schema check, Redis (snowflake worker
/// lease, kept by its refresh task), database, object store. No vault, no
/// migration, no seed row; the only background tasks are the lease refresh
/// and the connection pools' own maintenance.
async fn assemble(config: Config) -> Result<Operator, MegaError> {
    ensure_schema_current(&config.database).await?;
    let connection = init_connection(&config.redis).await?;
    let db = Arc::new(postgres_connection(&config.database).await?);
    let object_store = build_object_storage(&config.object_storage).await?;
    let storage = Storage::new_with_connection(Arc::new(config), db.clone(), object_store).await?;
    let prefix = std::env::var("MEGA_GIT_OBJECT_CACHE_PREFIX")
        .unwrap_or_else(|_| DEFAULT_CACHE_PREFIX.to_owned());
    Ok(Operator {
        storage,
        db,
        cache: Arc::new(GitObjectCache { connection, prefix }),
    })
}

enum Start {
    Path,
    Cleanup(i64),
}

struct Progress {
    start: Start,
    rounds: u32,
    live_before: Option<i64>,
    handle: Option<(i64, i64)>,
}

impl Progress {
    fn new(start: Start) -> Self {
        Self {
            start,
            rounds: 0,
            live_before: None,
            handle: None,
        }
    }
}

enum PendingCause {
    Rounds(u32),
    Failed(String),
}

enum Finish {
    Removed {
        repo_id: i64,
        cleanup_id: i64,
    },
    Absent,
    Pending {
        repo_id: i64,
        cleanup_id: i64,
        cause: PendingCause,
    },
    Interrupted(Resume),
}

enum Resume {
    Known(i64, i64),
    Nothing,
    Unknown,
}

/// Round 1 is a new operation or the named continuation; every later round
/// continues a ledger row, so one run detaches at most once. After the
/// primary chain the path's other `detached` rows are drained from the
/// ledger, again without detaching.
async fn drive(
    op: &Operator,
    path: &str,
    max_rounds: u32,
    progress: &mut Progress,
) -> Result<Finish, MegaError> {
    let git_db = op.storage.git_db_storage();
    let conn = git_db.get_connection();
    let first = match progress.start {
        Start::Path => {
            progress.live_before = git_db
                .find_git_repo_exact_match(path)
                .await?
                .map(|repo| repo.id);
            None
        }
        Start::Cleanup(id) => {
            if let Some(row) = git_db.cleanup_by_id(id, conn).await?
                && row.path == path
            {
                progress.handle = Some((row.repo_id, row.id));
            }
            Some(id)
        }
    };

    let mut outcome = round(op, path, first, progress).await?;
    let primary = loop {
        match outcome {
            RemoveOutcome::Pending {
                repo_id,
                cleanup_id,
            } => {
                progress.handle = Some((repo_id, cleanup_id));
                if progress.rounds >= max_rounds {
                    return Ok(Finish::Pending {
                        repo_id,
                        cleanup_id,
                        cause: PendingCause::Rounds(progress.rounds),
                    });
                }
                outcome = round(op, path, Some(cleanup_id), progress).await?;
            }
            RemoveOutcome::Removed {
                repo_id,
                cleanup_id,
            } => {
                progress.handle = Some((repo_id, cleanup_id));
                break Finish::Removed {
                    repo_id,
                    cleanup_id,
                };
            }
            RemoveOutcome::Absent => break Finish::Absent,
        }
    };

    while let Some(next) = git_db
        .pending_cleanups_by_path(path, conn)
        .await?
        .into_iter()
        .next()
    {
        progress.handle = Some((next.repo_id, next.id));
        if progress.rounds >= max_rounds {
            return Ok(Finish::Pending {
                repo_id: next.repo_id,
                cleanup_id: next.id,
                cause: PendingCause::Rounds(progress.rounds),
            });
        }
        round(op, path, Some(next.id), progress).await?;
        tracing::info!(path = ?path, cleanup_id = next.id, "import-repo remove: drained a pending cleanup of the path");
    }
    Ok(primary)
}

async fn round(
    op: &Operator,
    path: &str,
    continuation: Option<i64>,
    progress: &mut Progress,
) -> Result<RemoveOutcome, MegaError> {
    progress.rounds += 1;
    let outcome = remove_import_repo(
        &op.storage,
        op.cache.clone(),
        path,
        Some(OPERATOR_REQUESTER.to_owned()),
        continuation,
    )
    .await?;
    tracing::info!(path = ?path, round = progress.rounds, ?continuation, ?outcome, "import-repo remove round");
    Ok(outcome)
}

/// The cleanup a stopped run leaves behind, if any. When no answer named one
/// yet, the operator pool is closed first, so an interrupted round's
/// connection is closed instead of reused and its open transaction can only
/// roll back; then the write lock is taken on a read-only transaction, so a
/// detach queued on or holding that lock has committed or rolled back before
/// the ledger is read. A round blocked on another lock can outlast the lookup
/// timeout (`Unknown`). `live_before` is read before round 1: a repository
/// replaced in between is answered with the replaced one's cleanup.
async fn resume_handle(op: &Operator, db: &DbConfig, path: &str, progress: &Progress) -> Resume {
    if let Some((repo_id, cleanup_id)) = progress.handle {
        return Resume::Known(repo_id, cleanup_id);
    }
    if progress.rounds == 0 || matches!(progress.start, Start::Cleanup(_)) {
        return Resume::Nothing;
    }
    match tokio::time::timeout(
        HANDLE_LOOKUP_TIMEOUT,
        ledger_after_barrier(op, db, path, progress.live_before),
    )
    .await
    {
        Ok(Ok(Some((repo_id, cleanup_id)))) => Resume::Known(repo_id, cleanup_id),
        Ok(Ok(None)) => Resume::Nothing,
        Ok(Err(error)) => {
            tracing::warn!(%error, "import-repo remove: reading the cleanup ledger failed");
            Resume::Unknown
        }
        Err(_) => Resume::Unknown,
    }
}

async fn ledger_after_barrier(
    op: &Operator,
    db: &DbConfig,
    path: &str,
    live_before: Option<i64>,
) -> Result<Option<(i64, i64)>, MegaError> {
    op.db.close_by_ref().await?;
    let read_only = read_only_database_connection(db).await?;
    let txn = read_only.begin().await?;
    txn.execute_unprepared(MONO_WRITE_LOCK_SQL).await?;
    let git_db = op.storage.git_db_storage();
    let row = match live_before {
        Some(repo_id) => git_db.latest_cleanup_for(repo_id, path, &txn).await?,
        None => git_db
            .pending_cleanups_by_path(path, &txn)
            .await?
            .into_iter()
            .next(),
    };
    let _ = txn.rollback().await;
    let _ = read_only.close().await;
    Ok(row.map(|row| (row.repo_id, row.id)))
}

/// The stdout line and the result (exit code) of a finished run.
fn report(path: &str, finish: Finish) -> (String, MegaResult) {
    let p = escape_controls(path);
    match finish {
        Finish::Removed {
            repo_id,
            cleanup_id,
        } => (
            format!("removed {p} (repo_id={repo_id}, cleanup_id={cleanup_id})"),
            Ok(()),
        ),
        Finish::Absent => (format!("absent {p}"), Ok(())),
        Finish::Pending {
            repo_id,
            cleanup_id,
            cause,
        } => {
            let message = match cause {
                PendingCause::Rounds(rounds) => format!(
                    "import-repo remove: cleanup {cleanup_id} is not finished after {rounds} \
                     rounds; resume with --cleanup-id {cleanup_id}"
                ),
                PendingCause::Failed(error) => format!(
                    "{error}\nimport-repo remove: the path's cleanups stopped before they \
                     finished; resume with --cleanup-id {cleanup_id}"
                ),
            };
            (
                format!("pending {p} (repo_id={repo_id}, cleanup_id={cleanup_id})"),
                Err(MegaError::cli_exit(EXIT_PENDING, message)),
            )
        }
        Finish::Interrupted(Resume::Known(repo_id, cleanup_id)) => (
            format!("interrupted {p} (repo_id={repo_id}, cleanup_id={cleanup_id})"),
            Err(MegaError::cli_exit(
                EXIT_INTERRUPTED,
                format!("import-repo remove: interrupted; resume with --cleanup-id {cleanup_id}"),
            )),
        ),
        Finish::Interrupted(Resume::Nothing) => (
            format!("interrupted {p}"),
            Err(MegaError::cli_exit(
                EXIT_INTERRUPTED,
                "import-repo remove: interrupted before a cleanup was recorded; the ImportRepo \
                 is unchanged, rerun the same command",
            )),
        ),
        Finish::Interrupted(Resume::Unknown) => (
            format!("interrupted {p}"),
            Err(MegaError::cli_exit(
                EXIT_INTERRUPTED,
                "import-repo remove: interrupted; the cleanup ledger could not be read, so \
                 whether the ImportRepo was detached is unknown; check import_repo_cleanups for \
                 this path before rerunning",
            )),
        ),
    }
}

/// Write the outcome as the last stdout line. A write failure keeps a
/// non-zero outcome's exit code and turns a success into an I/O error.
fn emit(path: &str, finish: Finish) -> MegaResult {
    let (line, result) = report(path, finish);
    let mut stdout = std::io::stdout().lock();
    let written = writeln!(stdout, "{line}").and_then(|()| stdout.flush());
    match (written, result) {
        (Ok(()), result) => result,
        (Err(error), Ok(())) => Err(MegaError::Io(error)),
        (Err(error), Err(MegaError::CliExit { code, message })) => {
            Err(MegaError::cli_exit(code, format!("{message}\n{error}")))
        }
        (Err(_), Err(other)) => Err(other),
    }
}

/// Control characters of a path, escaped for a one-line stdout outcome.
fn escape_controls(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().collect()
            } else {
                c.to_string()
            }
        })
        .collect()
}

async fn sigint_recorded(rx: &mut Option<watch::Receiver<bool>>) {
    match rx {
        Some(rx) => {
            if rx.wait_for(|recorded| *recorded).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::PushAuth, orbit_api::factory::ObjectStorageBackend};

    #[test]
    fn import_repo_remove_contract() {
        let path = "/third-party/a";
        for (finish, line, code) in [
            (
                Finish::Removed {
                    repo_id: 9_007_199_254_740_993,
                    cleanup_id: 7,
                },
                "removed /third-party/a (repo_id=9007199254740993, cleanup_id=7)",
                0,
            ),
            (Finish::Absent, "absent /third-party/a", 0),
            (
                Finish::Pending {
                    repo_id: 5,
                    cleanup_id: 7,
                    cause: PendingCause::Rounds(1),
                },
                "pending /third-party/a (repo_id=5, cleanup_id=7)",
                3,
            ),
            (
                Finish::Pending {
                    repo_id: 5,
                    cleanup_id: 7,
                    cause: PendingCause::Failed("Database error: x".to_owned()),
                },
                "pending /third-party/a (repo_id=5, cleanup_id=7)",
                3,
            ),
            (
                Finish::Interrupted(Resume::Known(5, 7)),
                "interrupted /third-party/a (repo_id=5, cleanup_id=7)",
                130,
            ),
            (
                Finish::Interrupted(Resume::Nothing),
                "interrupted /third-party/a",
                130,
            ),
            (
                Finish::Interrupted(Resume::Unknown),
                "interrupted /third-party/a",
                130,
            ),
        ] {
            let resumable = matches!(
                finish,
                Finish::Pending { .. } | Finish::Interrupted(Resume::Known(..))
            );
            let unknown = matches!(finish, Finish::Interrupted(Resume::Unknown));
            let (got, result) = report(path, finish);
            if unknown {
                let message = result.as_ref().unwrap_err().to_string();
                assert!(message.contains("check import_repo_cleanups"), "{message}");
                assert!(!message.contains("rerun the same command"), "{message}");
            }
            assert_eq!(got, line);
            let exit = result
                .as_ref()
                .err()
                .map_or(0, MegaError::process_exit_code);
            assert_eq!(exit, code, "{line}");
            if resumable {
                assert!(
                    result.unwrap_err().to_string().contains("--cleanup-id 7"),
                    "{line}"
                );
            }
        }

        let failed = ledger_unreadable(&MegaError::Other("x".to_owned()));
        assert_eq!(failed.process_exit_code(), 1);
        let message = failed.to_string();
        assert!(message.contains("check import_repo_cleanups"), "{message}");
        assert!(!message.contains("rerun the same command"), "{message}");

        assert_eq!(
            escape_controls("/third-party/a\tb\u{7f}\u{1b}[2J\u{85}"),
            "/third-party/a\\tb\\u{7f}\\u{1b}[2J\\u{85}"
        );
        assert_eq!(
            escape_controls("/third-party/café o'neil"),
            "/third-party/café o'neil"
        );

        for source in [ConfigSource::Cli, ConfigSource::Env] {
            require_named_config(Some(source)).unwrap();
        }
        for source in [
            Some(ConfigSource::Cwd),
            Some(ConfigSource::Global),
            Some(ConfigSource::DefaultGenerated),
            None,
        ] {
            assert_eq!(
                require_named_config(source)
                    .unwrap_err()
                    .process_exit_code(),
                1
            );
        }

        let storage_only = || {
            let mut config = Config::mock();
            config.git.push_auth = Some(PushAuth::None);
            config.git.ssh_receive_pack = Some(false);
            config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
            config
        };
        let mut review = storage_only();
        review.git.push_auth = None;
        review.monorepo.push_policy = crate::config::PushPolicy::Review;
        let refused = preflight(&review, "/third-party/a")
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("needs a storage-only deployment"),
            "{refused}"
        );

        let mut redis_ref = storage_only();
        redis_ref.redis.url = "vault://secret/config/it/redis/url#value".to_owned();
        let refused = preflight(&redis_ref, "/third-party/a")
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("redis.url is a vault:// SecretRef"),
            "{refused}"
        );
        assert!(!refused.contains("secret/config"), "{refused}");

        let s3_ref = "vault://secret/config/it/object_storage/access_key_id#value";
        let mut s3 = storage_only();
        s3.object_storage.storage_type = ObjectStorageBackend::S3Compatible;
        s3.object_storage.s3.access_key_id = s3_ref.to_owned();
        s3.object_storage.s3.region = "us-east-1".to_owned();
        s3.object_storage.s3.bucket = "fu21".to_owned();
        s3.object_storage.s3.secret_access_key = "fu21-literal".to_owned();
        s3.object_storage.s3.endpoint_url = "http://127.0.0.1:1".to_owned();
        let refused = preflight(&s3, "/third-party/a").unwrap_err().to_string();
        assert!(
            refused.contains("object_storage.s3 credentials"),
            "{refused}"
        );

        let mut local = storage_only();
        local.object_storage.storage_type = ObjectStorageBackend::Local;
        local.object_storage.s3.access_key_id = s3_ref.to_owned();
        let refused = preflight(&local, "/third-party").unwrap_err().to_string();
        assert!(
            refused.starts_with("IMPORT_REPO_PATH_INVALID: "),
            "{refused}"
        );
        assert_eq!(
            preflight(&storage_only(), "/third-party/a").unwrap(),
            "/third-party/a"
        );
    }
}
