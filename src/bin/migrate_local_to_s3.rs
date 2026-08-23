//! Offline object migration: LocalFileSystem → Amazon S3 (or S3-compatible).
//!
//! This binary scans a **local object storage directory** (the same layout used by Mega's
//! `ObjectNamespace::{Git,Lfs,Log,...}` sharding paths) and uploads all objects to an **S3
//! bucket** (real AWS S3 or an S3-compatible service such as MinIO/RustFS).
//!
//! It is intended for **one-time / offline** backfill or migration jobs. It is *not* a
//! continuous sync tool.
//!
//! ## What it does
//! - Lists all objects under the configured local root (optionally narrowed by `--prefix`).
//! - For each object:
//!   - Performs a target `HEAD` and **skips** the upload if the object already exists.
//!     A `HEAD` that is `NotFound` triggers an upload; any other `HEAD` error (auth,
//!     permission, network, service) fails the migration instead of being treated as
//!     "missing".
//!   - Otherwise reads the local object and uploads it (multipart streaming).
//! - Runs uploads concurrently (configurable) and retries transient upload failures with
//!   exponential backoff.
//! - Prints a structured summary (`migrated` / `skipped` / `failed` / `retried`).
//!
//! ## What it does NOT do
//! - It does **not delete** local objects after upload.
//! - It does **not overwrite** objects on the target (it skips when `HEAD` succeeds).
//! - It does **not** perform content checksum/ETag validation between source and target.
//!
//! ## Safety / idempotency
//! Re-running skips any objects that already exist on the target (based on `HEAD` success),
//! so it is safe to re-run. If you need strict "exists but different content" detection, add
//! a `head`-then-compare strategy before skipping. Failed multipart uploads are best-effort
//! aborted so they do not leave dangling uploads. `--dry-run` performs the `HEAD` planning
//! but writes nothing to the target.
//!
//! ## Configuration
//! Reads an Orbit-compatible TOML config and uses:
//! - `object_storage.local.root_dir` as the source directory
//! - `object_storage.s3.{region,bucket,access_key_id,secret_access_key[,endpoint_url]}` as the
//!   destination. `storage_type` selects `s3` (real AWS) or `s3compatible` (uses `endpoint_url`).
//!
//! Config path resolution order:
//! 1. CLI: `--config <path>`
//! 2. Environment: `ORBIT_CONFIG=<path>`
//! 3. Environment: `MEGA_CONFIG=<path>` (compatibility with monoengine deployments)
//! 4. `config.toml` or `config/config.toml` in the current directory
//!
//! ## Usage
//!
//! From the workspace root:
//!
//! ```bash
//! cargo run -p orbit --bin migrate_local_to_s3 -- --config ./config.toml
//! cargo run -p orbit --bin migrate_local_to_s3 -- --config ./config.toml --dry-run
//! cargo run -p orbit --bin migrate_local_to_s3 -- --config ./config.toml --prefix git/ --concurrency 32
//! ```
//!
//! `--concurrency` overrides the `MIGRATE_CONCURRENCY` env var (default: 16).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::{StreamExt, TryStreamExt};
use monoengine_core::orbit_bin_api::{
    IoOrbitError, ObjectStorageBackend, ObjectStorageConfig, OrbitResult, head_result_to_exists,
};
use object_store::{ObjectStore, ObjectStoreExt, aws::AmazonS3Builder, local::LocalFileSystem};
use serde::Deserialize;
use tokio::{sync::Semaphore, task::JoinSet};
use tracing::info;

#[derive(Debug, Deserialize)]
struct ConfigFile {
    object_storage: ObjectStorageConfig,
}

/// Parsed command-line arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct CliArgs {
    config: Option<PathBuf>,
    dry_run: bool,
    concurrency: Option<usize>,
    prefix: Option<String>,
}

/// Per-object migration outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Skipped,
    Migrated,
}

/// Options controlling a migration run.
#[derive(Debug, Clone)]
struct MigrateOptions {
    concurrency: usize,
    dry_run: bool,
    prefix: Option<object_store::path::Path>,
    max_retries: usize,
    base_retry_delay: Duration,
}

/// Structured counters reported at the end of a run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MigrationSummary {
    migrated: u64,
    skipped: u64,
    failed: u64,
    retried: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("migration failed: {e}");
        std::process::exit(1);
    }
}

async fn run() -> OrbitResult<()> {
    init_tracing();

    let args = parse_args(std::env::args().skip(1))?;
    let config_path = load_config_path(args.config.clone())?;
    let object_cfg = load_object_storage_config(&config_path)?;
    require_s3_target(object_cfg.storage_type)?;

    // Source: local filesystem.
    let src: Arc<dyn ObjectStore> = Arc::new(
        LocalFileSystem::new_with_prefix(&object_cfg.local.root_dir)
            .map_err(|e| IoOrbitError::Other(format!("failed to init LocalFileSystem: {e}")))?,
    );
    // Destination: S3 / S3-compatible.
    let dst: Arc<dyn ObjectStore> = Arc::new(build_s3(&object_cfg)?);

    // `--concurrency` overrides `MIGRATE_CONCURRENCY`; default 16.
    let concurrency = args
        .concurrency
        .or_else(|| {
            std::env::var("MIGRATE_CONCURRENCY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&v| v > 0)
        })
        .unwrap_or(16);

    let opts = MigrateOptions {
        concurrency,
        dry_run: args.dry_run,
        prefix: args.prefix.as_deref().map(object_store::path::Path::from),
        max_retries: 3,
        base_retry_delay: Duration::from_millis(100),
    };

    info!(
        "starting offline migration (dry_run={}, concurrency={}) from local {:?} to s3 bucket {:?}",
        opts.dry_run, opts.concurrency, object_cfg.local.root_dir, object_cfg.s3.bucket
    );

    // `migrate_all` logs the summary itself.
    let summary = migrate_all(src, dst, opts).await?;
    if summary.failed > 0 {
        return Err(IoOrbitError::Other(format!(
            "migration finished with {} failed object(s)",
            summary.failed
        )));
    }
    Ok(())
}

fn init_tracing() {
    // Simple stderr logger; we don't depend on global app logging here.
    // NOTE: only paths/buckets are logged, never credentials.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .try_init();
}

/// Parses CLI arguments. Unknown args are ignored with a warning.
fn parse_args(args: impl Iterator<Item = String>) -> OrbitResult<CliArgs> {
    let mut out = CliArgs::default();
    let mut it = args;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => out.config = Some(PathBuf::from(next_value(&mut it, "--config")?)),
            "--concurrency" => {
                let v = next_value(&mut it, "--concurrency")?;
                out.concurrency = Some(v.parse().map_err(|_| {
                    IoOrbitError::Other(format!("invalid --concurrency value: {v}"))
                })?);
            }
            "--prefix" => out.prefix = Some(next_value(&mut it, "--prefix")?),
            "--dry-run" => out.dry_run = true,
            other => eprintln!("warning: unknown argument `{other}` is ignored"),
        }
    }
    Ok(out)
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> OrbitResult<String> {
    it.next()
        .ok_or_else(|| IoOrbitError::Other(format!("{flag} requires a value")))
}

/// Pure config-path resolver (order: cli > ORBIT_CONFIG > MEGA_CONFIG > cwd candidates).
/// `exists` decides whether a cwd candidate is present (injected for testing).
fn resolve_config_path(
    cli: Option<PathBuf>,
    orbit_env: Option<PathBuf>,
    mega_env: Option<PathBuf>,
    cwd: &Path,
    exists: impl Fn(&Path) -> bool,
) -> OrbitResult<PathBuf> {
    if let Some(p) = cli {
        return Ok(p);
    }
    if let Some(p) = orbit_env {
        return Ok(p);
    }
    if let Some(p) = mega_env {
        return Ok(p);
    }
    for candidate in [cwd.join("config.toml"), cwd.join("config/config.toml")] {
        if exists(&candidate) {
            return Ok(candidate);
        }
    }
    Err(IoOrbitError::Other(
        "config path not found; pass --config <path> or set ORBIT_CONFIG".to_string(),
    ))
}

fn load_config_path(cli: Option<PathBuf>) -> OrbitResult<PathBuf> {
    let cwd = std::env::current_dir()?;
    resolve_config_path(
        cli,
        std::env::var_os("ORBIT_CONFIG").map(PathBuf::from),
        std::env::var_os("MEGA_CONFIG").map(PathBuf::from),
        &cwd,
        |p| p.exists(),
    )
}

fn load_object_storage_config(path: &Path) -> OrbitResult<ObjectStorageConfig> {
    let content = std::fs::read_to_string(path)?;
    let config: ConfigFile = toml::from_str(&content)?;
    Ok(config.object_storage)
}

/// This tool only migrates to S3 / S3-compatible targets.
fn require_s3_target(backend: ObjectStorageBackend) -> OrbitResult<()> {
    match backend {
        ObjectStorageBackend::S3 | ObjectStorageBackend::S3Compatible => Ok(()),
        other => Err(IoOrbitError::Other(format!(
            "migrate_local_to_s3 only supports S3/S3Compatible targets, got {other:?}"
        ))),
    }
}

/// Builds the destination S3 client. `S3Compatible` additionally sets the custom endpoint.
fn build_s3(cfg: &ObjectStorageConfig) -> OrbitResult<object_store::aws::AmazonS3> {
    let s3 = &cfg.s3;
    let mut builder = AmazonS3Builder::new()
        .with_region(&s3.region)
        .with_bucket_name(&s3.bucket)
        .with_access_key_id(&s3.access_key_id)
        .with_secret_access_key(&s3.secret_access_key);
    if matches!(cfg.storage_type, ObjectStorageBackend::S3Compatible) {
        builder = builder
            .with_endpoint(&s3.endpoint_url)
            .with_allow_http(true)
            .with_virtual_hosted_style_request(false);
    }
    builder
        .build()
        .map_err(|e| IoOrbitError::Other(format!("failed to init S3 client: {e}")))
}

/// Retries `op` on error up to `max_retries` times with exponential backoff.
/// Returns the final result and the number of retries actually performed.
async fn retry_with_backoff<T, F, Fut>(
    max_retries: usize,
    base_delay: Duration,
    mut op: F,
) -> (OrbitResult<T>, u64)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = OrbitResult<T>>,
{
    let mut retries: u64 = 0;
    loop {
        match op().await {
            Ok(v) => return (Ok(v), retries),
            Err(e) => {
                if retries >= max_retries as u64 {
                    return (Err(e), retries);
                }
                retries += 1;
                let factor = 1u32
                    .checked_shl((retries - 1).min(16) as u32)
                    .unwrap_or(u32::MAX);
                let delay = base_delay
                    .checked_mul(factor)
                    .unwrap_or(Duration::from_secs(30));
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

/// Uploads a single object from `src` to `dst` via multipart streaming.
/// On failure, best-effort aborts the multipart upload.
async fn upload_once(
    src: &Arc<dyn ObjectStore>,
    dst: &Arc<dyn ObjectStore>,
    path: &object_store::path::Path,
) -> OrbitResult<()> {
    let obj = src
        .get(path)
        .await
        .map_err(|e| IoOrbitError::Other(format!("failed to read local object {path}: {e}")))?;

    let mut upload = dst
        .put_multipart(path)
        .await
        .map_err(|e| IoOrbitError::Other(format!("failed to init multipart upload {path}: {e}")))?;

    let mut stream = obj.into_stream();
    let res: OrbitResult<()> = async {
        while let Some(chunk) = stream.try_next().await.map_err(|e| {
            IoOrbitError::Other(format!("failed to read local stream chunk {path}: {e}"))
        })? {
            upload.put_part(chunk.into()).await.map_err(|e| {
                IoOrbitError::Other(format!("failed to upload multipart part for {path}: {e}"))
            })?;
        }
        upload.complete().await.map_err(|e| {
            IoOrbitError::Other(format!("failed to complete multipart {path}: {e}"))
        })?;
        Ok(())
    }
    .await;

    if let Err(ref e) = res
        && let Err(abort_err) = upload.abort().await
    {
        return Err(IoOrbitError::Other(format!(
            "multipart upload failed for {path}: {e}; abort also failed: {abort_err}"
        )));
    }
    res
}

/// Migrates a single object: HEAD-skip, dry-run planning, or retried upload.
async fn migrate_one(
    src: &Arc<dyn ObjectStore>,
    dst: &Arc<dyn ObjectStore>,
    path: &object_store::path::Path,
    opts: &MigrateOptions,
) -> (OrbitResult<Outcome>, u64) {
    // HEAD success -> skip; NotFound -> upload; other error -> fail (see item 4 semantics).
    match head_result_to_exists(dst.head(path).await) {
        Ok(true) => {
            info!("skip existing object on target: {path}");
            return (Ok(Outcome::Skipped), 0);
        }
        Ok(false) => {}
        Err(e) => return (Err(e), 0),
    }

    if opts.dry_run {
        info!("[dry-run] would migrate object: {path}");
        return (Ok(Outcome::Migrated), 0);
    }

    info!("migrating object: {path}");
    let op = || {
        let src = src.clone();
        let dst = dst.clone();
        let path = path.clone();
        async move { upload_once(&src, &dst, &path).await }
    };
    let (res, retries) = retry_with_backoff(opts.max_retries, opts.base_retry_delay, op).await;
    // Retries are reported even when the upload ultimately fails.
    (res.map(|()| Outcome::Migrated), retries)
}

/// Folds a completed task's (result, retries) into the summary. Retries are always
/// counted — including for a failed object. Returns the object's error (after
/// counting it as failed) so the caller can fail fast.
fn record(summary: &mut MigrationSummary, joined: (OrbitResult<Outcome>, u64)) -> OrbitResult<()> {
    let (res, retries) = joined;
    summary.retried += retries;
    match res {
        Ok(Outcome::Skipped) => {
            summary.skipped += 1;
            Ok(())
        }
        Ok(Outcome::Migrated) => {
            summary.migrated += 1;
            Ok(())
        }
        Err(e) => {
            summary.failed += 1;
            Err(e)
        }
    }
}

fn log_summary(s: &MigrationSummary) {
    info!(
        "migration summary: migrated={} skipped={} failed={} retried={}",
        s.migrated, s.skipped, s.failed, s.retried
    );
}

/// Migrates every object from `src` to `dst`. Fails fast: the first object that
/// still errors after retries stops the run (its error is returned, and the
/// summary — including that failure — is logged).
async fn migrate_all(
    src: Arc<dyn ObjectStore>,
    dst: Arc<dyn ObjectStore>,
    opts: MigrateOptions,
) -> OrbitResult<MigrationSummary> {
    let opts = Arc::new(opts);
    let semaphore = Arc::new(Semaphore::new(opts.concurrency));
    let mut listed = src.list(opts.prefix.as_ref());
    let mut set: JoinSet<(OrbitResult<Outcome>, u64)> = JoinSet::new();
    let mut summary = MigrationSummary::default();
    // Bound the number of buffered handles; the Semaphore bounds true I/O concurrency.
    let max_buffered = opts.concurrency.saturating_mul(4).max(64);

    while let Some(entry) = listed.next().await {
        // Fail fast: reap any already-completed tasks before doing more work, so a
        // failed upload stops the run without waiting for the buffer to fill.
        while let Some(joined) = set.try_join_next() {
            drain(&mut summary, joined)?;
        }
        let meta =
            entry.map_err(|e| IoOrbitError::Other(format!("list local objects failed: {e}")))?;
        let path = meta.location.clone();
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| IoOrbitError::Other(format!("failed to acquire semaphore: {e}")))?;

        // A task may have completed (and possibly failed) while we waited for a
        // permit; reap again before spawning more work so a failure wins.
        while let Some(joined) = set.try_join_next() {
            drain(&mut summary, joined)?;
        }

        let src_t = src.clone();
        let dst_t = dst.clone();
        let opts_t = opts.clone();
        set.spawn(async move {
            let _permit = permit;
            migrate_one(&src_t, &dst_t, &path, &opts_t).await
        });

        // Drain completed tasks as they finish (join_next yields the first completed,
        // so a failure fails the run promptly) while keeping handles bounded.
        while set.len() >= max_buffered {
            if let Some(joined) = set.join_next().await {
                drain(&mut summary, joined)?;
            }
        }
    }

    while let Some(joined) = set.join_next().await {
        drain(&mut summary, joined)?;
    }

    log_summary(&summary);
    Ok(summary)
}

/// Records one completed task result into the summary, logging the summary and
/// propagating the error (fail fast) if the object failed.
fn drain(
    summary: &mut MigrationSummary,
    joined: Result<(OrbitResult<Outcome>, u64), tokio::task::JoinError>,
) -> OrbitResult<()> {
    let joined =
        joined.map_err(|e| IoOrbitError::Other(format!("migration task panicked: {e}")))?;
    if let Err(e) = record(summary, joined) {
        log_summary(summary);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[test]
    fn parse_args_parses_all_flags() {
        let args = parse_args(
            [
                "--config",
                "c.toml",
                "--dry-run",
                "--concurrency",
                "8",
                "--prefix",
                "git/",
            ]
            .into_iter()
            .map(String::from),
        )
        .unwrap();
        assert_eq!(args.config, Some(PathBuf::from("c.toml")));
        assert!(args.dry_run);
        assert_eq!(args.concurrency, Some(8));
        assert_eq!(args.prefix, Some("git/".to_string()));
    }

    #[test]
    fn parse_args_rejects_invalid_input() {
        assert!(
            parse_args(["--concurrency", "abc"].into_iter().map(String::from)).is_err(),
            "non-numeric concurrency must error"
        );
        assert!(
            parse_args(["--config"].into_iter().map(String::from)).is_err(),
            "missing flag value must error"
        );
    }

    #[test]
    fn resolve_config_path_respects_precedence() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve_config_path(
                Some(PathBuf::from("cli.toml")),
                Some(PathBuf::from("o")),
                Some(PathBuf::from("m")),
                cwd,
                |_| true
            )
            .unwrap(),
            PathBuf::from("cli.toml")
        );
        assert_eq!(
            resolve_config_path(None, Some(PathBuf::from("o.toml")), None, cwd, |_| true).unwrap(),
            PathBuf::from("o.toml")
        );
        assert_eq!(
            resolve_config_path(None, None, Some(PathBuf::from("m.toml")), cwd, |_| true).unwrap(),
            PathBuf::from("m.toml")
        );
        assert_eq!(
            resolve_config_path(None, None, None, cwd, |p| p
                == Path::new("/work/config.toml"))
            .unwrap(),
            PathBuf::from("/work/config.toml")
        );
        assert!(resolve_config_path(None, None, None, cwd, |_| false).is_err());
    }

    #[test]
    fn require_s3_target_only_allows_s3_backends() {
        assert!(require_s3_target(ObjectStorageBackend::S3).is_ok());
        assert!(require_s3_target(ObjectStorageBackend::S3Compatible).is_ok());
        assert!(require_s3_target(ObjectStorageBackend::Gcs).is_err());
        assert!(require_s3_target(ObjectStorageBackend::Local).is_err());
    }

    #[tokio::test]
    async fn retry_succeeds_after_transient_failures() {
        let attempts = Arc::new(AtomicU64::new(0));
        let a = attempts.clone();
        let op = move || {
            let a = a.clone();
            async move {
                let n = a.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(IoOrbitError::Other("transient".into()))
                } else {
                    Ok::<u32, IoOrbitError>(42)
                }
            }
        };
        let (res, retries) = retry_with_backoff(5, Duration::ZERO, op).await;
        assert_eq!(res.unwrap(), 42);
        assert_eq!(retries, 2);
    }

    #[tokio::test]
    async fn retry_gives_up_after_max_retries() {
        let op = || async { Err::<(), IoOrbitError>(IoOrbitError::Other("always".into())) };
        let (res, retries) = retry_with_backoff(3, Duration::ZERO, op).await;
        assert!(res.is_err());
        assert_eq!(retries, 3);
    }

    fn local(dir: &Path) -> Arc<dyn ObjectStore> {
        Arc::new(LocalFileSystem::new_with_prefix(dir).unwrap())
    }

    fn opts(dry_run: bool) -> MigrateOptions {
        MigrateOptions {
            concurrency: 2,
            dry_run,
            prefix: None,
            max_retries: 1,
            base_retry_delay: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn migrate_copies_then_skips_and_dry_run_writes_nothing() {
        let src_dir = tempfile::TempDir::new().unwrap();
        let dst_dir = tempfile::TempDir::new().unwrap();
        let src = local(src_dir.path());
        let dst = local(dst_dir.path());

        for name in ["a.txt", "b.txt"] {
            src.put(
                &object_store::path::Path::from(name),
                object_store::PutPayload::from_static(b"hello"),
            )
            .await
            .unwrap();
        }

        // dry-run: counts as migrated (planned) but writes nothing to the destination.
        let s = migrate_all(src.clone(), dst.clone(), opts(true))
            .await
            .unwrap();
        assert_eq!(s.migrated, 2);
        assert_eq!(s.skipped, 0);
        assert_eq!(s.failed, 0);
        assert_eq!(
            dst.list(None).count().await,
            0,
            "dry-run must not write to the destination"
        );

        // real run: migrates both.
        let s = migrate_all(src.clone(), dst.clone(), opts(false))
            .await
            .unwrap();
        assert_eq!(s.migrated, 2);
        assert_eq!(s.skipped, 0);
        assert_eq!(dst.list(None).count().await, 2);

        // rerun: both already exist -> skipped, none migrated.
        let s = migrate_all(src.clone(), dst.clone(), opts(false))
            .await
            .unwrap();
        assert_eq!(s.migrated, 0);
        assert_eq!(s.skipped, 2);
    }
}
