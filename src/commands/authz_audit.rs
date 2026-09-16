//! UN-29: `authz-audit` CLI skeleton — audit modes + fsync tool mode.
//! UN-37: `authz-audit promote` mode (pure file; no config / DB).
//!
//! Audit core is UN-26; readonly context is UN-30; restricted writes are
//! UN-32/UN-59; promotion state machine is UN-35/UN-40.

use std::{
    fs::OpenOptions,
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::SystemTime,
};

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde::Serialize;

use crate::{
    commands::{CommandContext, LoadMode, require_config},
    common::errors::{MegaError, MegaResult},
    context::ReadOnlyContext,
    contract::policy::{
        authz_audit::{
            BaselineArtifact, DiffVerdict, SanitizedReport, bootstrap_candidate, compare,
        },
        baseline_pointer::{read_current_pointer, read_version_for_digest},
        baseline_promotion::{PromoteRequest, promote, resolve_promote_fence},
        secure_artifact::{ArtifactError, CandidateReference, RestrictedRoot, read_candidate},
        secure_hardcap::ReservedRun,
        secure_lifecycle::NoLeases,
        secure_producer::Producer,
        secure_sweep::{MaintenanceLock, NoReservations, sweep},
    },
};

/// Frozen CLI exit codes (UN-29 unique freeze point).
pub const EXIT_OK: i32 = 0;
pub const EXIT_AUDIT_DIFF: i32 = 2;
pub const EXIT_FENCING: i32 = 3;
pub(crate) const EXIT_PARAM: i32 = 4;

#[derive(Debug, Serialize)]
struct CliSanitizedReport {
    digest: String,
    closure_count: usize,
    diff_verdict: DiffVerdict,
    source_summary: crate::commands::SanitizedSourceSummary,
}

pub fn cli() -> Command {
    Command::new("authz-audit")
        .about("Authorize ACL audit against an approved baseline (readonly)")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(bootstrap_cli())
        .subcommand(compare_cli())
        .subcommand(promote_cli())
        .subcommand(crate::commands::authz_audit_run::run_init_cli())
        .subcommand(crate::commands::authz_audit_run::run_commit_cli())
        .subcommand(crate::commands::authz_audit_run::run_abort_cli())
        .subcommand(crate::commands::authz_audit_protect::protect_cli())
        .subcommand(crate::commands::authz_audit_protect::unprotect_cli())
        .subcommand(crate::commands::authz_audit_evidence::evidence_append_cli())
        .subcommand(fsync_cli())
}

fn bootstrap_cli() -> Command {
    Command::new("bootstrap-candidate")
        .about("Produce a candidate baseline artifact (verdict always not_compared)")
        .arg(restricted_root_arg())
        .arg(out_arg())
        .arg(restricted_out_arg())
}

fn compare_cli() -> Command {
    Command::new("compare")
        .about("Compare the live ACL against the current baseline pointer")
        .arg(restricted_root_arg())
        .arg(
            Arg::new("expect-digest")
                .long("expect-digest")
                .required(true)
                .value_name("DIGEST")
                .help("Approved digest from the ledger (sha256:<64hex>)"),
        )
        .arg(out_arg())
        .arg(restricted_out_arg())
}

fn promote_cli() -> Command {
    Command::new("promote")
        .about("Promote a candidate baseline under CAS fencing (no config / DB)")
        .arg(restricted_root_arg())
        .arg(
            Arg::new("candidate")
                .long("candidate")
                .required(true)
                .value_name("RUN_ID/FILE")
                .help("Root-relative candidate: <run-id>/<bare-file-name>"),
        )
        .arg(
            Arg::new("expect-digest")
                .long("expect-digest")
                .required(true)
                .value_name("DIGEST")
                .help("Content digest of the candidate bytes (sha256:<64hex>)"),
        )
        .arg(
            Arg::new("expect-no-current")
                .long("expect-no-current")
                .action(ArgAction::SetTrue)
                .help("Fence: current pointer must be absent (first promote)"),
        )
        .arg(
            Arg::new("expect-current-digest")
                .long("expect-current-digest")
                .value_name("DIGEST")
                .help("Fence: current pointer must equal this digest (update)"),
        )
}

fn fsync_cli() -> Command {
    Command::new("fsync")
        .about("fd-level fsync helper for Kill Switch scripts (no config / DB)")
        .arg(
            Arg::new("path")
                .value_name("PATH")
                .required(false)
                .help("File path to sync_all, then fsync its parent directory"),
        )
        .arg(
            Arg::new("probe")
                .long("probe")
                .action(ArgAction::SetTrue)
                .help("Capability probe: succeed without touching any path"),
        )
}

fn restricted_root_arg() -> Arg {
    Arg::new("restricted-root")
        .long("restricted-root")
        .required(true)
        .value_name("DIR")
        .value_parser(clap::value_parser!(PathBuf))
        .help("Restricted artifact root (UN-32)")
}

fn out_arg() -> Arg {
    Arg::new("out")
        .long("out")
        .required(true)
        .value_name("FILE")
        .help("Bare output file name under the run directory (candidate or sanitized report)")
}

fn restricted_out_arg() -> Arg {
    Arg::new("restricted-out")
        .long("restricted-out")
        .required(true)
        .value_name("FILE")
        .help("Bare restricted-channel output file name under the run directory")
}

pub(crate) fn load_mode(args: &ArgMatches) -> LoadMode {
    match args.subcommand() {
        Some((
            "fsync" | "promote" | "run-init" | "run-commit" | "run-abort" | "protect" | "unprotect"
            | "evidence-append",
            _,
        )) => LoadMode::None,
        Some(("bootstrap-candidate" | "compare", _)) => LoadMode::ParsedConfig,
        _ => LoadMode::ParsedConfig,
    }
}

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let result = match args.subcommand() {
        Some(("bootstrap-candidate", mode_args)) => exec_bootstrap(ctx, mode_args).await,
        Some(("compare", mode_args)) => exec_compare(ctx, mode_args).await,
        Some(("promote", mode_args)) => exec_promote(mode_args),
        Some(("run-init", mode_args)) => crate::commands::authz_audit_run::exec_run_init(mode_args),
        Some(("run-commit", mode_args)) => {
            crate::commands::authz_audit_run::exec_run_commit(mode_args)
        }
        Some(("run-abort", mode_args)) => {
            crate::commands::authz_audit_run::exec_run_abort(mode_args)
        }
        Some(("protect", mode_args)) => {
            crate::commands::authz_audit_protect::exec_protect(mode_args)
        }
        Some(("unprotect", mode_args)) => {
            crate::commands::authz_audit_protect::exec_unprotect(mode_args)
        }
        Some(("evidence-append", mode_args)) => {
            crate::commands::authz_audit_evidence::exec_evidence_append(mode_args)
        }
        Some(("fsync", mode_args)) => exec_fsync(mode_args),
        Some((other, _)) => Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("unknown authz-audit mode: {other}"),
        )),
        None => Err(MegaError::cli_exit(
            EXIT_PARAM,
            "authz-audit requires a mode subcommand",
        )),
    };
    result.map_err(map_cli_exit)
}

/// Collapse non-table errors onto exit 4 so this command never returns the
/// generic process exit 1 (frozen table: 0 / 2 / 3 / 4 only).
fn map_cli_exit(err: MegaError) -> MegaError {
    match err {
        MegaError::CliExit { .. } => err,
        other => MegaError::cli_exit(EXIT_PARAM, other.to_string()),
    }
}

async fn exec_bootstrap(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let summary = ctx.config_summary.clone();
    let config = require_config(ctx, "authz-audit bootstrap-candidate")?;
    let summary =
        summary.ok_or_else(|| MegaError::Other("missing config provenance summary".into()))?;
    let out_name = require_bare(args, "out")?;
    let restricted_name = require_bare(args, "restricted-out")?;
    let root_path = require_path(args, "restricted-root")?;

    let readonly = ReadOnlyContext::open(config, Some(summary.clone())).await?;
    let snapshot = readonly.storage.read_authz_source().await?;
    let outcome = bootstrap_candidate(&snapshot).map_err(audit_err)?;

    // --out = sanitized report; --restricted-out = candidate artifact (role projection).
    let report = assemble_report(&outcome.report, &summary);
    let report_bytes = serde_json::to_vec_pretty(&report)?;
    let artifact_bytes = serde_json::to_vec_pretty(&outcome.artifact)?;

    write_audit_run(
        &root_path,
        Producer::BootstrapCandidate,
        &out_name,
        &report_bytes,
        &restricted_name,
        &artifact_bytes,
    )?;
    Ok(())
}

async fn exec_compare(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let summary = ctx.config_summary.clone();
    let config = require_config(ctx, "authz-audit compare")?;
    let summary =
        summary.ok_or_else(|| MegaError::Other("missing config provenance summary".into()))?;
    let out_name = require_bare(args, "out")?;
    let restricted_name = require_bare(args, "restricted-out")?;
    let root_path = require_path(args, "restricted-root")?;
    let expect_digest = args
        .get_one::<String>("expect-digest")
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "--expect-digest is required"))?;

    let readonly = ReadOnlyContext::open(config, Some(summary.clone())).await?;
    let snapshot = readonly.storage.read_authz_source().await?;

    let root = RestrictedRoot::open(&root_path).map_err(artifact_err)?;
    let pointer = read_current_pointer(&root)
        .map_err(artifact_err)?
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "no current baseline pointer"))?;
    let version_bytes = read_version_for_digest(&root, &pointer.digest)
        .map_err(artifact_err)?
        .ok_or_else(|| {
            MegaError::cli_exit(
                EXIT_PARAM,
                "baseline version file missing for current pointer",
            )
        })?;
    let baseline: BaselineArtifact =
        serde_json::from_slice(&version_bytes).map_err(MegaError::from)?;

    let outcome = compare(&snapshot, &baseline, &expect_digest).map_err(audit_err)?;
    let report = assemble_report(&outcome.report, &summary);
    let report_bytes = serde_json::to_vec_pretty(&report)?;
    let findings_bytes = serde_json::to_vec_pretty(&outcome.findings)?;

    write_audit_run(
        &root_path,
        Producer::Compare,
        &out_name,
        &report_bytes,
        &restricted_name,
        &findings_bytes,
    )?;

    match outcome.report.diff_verdict {
        DiffVerdict::Match | DiffVerdict::NotCompared => Ok(()),
        DiffVerdict::Mismatch => Err(MegaError::cli_exit(
            EXIT_AUDIT_DIFF,
            "authorization ACL differs from the approved baseline",
        )),
    }
}

fn exec_promote(args: &ArgMatches) -> MegaResult {
    let root_path = require_path(args, "restricted-root")?;
    let candidate_ref = args
        .get_one::<String>("candidate")
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "--candidate is required"))?;
    let expect_digest = args
        .get_one::<String>("expect-digest")
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "--expect-digest is required"))?;
    let expect_no_current = args.get_flag("expect-no-current");
    let expect_current = args
        .get_one::<String>("expect-current-digest")
        .map(String::as_str);

    let fence = resolve_promote_fence(expect_no_current, expect_current).map_err(artifact_err)?;
    let reference = CandidateReference::parse(&candidate_ref).map_err(artifact_err)?;

    let root = RestrictedRoot::open(&root_path).map_err(artifact_err)?;
    let candidate = read_candidate(&root, &reference).map_err(artifact_err)?;

    let outcome = promote(
        &root,
        PromoteRequest {
            candidate: &candidate,
            expect_digest: &expect_digest,
            fence,
            now: SystemTime::now(),
            directory_entries: 0,
        },
    )
    .map_err(artifact_err)?;

    if let Some(marker) = outcome.already_current_marker() {
        eprintln!("{marker}");
    }
    Ok(())
}

fn exec_fsync(args: &ArgMatches) -> MegaResult {
    let probe = args.get_flag("probe");
    let path = args.get_one::<String>("path");
    match (probe, path) {
        (true, Some(_)) | (false, None) => Err(MegaError::cli_exit(
            EXIT_PARAM,
            "authz-audit fsync requires exactly one of <PATH> or --probe",
        )),
        (true, None) => Ok(()),
        (false, Some(path)) => fsync_path(Path::new(path)),
    }
}

fn fsync_path(path: &Path) -> MegaResult {
    let meta = std::fs::symlink_metadata(path).map_err(|err| {
        MegaError::cli_exit(EXIT_PARAM, format!("stat {}: {err}", path.display()))
    })?;
    if meta.file_type().is_symlink() {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("fsync refuses symlink path {}", path.display()),
        ));
    }
    if !meta.is_file() {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("fsync requires a regular file: {}", path.display()),
        ));
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|err| {
            MegaError::cli_exit(EXIT_PARAM, format!("open {}: {err}", path.display()))
        })?;
    file.sync_all().map_err(|err| {
        MegaError::cli_exit(EXIT_PARAM, format!("sync_all {}: {err}", path.display()))
    })?;

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let dir = OpenOptions::new().read(true).open(parent).map_err(|err| {
        MegaError::cli_exit(
            EXIT_PARAM,
            format!("open parent {}: {err}", parent.display()),
        )
    })?;
    // SAFETY: dir is an open directory fd; fsync is the directory durability barrier.
    let rc = unsafe { libc::fsync(dir.as_raw_fd()) };
    if rc != 0 {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!(
                "fsync parent {}: {}",
                parent.display(),
                io::Error::last_os_error()
            ),
        ));
    }
    Ok(())
}

fn write_audit_run(
    root_path: &Path,
    producer: Producer,
    out_name: &str,
    out_bytes: &[u8],
    restricted_name: &str,
    restricted_bytes: &[u8],
) -> MegaResult {
    let root = RestrictedRoot::open(root_path).map_err(artifact_err)?;
    let lock = MaintenanceLock::acquire(&root).map_err(artifact_err)?;
    let now = SystemTime::now();
    sweep(&root, &lock, &NoReservations, now).map_err(artifact_err)?;

    let reserved =
        ReservedRun::create(&root, &lock, producer, &NoLeases, now, 0, 0).map_err(artifact_err)?;
    let run_id_line = reserved.run.run_id_line();

    let write_both = (|| -> Result<(), ArtifactError> {
        reserved.run.write_output(out_name, out_bytes)?;
        reserved
            .run
            .write_output(restricted_name, restricted_bytes)?;
        Ok(())
    })();

    match write_both {
        Ok(()) => {
            let delta = i64::try_from(out_bytes.len().saturating_add(restricted_bytes.len()))
                .unwrap_or(i64::MAX);
            reserved
                .commit(&root, &lock, delta, now)
                .map_err(artifact_err)?;
            println!("{run_id_line}");
            Ok(())
        }
        Err(err) => {
            let _ = reserved.abort(&root, &lock, now);
            Err(artifact_err(err))
        }
    }
}

fn assemble_report(
    report: &SanitizedReport,
    summary: &crate::commands::LoadedConfigSummary,
) -> CliSanitizedReport {
    CliSanitizedReport {
        digest: report.digest.clone(),
        closure_count: report.closure_count,
        diff_verdict: report.diff_verdict,
        source_summary: summary.sanitized(),
    }
}

fn require_bare(args: &ArgMatches, name: &str) -> Result<String, MegaError> {
    let value = args
        .get_one::<String>(name)
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, format!("--{name} is required")))?;
    if value.contains('/') || value.contains('\\') || value == ".." || value.is_empty() {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("--{name} must be a bare file name"),
        ));
    }
    Ok(value)
}

fn require_path(args: &ArgMatches, name: &str) -> Result<PathBuf, MegaError> {
    args.get_one::<PathBuf>(name)
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, format!("--{name} is required")))
}

fn audit_err(err: crate::contract::policy::authz_audit::AuditError) -> MegaError {
    use crate::contract::policy::authz_audit::AuditError;
    let code = match &err {
        AuditError::BaselineTampered { .. } | AuditError::BaselineNotApproved { .. } => {
            EXIT_AUDIT_DIFF
        }
        AuditError::Json(_) | AuditError::Build(_) | AuditError::KeyEuidMismatch { .. } => {
            EXIT_PARAM
        }
    };
    MegaError::cli_exit(code, err.to_string())
}

fn artifact_err(err: ArtifactError) -> MegaError {
    match err {
        ArtifactError::PromotionFencing { reason, code } => MegaError::cli_exit(code, reason),
        // Writer / root / capacity / fence-param failures are environment or
        // parameter errors on this CLI surface (frozen table: 0/2/3/4).
        other => MegaError::cli_exit(EXIT_PARAM, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use clap::Command as ClapCommand;

    use super::*;
    use crate::commands::{builtin, load_mode};

    fn app() -> ClapCommand {
        ClapCommand::new("mega2").subcommands(builtin())
    }

    #[test]
    fn un29_authz_audit_is_registered_in_all_three_places() {
        let matches = app()
            .try_get_matches_from(["mega2", "authz-audit", "fsync", "--probe"])
            .expect("parse");
        let Some(("authz-audit", args)) = matches.subcommand() else {
            panic!("missing authz-audit");
        };
        assert!(matches!(
            load_mode("authz-audit", args),
            Some(LoadMode::None)
        ));
        assert!(crate::commands::builtin_exec("authz-audit").is_some());
    }

    #[test]
    fn un29_bootstrap_and_compare_load_parsed_config() {
        let matches = app()
            .try_get_matches_from([
                "mega2",
                "authz-audit",
                "bootstrap-candidate",
                "--restricted-root",
                "/tmp/r",
                "--out",
                "report.json",
                "--restricted-out",
                "candidate.json",
            ])
            .expect("parse");
        let Some(("authz-audit", args)) = matches.subcommand() else {
            panic!("missing");
        };
        assert_eq!(load_mode("authz-audit", args), Some(LoadMode::ParsedConfig));
    }

    #[test]
    fn un29_missing_out_flags_fail_clap_required() {
        let err = app().try_get_matches_from([
            "mega2",
            "authz-audit",
            "compare",
            "--restricted-root",
            "/tmp/r",
            "--expect-digest",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ]);
        assert!(err.is_err());
    }

    #[test]
    fn un29_fsync_probe_and_path_are_mutex_in_exec() {
        let matches = fsync_cli()
            .try_get_matches_from(["fsync", "--probe", "file.bin"])
            .expect("clap allows both; exec enforces mutex");
        let err = exec_fsync(&matches).expect_err("mutex");
        assert!(matches!(
            err,
            MegaError::CliExit {
                code: EXIT_PARAM,
                ..
            }
        ));
    }

    #[test]
    fn un29_fsync_probe_succeeds() {
        let matches = fsync_cli()
            .try_get_matches_from(["fsync", "--probe"])
            .expect("parse");
        exec_fsync(&matches).expect("probe");
    }

    #[test]
    fn un29_fsync_rejects_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let matches = fsync_cli()
            .try_get_matches_from(["fsync", dir.path().to_str().unwrap()])
            .expect("parse");
        let err = exec_fsync(&matches).expect_err("directory");
        assert!(matches!(
            err,
            MegaError::CliExit {
                code: EXIT_PARAM,
                ..
            }
        ));
    }

    #[test]
    fn un37_exit_codes_table() {
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_AUDIT_DIFF, 2);
        assert_eq!(EXIT_FENCING, 3);
        assert_eq!(EXIT_PARAM, 4);
    }

    #[test]
    fn un37_promote_load_mode_is_none() {
        let matches = app()
            .try_get_matches_from([
                "mega2",
                "authz-audit",
                "promote",
                "--restricted-root",
                "/tmp/r",
                "--candidate",
                "20260815T101112Z-1/candidate.json",
                "--expect-digest",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--expect-no-current",
            ])
            .expect("parse");
        let Some(("authz-audit", args)) = matches.subcommand() else {
            panic!("missing");
        };
        assert_eq!(load_mode("authz-audit", args), Some(LoadMode::None));
    }

    #[test]
    fn un37_promote_cas_flags_mutex_and_required() {
        let both = promote_cli()
            .try_get_matches_from([
                "promote",
                "--restricted-root",
                "/tmp/r",
                "--candidate",
                "20260815T101112Z-1/candidate.json",
                "--expect-digest",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--expect-no-current",
                "--expect-current-digest",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ])
            .expect("clap allows both; exec enforces mutex");
        let err = exec_promote(&both).expect_err("mutex");
        assert!(matches!(
            err,
            MegaError::CliExit {
                code: EXIT_PARAM,
                ..
            }
        ));

        let neither = promote_cli()
            .try_get_matches_from([
                "promote",
                "--restricted-root",
                "/tmp/r",
                "--candidate",
                "20260815T101112Z-1/candidate.json",
                "--expect-digest",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ])
            .expect("parse");
        let err = exec_promote(&neither).expect_err("missing fence");
        assert!(matches!(
            err,
            MegaError::CliExit {
                code: EXIT_PARAM,
                ..
            }
        ));
    }

    #[test]
    fn un37_promote_rejects_absolute_candidate() {
        let matches = promote_cli()
            .try_get_matches_from([
                "promote",
                "--restricted-root",
                "/tmp/r",
                "--candidate",
                "/abs/candidate.json",
                "--expect-digest",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--expect-no-current",
            ])
            .expect("parse");
        let err = exec_promote(&matches).expect_err("absolute");
        assert!(matches!(
            err,
            MegaError::CliExit {
                code: EXIT_PARAM,
                ..
            }
        ));
    }

    #[test]
    fn un37_promote_missing_required_flags_fail_clap() {
        let err = app().try_get_matches_from([
            "mega2",
            "authz-audit",
            "promote",
            "--restricted-root",
            "/tmp/r",
        ]);
        assert!(err.is_err());
    }
}
