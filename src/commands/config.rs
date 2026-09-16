use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Map, Value};

use crate::{
    cli::init_log,
    commands::{CommandContext, LoadMode, require_config_path},
    common::errors::{MegaError, MegaResult},
    config::{
        Config,
        secret::{SecretRef, SecretResolver, VaultSecretResolver, is_secret_ref_value},
        template::config_init_template,
        validate::{
            ConfigSourceDiagnostics, collect_source_diagnostics, validate_config_secret_ref,
            validate_redis_url_literal,
        },
    },
    contract::vault::integration::vault_core::{VaultCore, VaultCoreInterface, with_audit_caller},
};

/// Config-managed secret fields that may be stored in the monoengine vault, with
/// the vault namespace suffix each must use (`config/<profile>/<suffix>`). Only
/// these fields are accepted by `config secret set/check/rotate/ref`. Database
/// credentials remain intentionally excluded — they are a bootstrap dependency
/// that must stay in deployment/environment secrets. Redis URLs and
/// object-storage S3 credentials may now be vault-backed and are resolved
/// post-vault bootstrap.
///
/// WH-11 adds the parameterized `storage_events.targets.<id>.secret_ref` field
/// (namespace `config/<profile>/storage_events/targets/<id>/hmac`), handled by
/// [`storage_events_target_suffix`] because the suffix embeds the target id.
const SUPPORTED_SECRET_FIELDS: &[(&str, &str)] = &[
    (
        "notification.slack.webhook_url",
        "notification/slack/webhook_url",
    ),
    ("notification.webhook.token", "notification/webhook/token"),
    ("redis.url", "redis/url"),
    (
        "object_storage.s3.access_key_id",
        "object_storage/access_key_id",
    ),
    (
        "object_storage.s3.secret_access_key",
        "object_storage/secret_access_key",
    ),
];

/// WH-11: `storage_events.targets.<id>.secret_ref` maps to the per-target
/// namespace suffix `storage_events/targets/<id>/hmac`. The id charset matches
/// `config validate` (1..=32 ASCII `[A-Za-z0-9_-]`).
fn storage_events_target_suffix(name: &str) -> Option<String> {
    let id = name
        .strip_prefix("storage_events.targets.")?
        .strip_suffix(".secret_ref")?;
    if id.is_empty()
        || id.len() > 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some(format!("storage_events/targets/{id}/hmac"))
}

/// Look up the required vault namespace suffix for a supported secret field.
fn supported_secret_suffix(name: &str) -> Option<String> {
    if let Some((_, suffix)) = SUPPORTED_SECRET_FIELDS
        .iter()
        .find(|(field, _)| *field == name)
    {
        return Some((*suffix).to_string());
    }
    storage_events_target_suffix(name)
}

/// Validate that `secret_ref` uses the namespace required for the named field.
fn validate_secret_field_ref(name: &str, secret_ref: &SecretRef) -> Result<(), MegaError> {
    match supported_secret_suffix(name) {
        Some(suffix) => validate_config_secret_ref(name, secret_ref, &suffix),
        None => Err(unsupported_secret_field_error(name)),
    }
}

fn unsupported_secret_field_error(name: &str) -> MegaError {
    let supported = SUPPORTED_SECRET_FIELDS
        .iter()
        .map(|(field, _)| *field)
        .collect::<Vec<_>>()
        .join(", ");
    MegaError::Other(format!(
        "{name} cannot be stored in monoengine vault; supported fields are: {supported}, storage_events.targets.<id>.secret_ref. Database credentials must stay in deployment/environment secrets."
    ))
}

pub fn cli() -> Command {
    Command::new("config")
        .about("Inspect and validate monoengine configuration")
        .subcommand(
            Command::new("secret")
                .about("Manage config-backed vault secret references")
                .subcommand_required(true)
                .subcommand(secret_ref_cli())
                .subcommand(secret_set_cli())
                .subcommand(secret_rotate_cli())
                .subcommand(secret_check_cli()),
        )
        .subcommand(
            Command::new("vault")
                .about("Manage the monoengine vault")
                .subcommand_required(true)
                .subcommand(vault_reset_cli())
                .subcommand(vault_rekey_cli())
                .subcommand(vault_backup_cli())
                .subcommand(vault_restore_cli()),
        )
        .subcommand(
            Command::new("init")
                .about("Create a safe starter configuration without reading config or vault")
                .arg(
                    Arg::new("output")
                        .long("output")
                        .short('o')
                        .value_name("PATH")
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("Output config path; defaults to --config or config/config.toml"),
                )
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Overwrite an existing config file"),
                ),
        )
        .subcommand(
            Command::new("validate")
                .about("Validate configuration")
                .arg(
                    Arg::new("resolve-secrets")
                        .long("resolve-secrets")
                        .action(ArgAction::SetTrue)
                        .help("Resolve configured SecretRef values through the minimal DB/Vault bootstrap"),
                )
                .arg(
                    Arg::new("deny-warnings")
                        .long("deny-warnings")
                        .action(ArgAction::SetTrue)
                        .help("Fail validation when source diagnostics emit warnings"),
                )
                .arg(
                    Arg::new("show-sources")
                        .long("show-sources")
                        .action(ArgAction::SetTrue)
                        .help("Print base/profile/env source field and override diagnostics"),
                )
                .arg(
                    Arg::new("format")
                        .long("format")
                        .value_name("FORMAT")
                        .value_parser(["human", "json"])
                        .default_value("human")
                        .help("Output format for source diagnostics (human | json)"),
                ),
        )
}

fn secret_ref_cli() -> Command {
    Command::new("ref")
        .about("Print a vault SecretRef URI without reading config or vault")
        .arg(secret_name_arg())
        .arg(vault_path_arg())
        .arg(field_arg())
}

fn secret_set_cli() -> Command {
    Command::new("set")
        .about("Store a supported config secret in vault")
        .arg(secret_name_arg())
        .arg(vault_path_arg())
        .arg(field_arg())
        .arg(
            Arg::new("value-stdin")
                .long("value-stdin")
                .action(ArgAction::SetTrue)
                .required(true)
                .help("Read the secret value from stdin"),
        )
}

fn secret_rotate_cli() -> Command {
    Command::new("rotate")
        .about("Rotate a supported config secret in vault by overwriting its value")
        .arg(secret_name_arg())
        .arg(vault_path_arg())
        .arg(field_arg())
        .arg(
            Arg::new("value-stdin")
                .long("value-stdin")
                .action(ArgAction::SetTrue)
                .required(true)
                .help("Read the new secret value from stdin"),
        )
}

fn secret_check_cli() -> Command {
    Command::new("check")
        .about("Check that a supported config secret exists in vault")
        .arg(secret_name_arg())
        .arg(
            Arg::new("ref")
                .long("ref")
                .value_name("SECRET_REF")
                .conflicts_with("vault-path")
                .help("SecretRef URI to check"),
        )
        .arg(vault_path_arg().required_unless_present("ref"))
        .arg(field_arg())
}

fn vault_reset_cli() -> Command {
    Command::new("reset")
        .about("Reset the vault: delete all vault storage rows, back up core_key.json, and re-initialize")
        .arg(
            Arg::new("force")
                .long("force")
                .action(ArgAction::SetTrue)
                .required(true)
                .help("Confirm this destructive operation (required)"),
        )
        .arg(
            Arg::new("key-path")
                .long("key-path")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help("Path to core_key.json; defaults to the standard vault data directory"),
        )
}

fn vault_rekey_cli() -> Command {
    Command::new("rekey")
        .about("Regenerate and persist a fresh unseal share set for the current vault key")
        .arg(
            Arg::new("force")
                .long("force")
                .action(ArgAction::SetTrue)
                .required(true)
                .help("Confirm this operation (required)"),
        )
        .arg(
            Arg::new("key-path")
                .long("key-path")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help("Path to core_key.json; defaults to the standard vault data directory"),
        )
}

fn vault_backup_cli() -> Command {
    Command::new("backup")
        .about("Back up the vault core key file to a safe location")
        .arg(
            Arg::new("destination")
                .value_name("PATH")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("Destination file or directory for the backed-up core key"),
        )
        .arg(
            Arg::new("key-path")
                .long("key-path")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help("Path to core_key.json; defaults to the standard vault data directory"),
        )
}

fn vault_restore_cli() -> Command {
    Command::new("restore")
        .about("Restore the vault core key file from a backup and verify it unlocks the vault")
        .arg(
            Arg::new("source")
                .value_name("PATH")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("Backup core key file to restore"),
        )
        .arg(
            Arg::new("force")
                .long("force")
                .action(ArgAction::SetTrue)
                .required(true)
                .help("Confirm overwriting an existing core_key.json (required)"),
        )
        .arg(
            Arg::new("key-path")
                .long("key-path")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .help("Path to core_key.json; defaults to the standard vault data directory"),
        )
}

fn secret_name_arg() -> Arg {
    Arg::new("name")
        .value_name("CONFIG_FIELD")
        .required(true)
        .help(
            "Supported config secret field: redis.url, notification.slack.webhook_url, notification.webhook.token, object_storage.s3.access_key_id, object_storage.s3.secret_access_key, storage_events.targets.<id>.secret_ref",
        )
}

fn vault_path_arg() -> Arg {
    Arg::new("vault-path")
        .long("vault-path")
        .value_name("PATH")
        .help("Vault secret path without the secret/ mount prefix")
}

fn field_arg() -> Arg {
    Arg::new("field")
        .long("field")
        .value_name("FIELD")
        .default_value("value")
        .help("Field name inside the vault secret map")
}

pub(crate) fn load_mode(args: &ArgMatches) -> LoadMode {
    match args.subcommand() {
        Some(("init", _)) => LoadMode::None,
        Some(("secret", secret_args)) => match secret_args.subcommand() {
            Some(("ref", _)) => LoadMode::None,
            Some(("set" | "rotate" | "check", _)) => LoadMode::VaultBootstrap,
            _ => LoadMode::ParsedConfig,
        },
        Some(("vault", _)) => LoadMode::VaultBootstrap,
        Some(("validate", _)) => LoadMode::RawSources,
        _ => LoadMode::ParsedConfig,
    }
}

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("init", init_args)) => exec_init(ctx, init_args),
        Some(("secret", secret_args)) => exec_secret(ctx, secret_args).await,
        Some(("vault", vault_args)) => exec_vault(ctx, vault_args).await,
        Some(("validate", validate_args)) => {
            let config_path = require_config_path(&ctx, "config validate")?;
            let config_profile_path = ctx.config_profile_path.clone();
            let config = load_config_for_validate(&config_path, config_profile_path.as_deref())?;
            init_log(&config.log);
            validate_config(
                &config,
                Some(&config_path),
                config_profile_path.as_deref(),
                validate_args.get_flag("resolve-secrets"),
                validate_args.get_flag("deny-warnings"),
                validate_args.get_flag("show-sources"),
                validate_args
                    .get_one::<String>("format")
                    .map(String::as_str)
                    .unwrap_or("human"),
            )
            .await?;
            println!("config valid");
            Ok(())
        }
        Some((cmd, _)) => Err(MegaError::Other(format!(
            "Unknown config subcommand: {cmd}"
        ))),
        None => Ok(()),
    }
}

fn exec_init(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let output_path = init_output_path(&ctx, args);
    let force = args.get_flag("force");
    write_init_config(&output_path, force)?;

    println!("created {}", output_path.display());
    println!("next steps:");
    println!(
        "  printf '%s' \"$S3_ACCESS_KEY\" | monoengine --config {} config secret set object_storage.s3.access_key_id --vault-path config/prod/object_storage/access_key_id --field value --value-stdin",
        output_path.display()
    );
    println!(
        "  printf '%s' \"$S3_SECRET_KEY\" | monoengine --config {} config secret set object_storage.s3.secret_access_key --vault-path config/prod/object_storage/secret_access_key --field value --value-stdin",
        output_path.display()
    );
    println!(
        "  monoengine --config {} config validate --resolve-secrets",
        output_path.display()
    );

    Ok(())
}

fn init_output_path(ctx: &CommandContext, args: &ArgMatches) -> PathBuf {
    args.get_one::<PathBuf>("output")
        .cloned()
        .or_else(|| ctx.config_path.clone())
        .unwrap_or_else(|| PathBuf::from("config/config.toml"))
}

fn write_init_config(output_path: &Path, force: bool) -> Result<(), MegaError> {
    if output_path.exists() && !force {
        return Err(MegaError::Other(format!(
            "{} already exists; pass --force to overwrite",
            output_path.display()
        )));
    }

    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let base_dir = crate::config::mega_base();
    let content = config_init_template(&base_dir);
    fs::write(output_path, content)?;

    Ok(())
}

async fn exec_vault(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("reset", reset_args)) => exec_vault_reset(ctx, reset_args).await,
        Some(("rekey", rekey_args)) => exec_vault_rekey(ctx, rekey_args).await,
        Some(("backup", backup_args)) => exec_vault_backup(ctx, backup_args).await,
        Some(("restore", restore_args)) => exec_vault_restore(ctx, restore_args).await,
        Some((cmd, _)) => Err(MegaError::Other(format!(
            "Unknown config vault subcommand: {cmd}"
        ))),
        None => Ok(()),
    }
}

async fn exec_vault_reset(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    if !args.get_flag("force") {
        return Err(MegaError::Other(
            "config vault reset is destructive; pass --force to confirm".to_string(),
        ));
    }

    let config_path = require_config_path(&ctx, "config vault reset")?;
    let config_profile_path = ctx.config_profile_path.as_deref();
    let key_path = args
        .get_one::<PathBuf>("key-path")
        .cloned()
        .unwrap_or_else(VaultCore::default_key_path);
    let config_path_str = config_path.to_str().ok_or_else(|| {
        MegaError::Other(format!(
            "Config path contains invalid UTF-8: {:?}",
            config_path
        ))
    })?;
    let config = Config::load_vault_bootstrap_with_profile(config_path_str, config_profile_path)?;

    let (_vault, backup_path) = VaultCore::reset(&config.database, key_path)
        .await
        .map_err(MegaError::from)?;

    if let Some(backup_path) = backup_path {
        println!(
            "vault reset complete; previous core key backed up to {}",
            backup_path.display()
        );
    } else {
        println!("vault reset complete; no previous core key was present");
    }
    Ok(())
}

async fn exec_vault_rekey(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    if !args.get_flag("force") {
        return Err(MegaError::Other(
            "config vault rekey regenerates unseal shares; pass --force to confirm".to_string(),
        ));
    }

    let config_path = require_config_path(&ctx, "config vault rekey")?;
    let config_profile_path = ctx.config_profile_path.as_deref();
    let key_path = args
        .get_one::<PathBuf>("key-path")
        .cloned()
        .unwrap_or_else(VaultCore::default_key_path);
    let config_path_str = config_path.to_str().ok_or_else(|| {
        MegaError::Other(format!(
            "Config path contains invalid UTF-8: {:?}",
            config_path
        ))
    })?;
    let config = Config::load_vault_bootstrap_with_profile(config_path_str, config_profile_path)?;

    let vault = VaultCore::from_database_config(&config.database, key_path.clone())
        .await
        .map_err(MegaError::from)?;
    vault
        .rekey_unseal_shares(&key_path)
        .await
        .map_err(MegaError::from)?;

    println!(
        "vault rekey complete; fresh unseal shares written to {}",
        key_path.display()
    );
    // The rekey primitive re-splits the current KEK; it does not rotate the
    // encryption key, so previously exported share sets for this key still
    // unseal the vault. Be explicit so operators do not assume the old shares
    // were invalidated.
    println!(
        "note: this re-splits the current encryption key; previously exported share sets for this key still unseal the vault. A full KEK rotation is required to invalidate old shares."
    );
    Ok(())
}

async fn exec_vault_backup(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    let config_path = require_config_path(&ctx, "config vault backup")?;
    let config_profile_path = ctx.config_profile_path.as_deref();
    let key_path = args
        .get_one::<PathBuf>("key-path")
        .cloned()
        .unwrap_or_else(VaultCore::default_key_path);
    let destination = args
        .get_one::<PathBuf>("destination")
        .cloned()
        .expect("destination is required");

    let config_path_str = config_path.to_str().ok_or_else(|| {
        MegaError::Other(format!(
            "Config path contains invalid UTF-8: {:?}",
            config_path
        ))
    })?;
    // Loading config lets us validate the path/profile even though backup only
    // needs the key file on disk.
    let _config = Config::load_vault_bootstrap_with_profile(config_path_str, config_profile_path)?;

    let backup_path = VaultCore::backup_key(&key_path, &destination).map_err(MegaError::from)?;
    println!(
        "vault core key backed up to {} (metadata at {}.meta.json)",
        backup_path.display(),
        backup_path.display()
    );
    Ok(())
}

async fn exec_vault_restore(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    if !args.get_flag("force") {
        return Err(MegaError::Other(
            "config vault restore overwrites core_key.json; pass --force to confirm".to_string(),
        ));
    }

    let config_path = require_config_path(&ctx, "config vault restore")?;
    let config_profile_path = ctx.config_profile_path.as_deref();
    let key_path = args
        .get_one::<PathBuf>("key-path")
        .cloned()
        .unwrap_or_else(VaultCore::default_key_path);
    let source = args
        .get_one::<PathBuf>("source")
        .cloned()
        .expect("source is required");

    let config_path_str = config_path.to_str().ok_or_else(|| {
        MegaError::Other(format!(
            "Config path contains invalid UTF-8: {:?}",
            config_path
        ))
    })?;
    let config = Config::load_vault_bootstrap_with_profile(config_path_str, config_profile_path)?;

    let restored_path = VaultCore::restore_key(&source, &key_path, &config.database)
        .await
        .map_err(MegaError::from)?;
    println!(
        "vault core key restored to {}; the backup successfully unlocked the vault",
        restored_path.display()
    );
    Ok(())
}

async fn exec_secret(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("ref", ref_args)) => {
            let secret_ref = secret_ref_from_args(ref_args)?;
            println!("{}", secret_ref.as_uri());
            Ok(())
        }
        Some(("set", set_args)) => {
            let config_path = require_config_path(&ctx, "config secret set")?;
            let config_profile_path = ctx.config_profile_path.as_deref();
            let name = required_string_arg(set_args, "name")?;
            ensure_supported_secret_field(name)?;
            let secret_ref = secret_ref_from_args(set_args)?;
            let value = read_secret_value_from_stdin()?;

            let vault = bootstrap_vault_from_path(&config_path, config_profile_path).await?;
            let mut data = Map::new();
            data.insert(secret_ref.field().to_string(), Value::String(value));
            with_audit_caller(
                "cli:config-secret-set",
                vault.write_secret(secret_ref.secret_name(), Some(data)),
            )
            .await?;

            println!("stored {}", secret_ref.as_uri());
            Ok(())
        }
        Some(("rotate", rotate_args)) => {
            let config_path = require_config_path(&ctx, "config secret rotate")?;
            let config_profile_path = ctx.config_profile_path.as_deref();
            let name = required_string_arg(rotate_args, "name")?;
            ensure_supported_secret_field(name)?;
            let secret_ref = secret_ref_from_args(rotate_args)?;
            let value = read_secret_value_from_stdin()?;

            let vault = bootstrap_vault_from_path(&config_path, config_profile_path).await?;
            let mut data = Map::new();
            data.insert(secret_ref.field().to_string(), Value::String(value));
            with_audit_caller(
                "cli:config-secret-rotate",
                vault.write_secret(secret_ref.secret_name(), Some(data)),
            )
            .await?;

            println!("rotated {}", secret_ref.as_uri());
            println!(
                "note: restart running services that consume this secret so they re-resolve it; new resolves and `config validate --resolve-secrets` use the rotated value immediately"
            );
            Ok(())
        }
        Some(("check", check_args)) => {
            let config_path = require_config_path(&ctx, "config secret check")?;
            let config_profile_path = ctx.config_profile_path.as_deref();
            let name = required_string_arg(check_args, "name")?;
            ensure_supported_secret_field(name)?;
            let secret_ref = if let Some(value) = check_args.get_one::<String>("ref") {
                let secret_ref = SecretRef::parse(value)?;
                validate_secret_field_ref(name, &secret_ref)?;
                secret_ref
            } else {
                secret_ref_from_args(check_args)?
            };

            let vault = bootstrap_vault_from_path(&config_path, config_profile_path).await?;
            let resolver = VaultSecretResolver::new(vault, Duration::ZERO);
            let resolved =
                with_audit_caller("cli:config-secret-check", resolver.resolve(&secret_ref)).await?;
            if name == "redis.url" {
                validate_redis_url_literal("redis.url", &resolved)?;
            }

            println!("ok {}", secret_ref.as_uri());
            Ok(())
        }
        Some((cmd, _)) => Err(MegaError::Other(format!(
            "Unknown config secret subcommand: {cmd}"
        ))),
        None => Ok(()),
    }
}

async fn validate_config(
    config: &Config,
    config_path: Option<&Path>,
    config_profile_path: Option<&Path>,
    resolve_secrets: bool,
    deny_warnings: bool,
    show_sources: bool,
    format: &str,
) -> Result<(), MegaError> {
    config.validate()?;
    let diagnostics = collect_source_diagnostics(config_path, config_profile_path)?;
    diagnostics.emit_warnings();
    if show_sources {
        if format == "json" {
            print_source_diagnostics_json(&diagnostics, config_path, config_profile_path);
        } else {
            print_source_diagnostics(&diagnostics);
        }
    }
    if deny_warnings && diagnostics.has_warnings() {
        return Err(MegaError::Other(format!(
            "config source diagnostics produced {} warning(s); fix the warnings or rerun without --deny-warnings",
            diagnostics.warning_count()
        )));
    }

    if resolve_secrets {
        let vault = bootstrap_vault(config).await?;
        let resolver = VaultSecretResolver::new(vault, Duration::ZERO);
        resolve_config_secrets(config, &resolver).await?;
    }

    Ok(())
}

fn print_source_diagnostics(diagnostics: &ConfigSourceDiagnostics) {
    for line in source_diagnostic_lines(diagnostics) {
        println!("{line}");
    }
}

fn source_diagnostic_lines(diagnostics: &ConfigSourceDiagnostics) -> Vec<String> {
    let mut lines = Vec::new();

    for warning in &diagnostics.file_warnings {
        lines.push(format!(
            "source warning: file {} field {}: {}",
            warning.source_path.display(),
            warning.field_path,
            warning.message
        ));
    }
    for warning in &diagnostics.environment_warnings {
        lines.push(format!(
            "source warning: environment variable {} maps to {}: {}",
            warning.variable, warning.field_path, warning.message
        ));
    }
    for source_field in &diagnostics.source_fields {
        lines.push(format!("source field: {}", source_field.message));
    }
    for source_override in &diagnostics.source_overrides {
        lines.push(format!("source override: {}", source_override.message));
    }

    lines
}

/// Print source diagnostics as JSON (frozen schema for `config validate
/// --show-sources --format json`). Includes the `cedar.enforcement` winning
/// source field (ADR-UN-01). Human-readable output is unchanged.
fn print_source_diagnostics_json(
    diagnostics: &ConfigSourceDiagnostics,
    config_path: Option<&Path>,
    config_profile_path: Option<&Path>,
) {
    let cedar_source = cedar_enforcement_winning_source(config_path, config_profile_path);
    let json = serde_json::json!({
        "valid": true,
        "cedar": {
            "enforcement": {
                "winning_source": cedar_source,
            }
        },
        "source_diagnostics": {
            "file_warnings": diagnostics.file_warnings.iter().map(|w| serde_json::json!({
                "source_path": w.source_path,
                "field_path": w.field_path,
                "message": w.message,
            })).collect::<Vec<_>>(),
            "environment_warnings": diagnostics.environment_warnings.iter().map(|w| serde_json::json!({
                "variable": w.variable,
                "field_path": w.field_path,
                "message": w.message,
            })).collect::<Vec<_>>(),
            "source_fields": diagnostics.source_fields.iter().map(|f| serde_json::json!({
                "field_path": f.field_path,
                "source": f.source,
                "message": f.message,
            })).collect::<Vec<_>>(),
            "source_overrides": diagnostics.source_overrides.iter().map(|o| serde_json::json!({
                "field_path": o.field_path,
                "source": o.source,
                "overridden_source": o.overridden_source,
                "message": o.message,
            })).collect::<Vec<_>>(),
        }
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string())
    );
}

/// Determine the winning source for `cedar.enforcement` (env > profile > base > default).
fn cedar_enforcement_winning_source(
    config_path: Option<&Path>,
    config_profile_path: Option<&Path>,
) -> String {
    if std::env::var("MEGA_CEDAR__ENFORCEMENT").is_ok() {
        return "environment variable MEGA_CEDAR__ENFORCEMENT".to_string();
    }
    if let Some(profile) = config_profile_path
        && toml_has_cedar_enforcement(profile)
    {
        return format!("profile file {}", profile.display());
    }
    if let Some(base) = config_path
        && toml_has_cedar_enforcement(base)
    {
        return format!("base file {}", base.display());
    }
    "default".to_string()
}

fn toml_has_cedar_enforcement(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        return false;
    };
    value
        .get("cedar")
        .and_then(|c| c.get("enforcement"))
        .is_some()
}

async fn resolve_config_secrets<R>(config: &Config, resolver: &R) -> Result<(), MegaError>
where
    R: SecretResolver + ?Sized,
{
    if let Some(notification_cfg) = &config.notification {
        if let Some(slack) = &notification_cfg.slack
            && slack.enabled
            && let Some(secret_ref) = &slack.webhook_url_ref
        {
            with_audit_caller("cli:config-validate", resolver.resolve(secret_ref)).await?;
        }
        if let Some(webhook) = &notification_cfg.webhook
            && webhook.enabled
            && let Some(secret_ref) = &webhook.token_ref
        {
            with_audit_caller("cli:config-validate", resolver.resolve(secret_ref)).await?;
        }
    }

    let redis_url_trimmed = config.redis.url.trim_start();
    if is_secret_ref_value(redis_url_trimmed) {
        let secret_ref = SecretRef::parse(redis_url_trimmed)?;
        let resolved =
            with_audit_caller("cli:config-validate", resolver.resolve(&secret_ref)).await?;
        validate_redis_url_literal("redis.url", &resolved)?;
    }

    if matches!(
        config.object_storage.storage_type,
        crate::orbit_api::factory::ObjectStorageBackend::S3
            | crate::orbit_api::factory::ObjectStorageBackend::S3Compatible
    ) {
        let access_key_id_trimmed = config.object_storage.s3.access_key_id.trim_start();
        if is_secret_ref_value(access_key_id_trimmed) {
            let secret_ref = SecretRef::parse(access_key_id_trimmed)?;
            with_audit_caller("cli:config-validate", resolver.resolve(&secret_ref)).await?;
        }
        let secret_access_key_trimmed = config.object_storage.s3.secret_access_key.trim_start();
        if is_secret_ref_value(secret_access_key_trimmed) {
            let secret_ref = SecretRef::parse(secret_access_key_trimmed)?;
            with_audit_caller("cli:config-validate", resolver.resolve(&secret_ref)).await?;
        }
    }

    Ok(())
}

fn load_config_for_validate(path: &Path, profile_path: Option<&Path>) -> Result<Config, MegaError> {
    let path = path.to_str().ok_or_else(|| {
        MegaError::Other(format!("Config path contains invalid UTF-8: {:?}", path))
    })?;

    Config::new_with_profile(path, profile_path)
}

async fn bootstrap_vault(config: &Config) -> Result<VaultCore, MegaError> {
    VaultCore::from_database_config(&config.database, VaultCore::default_key_path())
        .await
        .map_err(MegaError::from)
}

async fn bootstrap_vault_from_path(
    path: &Path,
    profile_path: Option<&Path>,
) -> Result<VaultCore, MegaError> {
    let path = path.to_str().ok_or_else(|| {
        MegaError::Other(format!("Config path contains invalid UTF-8: {:?}", path))
    })?;
    let config = Config::load_vault_bootstrap_with_profile(path, profile_path)?;

    VaultCore::from_database_config(&config.database, VaultCore::default_key_path())
        .await
        .map_err(MegaError::from)
}

fn secret_ref_from_args(args: &ArgMatches) -> Result<SecretRef, MegaError> {
    let name = required_string_arg(args, "name")?;
    ensure_supported_secret_field(name)?;
    let vault_path = args
        .get_one::<String>("vault-path")
        .ok_or_else(|| MegaError::Other("--vault-path is required".to_string()))?;
    let field = required_string_arg(args, "field")?;

    let secret_ref = SecretRef::from_parts(vault_path, field)?;
    validate_secret_field_ref(name, &secret_ref)?;
    Ok(secret_ref)
}

fn required_string_arg<'a>(args: &'a ArgMatches, name: &str) -> Result<&'a String, MegaError> {
    args.get_one::<String>(name).ok_or_else(|| {
        MegaError::Other(format!(
            "internal CLI wiring error: missing `{name}` argument"
        ))
    })
}

fn ensure_supported_secret_field(name: &str) -> Result<(), MegaError> {
    if supported_secret_suffix(name).is_some() {
        return Ok(());
    }

    Err(unsupported_secret_field_error(name))
}

fn read_secret_value_from_stdin() -> Result<String, MegaError> {
    let mut value = String::new();
    io::stdin().read_to_string(&mut value)?;
    if value.ends_with("\r\n") {
        value.truncate(value.len() - 2);
    } else if value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }

    if value.is_empty() {
        return Err(MegaError::Other(
            "secret value read from stdin must not be empty".to_string(),
        ));
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing::{TestSecretResolver, env_lock, isolated_config};
    #[rustfmt::skip]
    use crate::orbit_api::factory::{ObjectStorageBackend, ObjectStorageConfig, S3Config};

    #[test]
    fn config_init_uses_no_config_load_mode() {
        let matches = cli().try_get_matches_from(["config", "init"]).unwrap();

        assert_eq!(load_mode(&matches), LoadMode::None);
    }

    #[test]
    fn config_vault_reset_uses_vault_bootstrap_load_mode() {
        let matches = cli()
            .try_get_matches_from(["config", "vault", "reset", "--force"])
            .unwrap();

        assert_eq!(load_mode(&matches), LoadMode::VaultBootstrap);
    }

    #[test]
    fn config_vault_reset_requires_force() {
        let matches = cli()
            .try_get_matches_from(["config", "vault", "reset", "--force"])
            .unwrap();
        let Some(("vault", vault_args)) = matches.subcommand() else {
            panic!("vault subcommand should parse");
        };
        let Some(("reset", reset_args)) = vault_args.subcommand() else {
            panic!("reset subcommand should parse");
        };

        assert!(reset_args.get_flag("force"));
    }

    #[test]
    fn config_vault_rekey_uses_vault_bootstrap_load_mode() {
        let matches = cli()
            .try_get_matches_from(["config", "vault", "rekey", "--force"])
            .unwrap();

        assert_eq!(load_mode(&matches), LoadMode::VaultBootstrap);
    }

    #[test]
    fn config_vault_rekey_requires_force() {
        // `--force` is a required flag, so omitting it must fail to parse.
        let err = cli()
            .try_get_matches_from(["config", "vault", "rekey"])
            .expect_err("rekey without --force should not parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn config_vault_rekey_accepts_key_path() {
        let matches = cli()
            .try_get_matches_from([
                "config",
                "vault",
                "rekey",
                "--force",
                "--key-path",
                "/tmp/core_key.json",
            ])
            .unwrap();
        let Some(("vault", vault_args)) = matches.subcommand() else {
            panic!("vault subcommand should parse");
        };
        let Some(("rekey", rekey_args)) = vault_args.subcommand() else {
            panic!("rekey subcommand should parse");
        };

        assert!(rekey_args.get_flag("force"));
        assert_eq!(
            rekey_args.get_one::<PathBuf>("key-path"),
            Some(&PathBuf::from("/tmp/core_key.json"))
        );
    }

    #[test]
    fn config_vault_backup_uses_vault_bootstrap_load_mode() {
        let matches = cli()
            .try_get_matches_from(["config", "vault", "backup", "/tmp/vault-backup"])
            .unwrap();

        assert_eq!(load_mode(&matches), LoadMode::VaultBootstrap);
    }

    #[test]
    fn config_vault_backup_accepts_key_path() {
        let matches = cli()
            .try_get_matches_from([
                "config",
                "vault",
                "backup",
                "/tmp/vault-backup",
                "--key-path",
                "/tmp/core_key.json",
            ])
            .unwrap();
        let Some(("vault", vault_args)) = matches.subcommand() else {
            panic!("vault subcommand should parse");
        };
        let Some(("backup", backup_args)) = vault_args.subcommand() else {
            panic!("backup subcommand should parse");
        };

        assert_eq!(
            backup_args.get_one::<PathBuf>("destination"),
            Some(&PathBuf::from("/tmp/vault-backup"))
        );
        assert_eq!(
            backup_args.get_one::<PathBuf>("key-path"),
            Some(&PathBuf::from("/tmp/core_key.json"))
        );
    }

    #[test]
    fn config_vault_restore_uses_vault_bootstrap_load_mode() {
        let matches = cli()
            .try_get_matches_from(["config", "vault", "restore", "/tmp/vault-backup", "--force"])
            .unwrap();

        assert_eq!(load_mode(&matches), LoadMode::VaultBootstrap);
    }

    #[test]
    fn config_vault_restore_requires_force() {
        let err = cli()
            .try_get_matches_from(["config", "vault", "restore", "/tmp/vault-backup"])
            .expect_err("restore without --force should not parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn config_secret_rotate_rejects_unsupported_field() {
        let matches = secret_rotate_cli()
            .try_get_matches_from([
                "rotate",
                "database.password",
                "--vault-path",
                "config/prod/database/password",
                "--value-stdin",
            ])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("unsupported secret");
        let message = err.to_string();
        assert!(message.contains("cannot be stored in monoengine vault"));
        assert!(message.contains("supported fields are"));
    }

    #[test]
    fn config_validate_uses_raw_sources_load_mode() {
        let matches = cli().try_get_matches_from(["config", "validate"]).unwrap();

        assert_eq!(load_mode(&matches), LoadMode::RawSources);
    }

    #[test]
    fn config_validate_accepts_deny_warnings_flag() {
        let matches = cli()
            .try_get_matches_from(["config", "validate", "--deny-warnings", "--show-sources"])
            .unwrap();
        let Some(("validate", validate_args)) = matches.subcommand() else {
            panic!("validate subcommand should parse");
        };

        assert_eq!(load_mode(&matches), LoadMode::RawSources);
        assert!(validate_args.get_flag("deny-warnings"));
        assert!(validate_args.get_flag("show-sources"));
    }

    #[test]
    fn config_init_writes_safe_skeleton() {
        let _lock = env_lock();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");

        write_init_config(&config_path, false).expect("init config should write");
        let content = fs::read_to_string(&config_path).expect("init config should be readable");
        let config = Config::load_str(&content).expect("init config should parse");
        config.validate().expect("init config should validate");

        assert!(!content.contains("postgres://mono:mono@"));
        assert!(!content.contains("postgres://mega:mega@"));
        assert!(!content.contains("postgres://postgres:postgres@"));
        assert!(!content.contains("password = "));

        let err = write_init_config(&config_path, false).expect_err("existing file should fail");
        assert!(err.to_string().contains("--force"));

        write_init_config(&config_path, true).expect("force should overwrite");
    }

    #[test]
    fn secret_ref_from_args_rejects_unsupported_bootstrap_secret() {
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "database.password",
                "--vault-path",
                "config/prod/database/password",
            ])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("unsupported secret");
        let message = err.to_string();
        assert!(message.contains("cannot be stored in monoengine vault"));
        assert!(message.contains("supported fields are"));
    }

    #[test]
    fn secret_ref_from_args_accepts_notification_slack_webhook_url_namespace() {
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "notification.slack.webhook_url",
                "--vault-path",
                "config/prod/notification/slack/webhook_url",
            ])
            .unwrap();

        let secret_ref =
            secret_ref_from_args(&matches).expect("slack webhook_url ref should be accepted");
        assert_eq!(
            secret_ref.secret_name(),
            "config/prod/notification/slack/webhook_url"
        );
    }

    #[test]
    fn secret_ref_from_args_rejects_notification_secret_outside_namespace() {
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "notification.webhook.token",
                "--vault-path",
                "config/prod/other/path",
            ])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("wrong namespace");
        let message = err.to_string();
        assert!(message.contains("notification.webhook.token"));
        assert!(message.contains("notification/webhook/token"));
        assert!(!message.contains("config/prod/other/path"));
    }

    #[test]
    fn secret_ref_from_args_accepts_redis_url_namespace() {
        let matches = secret_ref_cli()
            .try_get_matches_from(["ref", "redis.url", "--vault-path", "config/prod/redis/url"])
            .unwrap();

        let secret_ref = secret_ref_from_args(&matches).expect("redis.url ref should be accepted");
        assert_eq!(secret_ref.secret_name(), "config/prod/redis/url");
    }

    #[test]
    fn secret_ref_from_args_accepts_storage_events_target_namespace() {
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "storage_events.targets.ops-main.secret_ref",
                "--vault-path",
                "config/prod/storage_events/targets/ops-main/hmac",
            ])
            .unwrap();

        let secret_ref =
            secret_ref_from_args(&matches).expect("storage_events target ref should be accepted");
        assert_eq!(
            secret_ref.secret_name(),
            "config/prod/storage_events/targets/ops-main/hmac"
        );

        // The namespace is pinned to the id in the field name.
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "storage_events.targets.ops-main.secret_ref",
                "--vault-path",
                "config/prod/storage_events/targets/other/hmac",
            ])
            .unwrap();
        let err = secret_ref_from_args(&matches).expect_err("id mismatch must fail");
        let message = err.to_string();
        assert!(message.contains("storage_events.targets.ops-main.secret_ref"));
        assert!(!message.contains("targets/other"), "{message}");

        // Malformed target ids stay unsupported fields.
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "storage_events.targets.ops/main.secret_ref",
                "--vault-path",
                "config/prod/storage_events/targets/ops/main/hmac",
            ])
            .unwrap();
        let err = secret_ref_from_args(&matches).expect_err("invalid id must fail");
        assert!(
            err.to_string()
                .contains("cannot be stored in monoengine vault")
        );
    }

    #[test]
    fn secret_ref_from_args_rejects_redis_url_outside_namespace() {
        let matches = secret_ref_cli()
            .try_get_matches_from(["ref", "redis.url", "--vault-path", "config/prod/other/path"])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("wrong namespace");
        let message = err.to_string();
        assert!(message.contains("redis.url"));
        assert!(message.contains("redis/url"));
        assert!(!message.contains("config/prod/other/path"));
    }

    #[test]
    fn secret_ref_from_args_accepts_object_storage_access_key_id_namespace() {
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "object_storage.s3.access_key_id",
                "--vault-path",
                "config/prod/object_storage/access_key_id",
            ])
            .unwrap();

        let secret_ref = secret_ref_from_args(&matches)
            .expect("object storage access key ref should be accepted");
        assert_eq!(
            secret_ref.secret_name(),
            "config/prod/object_storage/access_key_id"
        );
    }

    #[test]
    fn secret_ref_from_args_rejects_object_storage_secret_access_key_outside_namespace() {
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "object_storage.s3.secret_access_key",
                "--vault-path",
                "config/prod/other/path",
            ])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("wrong namespace");
        let message = err.to_string();
        assert!(message.contains("object_storage.s3.secret_access_key"));
        assert!(message.contains("object_storage/secret_access_key"));
        assert!(!message.contains("config/prod/other/path"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn validate_config_denies_source_warnings_when_requested() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [obsolete]
            smtp_tls = false
            "#,
        )
        .expect("write config source");
        let config = isolated_config(temp_dir.path().join("base"));

        validate_config(
            &config,
            Some(&config_path),
            None,
            false,
            false,
            false,
            "human",
        )
        .await
        .expect("warnings should not fail by default");

        let err = validate_config(
            &config,
            Some(&config_path),
            None,
            false,
            true,
            false,
            "human",
        )
        .await
        .expect_err("deny warnings should fail");

        assert!(err.to_string().contains("source diagnostics produced"));
        assert!(err.to_string().contains("--deny-warnings"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn validate_config_rejects_obsolete_section_source() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
            [obsolete]
            {} = "plain-text-password"
            "#,
                "password"
            ),
        )
        .expect("write config source");
        let config = isolated_config(temp_dir.path().join("base"));

        let err = validate_config(
            &config,
            Some(&config_path),
            None,
            false,
            true,
            false,
            "human",
        )
        .await
        .expect_err("obsolete section should fail under deny warnings");
        let message = err.to_string();

        assert!(message.contains("source diagnostics produced"));
        assert!(!message.contains("plain-text-password"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_config_secrets_resolves_object_storage_s3_secret_refs() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let access_key_ref =
            SecretRef::parse("vault://secret/config/test/object_storage/access_key_id#value")
                .unwrap();
        let secret_key_ref =
            SecretRef::parse("vault://secret/config/test/object_storage/secret_access_key#value")
                .unwrap();

        let mut config = isolated_config(temp_dir.path().join("base"));
        config.object_storage = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::S3,
            s3: S3Config {
                region: "us-east-1".to_string(),
                bucket: "monoengine-test".to_string(),
                access_key_id: access_key_ref.as_uri().to_string(),
                secret_access_key: secret_key_ref.as_uri().to_string(),
                endpoint_url: String::new(),
            },
            ..Default::default()
        };

        let resolver = TestSecretResolver::new()
            .with_secret(&access_key_ref, "AKIA-test")
            .expect("access key should insert")
            .with_secret(&secret_key_ref, "secret-test")
            .expect("secret key should insert");

        resolve_config_secrets(&config, &resolver)
            .await
            .expect("object storage S3 SecretRefs should resolve");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_config_secrets_resolves_redis_url_secret_ref() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let redis_url_ref = SecretRef::parse("vault://secret/config/test/redis/url#value").unwrap();

        let mut config = isolated_config(temp_dir.path().join("base"));
        config.redis.url = redis_url_ref.as_uri().to_string();

        let resolver = TestSecretResolver::new()
            .with_secret(&redis_url_ref, "redis://vault-backed:6379")
            .expect("redis url secret should insert");

        resolve_config_secrets(&config, &resolver)
            .await
            .expect("redis.url SecretRef should resolve");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_config_secrets_reports_missing_redis_url_ref_without_leaking_ref() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let redis_url_ref = SecretRef::parse("vault://secret/config/test/redis/url#value").unwrap();

        let mut config = isolated_config(temp_dir.path().join("base"));
        config.redis.url = redis_url_ref.as_uri().to_string();

        let resolver = TestSecretResolver::new();

        let err = resolve_config_secrets(&config, &resolver)
            .await
            .expect_err("missing redis.url SecretRef should fail");
        let message = err.to_string();

        assert!(message.contains("test secret not found"));
        assert!(!message.contains("config/test/redis/url"));
        assert!(!message.contains("#value"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_config_secrets_rejects_malformed_resolved_redis_url() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let redis_url_ref = SecretRef::parse("vault://secret/config/test/redis/url#value").unwrap();

        let mut config = isolated_config(temp_dir.path().join("base"));
        config.redis.url = redis_url_ref.as_uri().to_string();

        let resolver = TestSecretResolver::new()
            .with_secret(&redis_url_ref, "http://not-a-redis-url:6379")
            .expect("redis url secret should insert");

        let err = resolve_config_secrets(&config, &resolver)
            .await
            .expect_err("resolved redis.url with non-redis scheme should fail");
        let message = err.to_string();

        assert!(message.contains("redis.url scheme"));
        assert!(!message.contains("http://not-a-redis-url:6379"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_config_secrets_redacted_error_does_not_leak_secret_like_scheme() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let redis_url_ref = SecretRef::parse("vault://secret/config/test/redis/url#value").unwrap();

        let mut config = isolated_config(temp_dir.path().join("base"));
        config.redis.url = redis_url_ref.as_uri().to_string();

        let resolver = TestSecretResolver::new()
            .with_secret(&redis_url_ref, "mysecret://sensitive-host:6379")
            .expect("redis url secret should insert");

        let err = resolve_config_secrets(&config, &resolver)
            .await
            .expect_err("resolved redis.url with secret-like scheme should fail");
        let message = err.to_string();

        assert!(message.contains("redis.url scheme"));
        assert!(!message.contains("mysecret"));
        assert!(!message.contains("sensitive-host"));
    }

    #[test]
    fn cedar_enforcement_winning_source_detects_base_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "[cedar]\nenforcement = \"off\"\n").expect("write config");
        let source = cedar_enforcement_winning_source(Some(&path), None);
        assert!(source.contains("base file"), "got: {source}");
    }

    #[test]
    fn cedar_enforcement_winning_source_defaults_when_absent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "[log]\nlevel = \"info\"\n").expect("write config");
        let source = cedar_enforcement_winning_source(Some(&path), None);
        assert_eq!(source, "default");
    }
}
