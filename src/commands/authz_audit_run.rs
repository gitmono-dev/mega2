//! UN-52: `authz-audit run-init` / `run-commit` / `run-abort` (Kill Switch evidence runs).

use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::SystemTime,
};

use clap::{Arg, ArgMatches, Command};
use rand::RngExt;
use sha2::{Digest, Sha256};

use super::authz_audit::EXIT_PARAM;
use crate::{
    common::errors::{MegaError, MegaResult},
    contract::policy::{
        secure_artifact::{
            ArtifactError, RUNS_DIR, RestrictedRoot, RunDir, generate_run_id, validate_run_id,
        },
        secure_counter::{ReservationKind, load_counter},
        secure_hardcap::{new_op_id, rfc3339_utc},
        secure_lifecycle::{
            NoLeases, ReserveRequest, SettleRequest, abort_locked, admit_and_reserve_locked,
            commit_locked,
        },
        secure_producer::Producer,
        secure_sweep::{EVIDENCE_LOCK, LEASE_LOCK, MaintenanceLock},
    },
};

const RUN_CAP_ENV: &str = "RUN_CAP";

pub(crate) fn run_init_cli() -> Command {
    Command::new("run-init")
        .about("Admit and claim a Kill Switch evidence run directory (pure file)")
        .arg(restricted_root_arg())
}

pub(crate) fn run_commit_cli() -> Command {
    Command::new("run-commit")
        .about("Settle an evidence run (requires RUN_CAP)")
        .arg(restricted_root_arg())
        .arg(run_id_arg())
}

pub(crate) fn run_abort_cli() -> Command {
    Command::new("run-abort")
        .about("Abort an evidence run under the three-state freeze (requires RUN_CAP)")
        .arg(restricted_root_arg())
        .arg(run_id_arg())
}

fn restricted_root_arg() -> Arg {
    Arg::new("restricted-root")
        .long("restricted-root")
        .required(true)
        .value_name("DIR")
        .value_parser(clap::value_parser!(PathBuf))
}

fn run_id_arg() -> Arg {
    Arg::new("run-id")
        .long("run-id")
        .required(true)
        .value_name("ID")
}

pub(crate) fn exec_run_init(args: &ArgMatches) -> MegaResult {
    let root_path = require_path(args, "restricted-root")?;
    let root = RestrictedRoot::open(&root_path).map_err(artifact_err)?;
    let lock = MaintenanceLock::acquire(&root).map_err(artifact_err)?;
    let now = SystemTime::now();

    let counter = load_counter(&root, &lock).map_err(artifact_err)?;
    let active_runs = counter
        .reservations
        .iter()
        .filter(|r| r.kind == ReservationKind::Run)
        .count() as u64;

    let plaintext = generate_run_cap();
    let cap_hash = hash_run_cap(&plaintext);

    let mut last_err = None;
    for _ in 0..8 {
        let run_id = generate_run_id();
        let op_id = new_op_id();
        let target = format!("{RUNS_DIR}/{run_id}");
        match admit_and_reserve_locked(
            &root,
            &lock,
            Producer::KillSwitchEvidence,
            ReserveRequest {
                op_id: op_id.clone(),
                target,
                payload: cap_hash.clone(),
                owner_fenced: Some(true),
                created_at: rfc3339_utc(now),
                active_runs,
                protected_count: 0,
                directory_entries: 0,
            },
            &NoLeases,
            now,
        ) {
            Ok(_) => match RunDir::claim_exact(&root, &run_id) {
                Ok(_run) => {
                    println!("run_id={run_id}");
                    println!("run_cap={plaintext}");
                    return Ok(());
                }
                Err(err) => {
                    let _ = abort_locked(
                        &root,
                        &lock,
                        SettleRequest {
                            op_id,
                            settled_delta: 0,
                            final_state: "aborted".into(),
                            settled_at: rfc3339_utc(now),
                            run_id: Some(run_id),
                            cap_hash: Some(cap_hash.clone()),
                        },
                    );
                    last_err = Some(err);
                }
            },
            Err(err) => return Err(artifact_err(err)),
        }
    }
    Err(artifact_err(
        last_err.unwrap_or(ArtifactError::RunIdExhausted { attempts: 8 }),
    ))
}

pub(crate) fn exec_run_commit(args: &ArgMatches) -> MegaResult {
    let root_path = require_path(args, "restricted-root")?;
    let run_id = require_run_id(args)?;
    let cap_hash = require_cap_hash()?;
    let root = RestrictedRoot::open(&root_path).map_err(artifact_err)?;
    let lock = MaintenanceLock::acquire(&root).map_err(artifact_err)?;
    let now = SystemTime::now();

    let op_id = resolve_op_id(&root, &lock, &run_id, &cap_hash)?;
    let bytes = measure_run_bytes(&root_path, &run_id)?;
    let delta = i64::try_from(bytes).unwrap_or(i64::MAX);

    commit_locked(
        &root,
        &lock,
        SettleRequest {
            op_id,
            settled_delta: delta,
            final_state: "committed".into(),
            settled_at: rfc3339_utc(now),
            run_id: Some(run_id),
            cap_hash: Some(cap_hash),
        },
    )
    .map_err(artifact_err)?;
    Ok(())
}

pub(crate) fn exec_run_abort(args: &ArgMatches) -> MegaResult {
    let root_path = require_path(args, "restricted-root")?;
    let run_id = require_run_id(args)?;
    let cap_hash = require_cap_hash()?;
    let root = RestrictedRoot::open(&root_path).map_err(artifact_err)?;
    let lock = MaintenanceLock::acquire(&root).map_err(artifact_err)?;
    let now = SystemTime::now();

    let op_id = resolve_op_id(&root, &lock, &run_id, &cap_hash)?;
    let run_dir = root_path.join(RUNS_DIR).join(&run_id);

    let (delta, final_state) = if !run_dir.exists() {
        // Already removed (empty/locks-only abort already ran): settle is idempotent.
        (0_i64, "aborted")
    } else {
        match classify_run_dir(&root_path, &run_id)? {
            RunAbortState::Empty | RunAbortState::LocksOnly => {
                remove_run_directory(&root_path, &run_id)?;
                (0_i64, "aborted")
            }
            RunAbortState::Partial { bytes } => {
                (i64::try_from(bytes).unwrap_or(i64::MAX), "aborted")
            }
        }
    };

    abort_locked(
        &root,
        &lock,
        SettleRequest {
            op_id,
            settled_delta: delta,
            final_state: final_state.into(),
            settled_at: rfc3339_utc(now),
            run_id: Some(run_id),
            cap_hash: Some(cap_hash),
        },
    )
    .map_err(artifact_err)?;
    Ok(())
}

#[derive(Debug)]
enum RunAbortState {
    Empty,
    LocksOnly,
    Partial { bytes: u64 },
}

fn classify_run_dir(root_path: &Path, run_id: &str) -> Result<RunAbortState, MegaError> {
    let dir = root_path.join(RUNS_DIR).join(run_id);
    if !dir.is_dir() {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("run directory missing: {}", dir.display()),
        ));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|err| {
        MegaError::cli_exit(EXIT_PARAM, format!("read_dir {}: {err}", dir.display()))
    })? {
        let entry = entry.map_err(|err| MegaError::cli_exit(EXIT_PARAM, err.to_string()))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "." || name == ".." {
            continue;
        }
        entries.push(name.into_owned());
    }

    if entries.is_empty() {
        return Ok(RunAbortState::Empty);
    }
    if entries
        .iter()
        .all(|n| n == LEASE_LOCK || n == EVIDENCE_LOCK)
    {
        return Ok(RunAbortState::LocksOnly);
    }
    Ok(RunAbortState::Partial {
        bytes: measure_run_bytes(root_path, run_id)?,
    })
}

fn measure_run_bytes(root_path: &Path, run_id: &str) -> Result<u64, MegaError> {
    let dir = root_path.join(RUNS_DIR).join(run_id);
    let mut total = 0u64;
    fn walk(path: &Path, total: &mut u64) -> Result<(), MegaError> {
        let meta = fs::symlink_metadata(path).map_err(|err| {
            MegaError::cli_exit(EXIT_PARAM, format!("stat {}: {err}", path.display()))
        })?;
        if meta.file_type().is_symlink() {
            return Err(MegaError::cli_exit(
                EXIT_PARAM,
                format!("run tree must not contain symlinks: {}", path.display()),
            ));
        }
        if meta.is_dir() {
            for entry in fs::read_dir(path).map_err(|err| {
                MegaError::cli_exit(EXIT_PARAM, format!("read_dir {}: {err}", path.display()))
            })? {
                let entry =
                    entry.map_err(|err| MegaError::cli_exit(EXIT_PARAM, err.to_string()))?;
                walk(&entry.path(), total)?;
            }
        } else if meta.is_file() {
            *total = total.saturating_add(meta.len());
        }
        Ok(())
    }
    walk(&dir, &mut total)?;
    Ok(total)
}

fn remove_run_directory(root_path: &Path, run_id: &str) -> MegaResult {
    let dir = root_path.join(RUNS_DIR).join(run_id);
    // Remove known lock files first so remove_dir can succeed for LocksOnly.
    for name in [LEASE_LOCK, EVIDENCE_LOCK] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(|err| {
                MegaError::cli_exit(EXIT_PARAM, format!("unlink {}: {err}", path.display()))
            })?;
        }
    }
    // Refuse if anything else remains (Partial must not call this).
    let leftover: Vec<_> = fs::read_dir(&dir)
        .map_err(|err| MegaError::cli_exit(EXIT_PARAM, err.to_string()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .collect();
    if !leftover.is_empty() {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("refusing to delete non-empty run dir with {:?}", leftover),
        ));
    }
    fs::remove_dir(&dir).map_err(|err| {
        MegaError::cli_exit(EXIT_PARAM, format!("rmdir {}: {err}", dir.display()))
    })?;

    let parent = root_path.join(RUNS_DIR);
    let parent_fd = OpenOptions::new().read(true).open(&parent).map_err(|err| {
        MegaError::cli_exit(EXIT_PARAM, format!("open {}: {err}", parent.display()))
    })?;
    let rc = unsafe { libc::fsync(parent_fd.as_raw_fd()) };
    if rc != 0 {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("fsync {}: {}", parent.display(), io::Error::last_os_error()),
        ));
    }
    Ok(())
}

fn resolve_op_id(
    root: &RestrictedRoot,
    lock: &MaintenanceLock,
    run_id: &str,
    cap_hash: &str,
) -> Result<String, MegaError> {
    let counter = load_counter(root, lock).map_err(artifact_err)?;
    let prefix = format!("{RUNS_DIR}/{run_id}");
    if let Some(res) = counter.reservations.iter().find(|r| {
        r.kind == ReservationKind::Run
            && r.owner_fenced == Some(true)
            && (r.target == prefix || r.target.starts_with(&(prefix.clone() + "/")))
    }) {
        if !ct_eq(&res.payload, cap_hash) {
            return Err(MegaError::cli_exit(
                EXIT_PARAM,
                "cap_hash does not match reservation",
            ));
        }
        return Ok(res.op_id.clone());
    }
    if let Some(tomb) = counter
        .settled
        .iter()
        .find(|t| t.owner_fenced == Some(true) && t.run_id.as_deref() == Some(run_id))
    {
        let Some(stored) = tomb.cap_hash.as_deref() else {
            return Err(MegaError::cli_exit(
                EXIT_PARAM,
                "settled tomb missing cap_hash",
            ));
        };
        if !ct_eq(stored, cap_hash) {
            return Err(MegaError::cli_exit(
                EXIT_PARAM,
                "cap_hash does not match settled tomb",
            ));
        }
        return Ok(tomb.op_id.clone());
    }
    Err(MegaError::cli_exit(
        EXIT_PARAM,
        format!("no active or settled reservation for run_id={run_id}"),
    ))
}

fn require_cap_hash() -> Result<String, MegaError> {
    let plaintext = if let Ok(value) = std::env::var(RUN_CAP_ENV) {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            return Err(MegaError::cli_exit(
                EXIT_PARAM,
                "RUN_CAP environment variable is empty",
            ));
        }
        trimmed
    } else {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|err| MegaError::cli_exit(EXIT_PARAM, format!("read RUN_CAP stdin: {err}")))?;
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            return Err(MegaError::cli_exit(
                EXIT_PARAM,
                "RUN_CAP missing: set the environment variable or provide it on stdin",
            ));
        }
        trimmed
    };
    if plaintext.contains('=') || plaintext.contains('\n') || plaintext.starts_with("sha256:") {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            "RUN_CAP must be the plaintext capability from run-init (not run_cap=/hash form)",
        ));
    }
    Ok(hash_run_cap(&plaintext))
}

fn generate_run_cap() -> String {
    let n: u128 = rand::rng().random();
    format!("{n:032x}")
}

fn hash_run_cap(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn require_path(args: &ArgMatches, name: &str) -> Result<PathBuf, MegaError> {
    args.get_one::<PathBuf>(name)
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, format!("--{name} is required")))
}

fn require_run_id(args: &ArgMatches) -> Result<String, MegaError> {
    let value = args
        .get_one::<String>("run-id")
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "--run-id is required"))?;
    validate_run_id(&value).map_err(artifact_err)?;
    Ok(value)
}

fn artifact_err(err: ArtifactError) -> MegaError {
    MegaError::cli_exit(EXIT_PARAM, err.to_string())
}

#[cfg(test)]
mod tests {
    use clap::Command as ClapCommand;

    use super::*;
    use crate::commands::{LoadMode, builtin, load_mode};

    fn app() -> ClapCommand {
        ClapCommand::new("mega2").subcommands(builtin())
    }

    #[test]
    fn un52_run_modes_are_pure_file() {
        for mode in ["run-init", "run-commit", "run-abort"] {
            let mut argv = vec![
                "mega2".into(),
                "authz-audit".into(),
                mode.to_string(),
                "--restricted-root".into(),
                "/tmp/r".into(),
            ];
            if mode != "run-init" {
                argv.push("--run-id".into());
                argv.push("20260815T101112Z-1".into());
            }
            let matches = app().try_get_matches_from(argv).expect("parse");
            let Some(("authz-audit", args)) = matches.subcommand() else {
                panic!("missing");
            };
            assert_eq!(
                load_mode("authz-audit", args),
                Some(LoadMode::None),
                "{mode}"
            );
        }
    }

    #[test]
    fn un52_run_commit_requires_run_id() {
        let err = app().try_get_matches_from([
            "mega2",
            "authz-audit",
            "run-commit",
            "--restricted-root",
            "/tmp/r",
        ]);
        assert!(err.is_err());
    }

    #[test]
    fn un52_hash_is_stable_and_rejects_mismatch() {
        let a = hash_run_cap("abc");
        let b = hash_run_cap("abc");
        let c = hash_run_cap("abd");
        assert!(ct_eq(&a, &b));
        assert!(!ct_eq(&a, &c));
        assert!(a.starts_with("sha256:"));
        assert_eq!(a.len(), "sha256:".len() + 64);
    }
}
