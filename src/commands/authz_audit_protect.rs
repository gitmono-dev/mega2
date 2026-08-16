//! UN-55: `authz-audit protect` / `unprotect` — unique writers for `protected.json`.

use std::{collections::BTreeSet, path::PathBuf, time::SystemTime};

use clap::{Arg, ArgMatches, Command};
use serde::{Deserialize, Serialize};

use super::authz_audit::EXIT_PARAM;
use crate::{
    common::errors::{MegaError, MegaResult},
    contract::policy::{
        baseline_pointer::validate_artifact_digest,
        secure_artifact::{
            ArtifactError, RestrictedRoot, read_baseline_file, replace_baseline_file,
        },
        secure_capacity::P_MAX,
        secure_hardcap::{new_op_id, rfc3339_utc},
        secure_lifecycle::{
            NoLeases, ReserveRequest, SettleRequest, abort_locked, admit_and_reserve_locked,
            commit_locked, signed_delta,
        },
        secure_producer::Producer,
        secure_sweep::{MaintenanceLock, PROTECTED_MANIFEST},
    },
};

const TARGET: &str = "baselines/protected.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtectedManifest {
    schema_version: u32,
    protected: Vec<String>,
}

pub(crate) fn protect_cli() -> Command {
    Command::new("protect")
        .about("Register a baseline digest in protected.json (pure file)")
        .arg(restricted_root_arg())
        .arg(digest_arg())
}

pub(crate) fn unprotect_cli() -> Command {
    Command::new("unprotect")
        .about("Remove a baseline digest from protected.json (pure file)")
        .arg(restricted_root_arg())
        .arg(digest_arg())
}

fn restricted_root_arg() -> Arg {
    Arg::new("restricted-root")
        .long("restricted-root")
        .required(true)
        .value_name("DIR")
        .value_parser(clap::value_parser!(PathBuf))
}

fn digest_arg() -> Arg {
    Arg::new("digest")
        .long("digest")
        .required(true)
        .value_name("DIGEST")
        .help("Baseline digest to protect/unprotect (sha256:<64hex>)")
}

pub(crate) fn exec_protect(args: &ArgMatches) -> MegaResult {
    mutate_manifest(args, ProtectOp::Protect)
}

pub(crate) fn exec_unprotect(args: &ArgMatches) -> MegaResult {
    mutate_manifest(args, ProtectOp::Unprotect)
}

#[derive(Clone, Copy)]
enum ProtectOp {
    Protect,
    Unprotect,
}

fn mutate_manifest(args: &ArgMatches, op: ProtectOp) -> MegaResult {
    let root_path = args
        .get_one::<PathBuf>("restricted-root")
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "--restricted-root is required"))?;
    let digest = args
        .get_one::<String>("digest")
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "--digest is required"))?;
    validate_artifact_digest(&digest).map_err(artifact_err)?;

    let root = RestrictedRoot::open(&root_path).map_err(artifact_err)?;
    let lock = MaintenanceLock::acquire(&root).map_err(artifact_err)?;
    let now = SystemTime::now();

    let before_bytes = read_baseline_file(&root, PROTECTED_MANIFEST).map_err(artifact_err)?;
    let before_len = before_bytes.as_ref().map(|b| b.len() as u64).unwrap_or(0);
    let mut set = load_protected_set(before_bytes.as_deref())?;

    let changed = match op {
        ProtectOp::Protect => {
            if set.contains(&digest) {
                false
            } else {
                if set.len() >= P_MAX {
                    return Err(MegaError::cli_exit(
                        EXIT_PARAM,
                        format!("protected.json already has {P_MAX} entries (P limit)"),
                    ));
                }
                set.insert(digest.clone());
                true
            }
        }
        ProtectOp::Unprotect => set.remove(&digest),
    };

    if !changed {
        // Idempotent: expected end state already holds.
        return Ok(());
    }

    let after = encode_manifest(&set)?;
    let after_len = after.len() as u64;
    let producer = match op {
        ProtectOp::Protect => Producer::Protect,
        ProtectOp::Unprotect => Producer::Unprotect,
    };
    let op_id = new_op_id();
    admit_and_reserve_locked(
        &root,
        &lock,
        producer,
        ReserveRequest {
            op_id: op_id.clone(),
            target: TARGET.into(),
            payload: digest.clone(),
            owner_fenced: None,
            created_at: rfc3339_utc(now),
            active_runs: 0,
            protected_count: match op {
                ProtectOp::Protect => set.len().saturating_sub(1),
                ProtectOp::Unprotect => set.len().saturating_add(1),
            },
            directory_entries: 0,
        },
        &NoLeases,
        now,
    )
    .map_err(artifact_err)?;

    match replace_baseline_file(&root, PROTECTED_MANIFEST, &after) {
        Ok(()) => {
            let delta = signed_delta(before_len, after_len).map_err(artifact_err)?;
            commit_locked(
                &root,
                &lock,
                SettleRequest {
                    op_id,
                    settled_delta: delta,
                    final_state: match op {
                        ProtectOp::Protect => "digest present".into(),
                        ProtectOp::Unprotect => "digest absent".into(),
                    },
                    settled_at: rfc3339_utc(now),
                    run_id: None,
                    cap_hash: None,
                },
            )
            .map_err(artifact_err)?;
            Ok(())
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
                    run_id: None,
                    cap_hash: None,
                },
            );
            Err(artifact_err(err))
        }
    }
}

fn load_protected_set(bytes: Option<&[u8]>) -> Result<BTreeSet<String>, MegaError> {
    let Some(bytes) = bytes else {
        return Ok(BTreeSet::new());
    };
    let manifest: ProtectedManifest = serde_json::from_slice(bytes)
        .map_err(|err| MegaError::cli_exit(EXIT_PARAM, format!("parse protected.json: {err}")))?;
    if manifest.schema_version != 1 {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!(
                "protected.json schema_version {} is unsupported",
                manifest.schema_version
            ),
        ));
    }
    let mut set = BTreeSet::new();
    for digest in manifest.protected {
        validate_artifact_digest(&digest).map_err(artifact_err)?;
        if !set.insert(digest.clone()) {
            return Err(MegaError::cli_exit(
                EXIT_PARAM,
                format!("protected.json contains duplicate digest {digest}"),
            ));
        }
    }
    Ok(set)
}

fn encode_manifest(set: &BTreeSet<String>) -> Result<Vec<u8>, MegaError> {
    let manifest = ProtectedManifest {
        schema_version: 1,
        protected: set.iter().cloned().collect(),
    };
    // Compact canonical JSON: fixed key order via struct fields, sorted digests,
    // no trailing newline.
    serde_json::to_vec(&manifest)
        .map_err(|err| MegaError::cli_exit(EXIT_PARAM, format!("serialize protected.json: {err}")))
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
        ClapCommand::new("monoengine").subcommands(builtin())
    }

    #[test]
    fn un55_protect_modes_are_pure_file() {
        for mode in ["protect", "unprotect"] {
            let matches = app()
                .try_get_matches_from([
                    "monoengine",
                    "authz-audit",
                    mode,
                    "--restricted-root",
                    "/tmp/r",
                    "--digest",
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                ])
                .expect("parse");
            let Some(("authz-audit", args)) = matches.subcommand() else {
                panic!("missing");
            };
            assert_eq!(load_mode("authz-audit", args), Some(LoadMode::None));
        }
    }

    #[test]
    fn un55_digest_required() {
        let err = app().try_get_matches_from([
            "monoengine",
            "authz-audit",
            "protect",
            "--restricted-root",
            "/tmp/r",
        ]);
        assert!(err.is_err());
    }

    #[test]
    fn un55_rejects_bad_digest_in_exec() {
        let matches = protect_cli()
            .try_get_matches_from([
                "protect",
                "--restricted-root",
                "/tmp/r",
                "--digest",
                "not-a-digest",
            ])
            .expect("parse");
        let err = exec_protect(&matches).expect_err("bad digest");
        assert!(matches!(
            err,
            MegaError::CliExit {
                code: EXIT_PARAM,
                ..
            }
        ));
    }
}
