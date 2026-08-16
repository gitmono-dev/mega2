//! UN-56: `authz-audit evidence-append` — strong-typed Kill Switch evidence writer.

use std::{path::PathBuf, time::SystemTime};

use clap::{Arg, ArgMatches, Command};
use serde::{Deserialize, Serialize};

use super::authz_audit::EXIT_PARAM;
use crate::{
    common::errors::{MegaError, MegaResult},
    contract::policy::{
        secure_artifact::{ArtifactError, RestrictedRoot, RunDir, validate_run_id},
        secure_hardcap::rfc3339_utc,
        secure_sweep::EVIDENCE_LOCK,
    },
};

/// Canonical evidence file name under `runs/<run-id>/` (UN-60 producer target).
pub(crate) const EVIDENCE_FILE: &str = "killswitch-evidence.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceChannel {
    Http,
    Git,
    Ssh,
    Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceCheck {
    HttpServing,
    HttpStatus,
    HttpBinding,
    TlsChain,
    GitLsRemote,
    GitBinding,
    SshNegotiate,
    SshHostKey,
    SshBinding,
    LogNoWouldDeny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceVerdict {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceCheckRecord {
    channel: EvidenceChannel,
    check: EvidenceCheck,
    verdict: EvidenceVerdict,
    status: Option<i32>,
    timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceDocument {
    schema_version: u32,
    run_id: String,
    checks: Vec<EvidenceCheckRecord>,
}

pub(crate) fn evidence_append_cli() -> Command {
    Command::new("evidence-append")
        .about("Append a sanitized Kill Switch evidence check (pure file)")
        .arg(restricted_root_arg())
        .arg(
            Arg::new("run-id")
                .long("run-id")
                .required(true)
                .value_name("ID"),
        )
        .arg(
            Arg::new("channel")
                .long("channel")
                .required(true)
                .value_name("CHANNEL")
                .help("http | git | ssh | log"),
        )
        .arg(
            Arg::new("check")
                .long("check")
                .required(true)
                .value_name("CHECK")
                .help("Channel-legal check enum value"),
        )
        .arg(
            Arg::new("verdict")
                .long("verdict")
                .required(true)
                .value_name("VERDICT")
                .help("pass | fail | skip"),
        )
        .arg(
            Arg::new("status")
                .long("status")
                .required(false)
                .value_name("INT")
                .value_parser(clap::value_parser!(i32)),
        )
}

fn restricted_root_arg() -> Arg {
    Arg::new("restricted-root")
        .long("restricted-root")
        .required(true)
        .value_name("DIR")
        .value_parser(clap::value_parser!(PathBuf))
}

pub(crate) fn exec_evidence_append(args: &ArgMatches) -> MegaResult {
    let root_path = args
        .get_one::<PathBuf>("restricted-root")
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "--restricted-root is required"))?;
    let run_id = args
        .get_one::<String>("run-id")
        .cloned()
        .ok_or_else(|| MegaError::cli_exit(EXIT_PARAM, "--run-id is required"))?;
    validate_run_id(&run_id).map_err(artifact_err)?;

    let channel = parse_channel(
        args.get_one::<String>("channel")
            .map(String::as_str)
            .unwrap_or(""),
    )?;
    let check = parse_check(
        args.get_one::<String>("check")
            .map(String::as_str)
            .unwrap_or(""),
    )?;
    if !channel_allows(channel, check) {
        return Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("check {check:?} is not legal for channel {channel:?}"),
        ));
    }
    let verdict = parse_verdict(
        args.get_one::<String>("verdict")
            .map(String::as_str)
            .unwrap_or(""),
    )?;
    let status = args.get_one::<i32>("status").copied();

    let root = RestrictedRoot::open(&root_path).map_err(artifact_err)?;
    let run = RunDir::open(&root, &run_id).map_err(artifact_err)?;
    let _lock = run.lock_exclusive(EVIDENCE_LOCK).map_err(artifact_err)?;

    let mut doc = match run.read_file(EVIDENCE_FILE).map_err(artifact_err)? {
        None => EvidenceDocument {
            schema_version: 1,
            run_id: run_id.clone(),
            checks: Vec::new(),
        },
        Some(bytes) => {
            let existing: EvidenceDocument = serde_json::from_slice(&bytes).map_err(|err| {
                MegaError::cli_exit(EXIT_PARAM, format!("parse {EVIDENCE_FILE}: {err}"))
            })?;
            if existing.schema_version != 1 {
                return Err(MegaError::cli_exit(
                    EXIT_PARAM,
                    format!(
                        "{EVIDENCE_FILE} schema_version {} is unsupported",
                        existing.schema_version
                    ),
                ));
            }
            if existing.run_id != run_id {
                return Err(MegaError::cli_exit(
                    EXIT_PARAM,
                    format!(
                        "{EVIDENCE_FILE} run_id {} does not match --run-id {run_id}",
                        existing.run_id
                    ),
                ));
            }
            for (i, record) in existing.checks.iter().enumerate() {
                if !channel_allows(record.channel, record.check) {
                    return Err(MegaError::cli_exit(
                        EXIT_PARAM,
                        format!(
                            "{EVIDENCE_FILE} checks[{i}]: check {:?} is not legal for channel {:?}",
                            record.check, record.channel
                        ),
                    ));
                }
                validate_rfc3339_utc(&record.timestamp).map_err(|reason| {
                    MegaError::cli_exit(
                        EXIT_PARAM,
                        format!("{EVIDENCE_FILE} checks[{i}].timestamp: {reason}"),
                    )
                })?;
            }
            existing
        }
    };

    doc.checks.push(EvidenceCheckRecord {
        channel,
        check,
        verdict,
        status,
        timestamp: rfc3339_utc(SystemTime::now()),
    });

    let bytes = serde_json::to_vec(&doc)
        .map_err(|err| MegaError::cli_exit(EXIT_PARAM, format!("serialize evidence: {err}")))?;
    run.replace_file(EVIDENCE_FILE, &bytes)
        .map_err(artifact_err)?;
    Ok(())
}

fn channel_allows(channel: EvidenceChannel, check: EvidenceCheck) -> bool {
    match channel {
        EvidenceChannel::Http => matches!(
            check,
            EvidenceCheck::HttpServing
                | EvidenceCheck::HttpStatus
                | EvidenceCheck::HttpBinding
                | EvidenceCheck::TlsChain
        ),
        EvidenceChannel::Git => {
            matches!(
                check,
                EvidenceCheck::GitLsRemote | EvidenceCheck::GitBinding
            )
        }
        EvidenceChannel::Ssh => matches!(
            check,
            EvidenceCheck::SshNegotiate | EvidenceCheck::SshHostKey | EvidenceCheck::SshBinding
        ),
        EvidenceChannel::Log => matches!(check, EvidenceCheck::LogNoWouldDeny),
    }
}

fn parse_channel(raw: &str) -> Result<EvidenceChannel, MegaError> {
    match raw {
        "http" => Ok(EvidenceChannel::Http),
        "git" => Ok(EvidenceChannel::Git),
        "ssh" => Ok(EvidenceChannel::Ssh),
        "log" => Ok(EvidenceChannel::Log),
        other => Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("unknown --channel `{other}` (expected http|git|ssh|log)"),
        )),
    }
}

fn parse_check(raw: &str) -> Result<EvidenceCheck, MegaError> {
    match raw {
        "http_serving" => Ok(EvidenceCheck::HttpServing),
        "http_status" => Ok(EvidenceCheck::HttpStatus),
        "http_binding" => Ok(EvidenceCheck::HttpBinding),
        "tls_chain" => Ok(EvidenceCheck::TlsChain),
        "git_ls_remote" => Ok(EvidenceCheck::GitLsRemote),
        "git_binding" => Ok(EvidenceCheck::GitBinding),
        "ssh_negotiate" => Ok(EvidenceCheck::SshNegotiate),
        "ssh_host_key" => Ok(EvidenceCheck::SshHostKey),
        "ssh_binding" => Ok(EvidenceCheck::SshBinding),
        "log_no_would_deny" => Ok(EvidenceCheck::LogNoWouldDeny),
        other => Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("unknown --check `{other}`"),
        )),
    }
}

fn parse_verdict(raw: &str) -> Result<EvidenceVerdict, MegaError> {
    match raw {
        "pass" => Ok(EvidenceVerdict::Pass),
        "fail" => Ok(EvidenceVerdict::Fail),
        "skip" => Ok(EvidenceVerdict::Skip),
        other => Err(MegaError::cli_exit(
            EXIT_PARAM,
            format!("unknown --verdict `{other}` (expected pass|fail|skip)"),
        )),
    }
}

/// Accept only the RFC3339 UTC form produced by [`rfc3339_utc`] (`…Z`, second precision).
fn validate_rfc3339_utc(ts: &str) -> Result<(), String> {
    let bytes = ts.as_bytes();
    if bytes.len() != 20 {
        return Err(format!("expected 20-char …Z form, got len {}", bytes.len()));
    }
    let ok_shape = bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[17..19].iter().all(u8::is_ascii_digit);
    if !ok_shape {
        return Err(format!("not RFC3339 UTC second-precision: {ts}"));
    }
    let month: u32 = std::str::from_utf8(&bytes[5..7])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let day: u32 = std::str::from_utf8(&bytes[8..10])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let hour: u32 = std::str::from_utf8(&bytes[11..13])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(99);
    let minute: u32 = std::str::from_utf8(&bytes[14..16])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(99);
    let second: u32 = std::str::from_utf8(&bytes[17..19])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(99);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(format!("out-of-range RFC3339 UTC fields: {ts}"));
    }
    Ok(())
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
    fn un56_evidence_append_is_pure_file() {
        let matches = app()
            .try_get_matches_from([
                "monoengine",
                "authz-audit",
                "evidence-append",
                "--restricted-root",
                "/tmp/r",
                "--run-id",
                "20260817T000000Z-1",
                "--channel",
                "http",
                "--check",
                "http_serving",
                "--verdict",
                "pass",
            ])
            .expect("parse");
        let Some(("authz-audit", args)) = matches.subcommand() else {
            panic!("missing");
        };
        assert_eq!(load_mode("authz-audit", args), Some(LoadMode::None));
    }

    #[test]
    fn un56_rejects_missing_required_args() {
        let err = app().try_get_matches_from([
            "monoengine",
            "authz-audit",
            "evidence-append",
            "--restricted-root",
            "/tmp/r",
            "--run-id",
            "20260817T000000Z-1",
            "--channel",
            "http",
            "--check",
            "http_serving",
        ]);
        assert!(err.is_err(), "verdict required");
    }

    #[test]
    fn un56_rejects_illegal_channel_check_pair() {
        let matches = evidence_append_cli()
            .try_get_matches_from([
                "evidence-append",
                "--restricted-root",
                "/tmp/r",
                "--run-id",
                "20260817T000000Z-1",
                "--channel",
                "http",
                "--check",
                "git_ls_remote",
                "--verdict",
                "pass",
            ])
            .expect("parse");
        let err = exec_evidence_append(&matches).expect_err("illegal pair");
        assert!(matches!(
            err,
            MegaError::CliExit {
                code: EXIT_PARAM,
                ..
            }
        ));
    }

    #[test]
    fn un56_rejects_unknown_enum() {
        let matches = evidence_append_cli()
            .try_get_matches_from([
                "evidence-append",
                "--restricted-root",
                "/tmp/r",
                "--run-id",
                "20260817T000000Z-1",
                "--channel",
                "ftp",
                "--check",
                "http_serving",
                "--verdict",
                "pass",
            ])
            .expect("parse");
        let err = exec_evidence_append(&matches).expect_err("bad channel");
        assert!(matches!(
            err,
            MegaError::CliExit {
                code: EXIT_PARAM,
                ..
            }
        ));
    }

    #[test]
    fn un56_channel_check_matrix_rows() {
        assert!(channel_allows(
            EvidenceChannel::Http,
            EvidenceCheck::HttpServing
        ));
        assert!(channel_allows(
            EvidenceChannel::Http,
            EvidenceCheck::TlsChain
        ));
        assert!(channel_allows(
            EvidenceChannel::Git,
            EvidenceCheck::GitLsRemote
        ));
        assert!(channel_allows(
            EvidenceChannel::Ssh,
            EvidenceCheck::SshHostKey
        ));
        assert!(channel_allows(
            EvidenceChannel::Log,
            EvidenceCheck::LogNoWouldDeny
        ));
        assert!(!channel_allows(
            EvidenceChannel::Log,
            EvidenceCheck::HttpServing
        ));
        assert!(!channel_allows(
            EvidenceChannel::Git,
            EvidenceCheck::SshBinding
        ));
    }

    #[test]
    fn un56_schema_has_no_forbidden_fields() {
        let doc = EvidenceDocument {
            schema_version: 1,
            run_id: "20260817T000000Z-1".into(),
            checks: vec![EvidenceCheckRecord {
                channel: EvidenceChannel::Http,
                check: EvidenceCheck::HttpServing,
                verdict: EvidenceVerdict::Pass,
                status: None,
                timestamp: "2026-08-17T00:00:00Z".into(),
            }],
        };
        let json = serde_json::to_string(&doc).unwrap();
        for forbidden in [
            "stdout",
            "stderr",
            "refs",
            "password",
            "token",
            "credential",
            "/home/",
            "C:\\\\",
        ] {
            assert!(
                !json.contains(forbidden),
                "forbidden `{forbidden}` in {json}"
            );
        }
    }
}
