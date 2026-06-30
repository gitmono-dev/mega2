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

const MAIL_PASSWORD_FIELD: &str = "mail.password";

/// Config-managed secret fields that may be stored in the monoengine vault, with
/// the vault namespace suffix each must use (`config/<profile>/<suffix>`). Only
/// these fields are accepted by `config secret set/check/rotate/ref`. Database
/// credentials remain intentionally excluded — they are a bootstrap dependency
/// that must stay in deployment/environment secrets. Redis URLs and
/// object-storage S3 credentials may now be vault-backed and are resolved
/// post-vault bootstrap.
const SUPPORTED_SECRET_FIELDS: &[(&str, &str)] = &[
    (MAIL_PASSWORD_FIELD, "mail/password"),
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

/// Look up the required vault namespace suffix for a supported secret field.
fn supported_secret_suffix(name: &str) -> Option<&'static str> {
    SUPPORTED_SECRET_FIELDS
        .iter()
        .find(|(field, _)| *field == name)
        .map(|(_, suffix)| *suffix)
}

/// Validate that `secret_ref` uses the namespace required for the named field.
fn validate_secret_field_ref(name: &str, secret_ref: &SecretRef) -> Result<(), MegaError> {
    match supported_secret_suffix(name) {
        Some(suffix) => validate_config_secret_ref(name, secret_ref, suffix),
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
        "{name} cannot be stored in monoengine vault; supported fields are: {supported}. Database credentials must stay in deployment/environment secrets."
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
            "Supported config secret field: mail.password, redis.url, notification.slack.webhook_url, notification.webhook.token, object_storage.s3.access_key_id, object_storage.s3.secret_access_key",
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
        "  printf '%s' \"$SMTP_PASSWORD\" | monoengine --config {} config secret set mail.password --vault-path config/prod/mail/password --field value --value-stdin",
        output_path.display()
    );
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
    // The vendored libvault primitive re-splits the current KEK; it does not
    // rotate the encryption key, so previously exported share sets for this key
    // still unseal the vault. Be explicit so operators do not assume the old
    // shares were invalidated.
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
            // The mailer resolves mail.password_ref once at AppContext startup, so a
            // running service keeps using the previous value until it is restarted.
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
) -> Result<(), MegaError> {
    config.validate()?;
    let diagnostics = collect_source_diagnostics(config_path, config_profile_path)?;
    diagnostics.emit_warnings();
    if show_sources {
        print_source_diagnostics(&diagnostics);
    }
    if deny_warnings && diagnostics.has_warnings() {
        return Err(MegaError::Other(format!(
            "config source diagnostics produced {} warning(s); fix the warnings or rerun without --deny-warnings",
            diagnostics.warning_count()
        )));
    }

    if let Some(mail_cfg) = &config.mail {
        mail_cfg.warn_plaintext_password_deprecated();
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

async fn resolve_config_secrets<R>(config: &Config, resolver: &R) -> Result<(), MegaError>
where
    R: SecretResolver + ?Sized,
{
    if let Some(mail_cfg) = &config.mail
        && let Some(secret_ref) = &mail_cfg.password_ref
    {
        with_audit_caller("cli:config-validate", resolver.resolve(secret_ref)).await?;
    }

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
        orbit_api::factory::ObjectStorageBackend::S3
            | orbit_api::factory::ObjectStorageBackend::S3Compatible
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
    use orbit_api::factory::{ObjectStorageBackend, ObjectStorageConfig, S3Config};

    use super::*;
    use crate::config::{
        MailConfig,
        testing::{TestSecretResolver, env_lock, isolated_config},
        validate::collect_source_diagnostics_from_keys,
    };

    #[test]
    fn config_secret_ref_uses_no_config_load_mode() {
        let matches = cli()
            .try_get_matches_from([
                "config",
                "secret",
                "ref",
                "mail.password",
                "--vault-path",
                "config/prod/mail/password",
            ])
            .unwrap();
        let Some(("secret", _)) = matches.subcommand() else {
            panic!("secret subcommand should parse");
        };

        assert_eq!(load_mode(&matches), LoadMode::None);
    }

    #[test]
    fn config_init_uses_no_config_load_mode() {
        let matches = cli().try_get_matches_from(["config", "init"]).unwrap();

        assert_eq!(load_mode(&matches), LoadMode::None);
    }

    #[test]
    fn config_secret_rotate_uses_vault_bootstrap_load_mode() {
        let matches = cli()
            .try_get_matches_from([
                "config",
                "secret",
                "rotate",
                "mail.password",
                "--vault-path",
                "config/prod/mail/password",
                "--value-stdin",
            ])
            .unwrap();
        let Some(("secret", _)) = matches.subcommand() else {
            panic!("secret subcommand should parse");
        };

        assert_eq!(load_mode(&matches), LoadMode::VaultBootstrap);
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
        assert!(content.contains("password_ref = "));

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
                "config/prod/mail/password",
            ])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("wrong namespace");
        let message = err.to_string();
        assert!(message.contains("notification.webhook.token"));
        assert!(message.contains("notification/webhook/token"));
        assert!(!message.contains("config/prod/mail/password"));
    }

    #[test]
    fn secret_ref_from_args_rejects_wrong_mail_namespace_without_leaking_path() {
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "mail.password",
                "--vault-path",
                "config/prod/database/password",
            ])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("wrong namespace");
        let message = err.to_string();

        assert!(message.contains("mail.password"));
        assert!(message.contains("vault://secret/config/<profile>/mail/password#<field>"));
        assert!(message.contains("value is redacted"));
        assert!(!message.contains("config/prod/database/password"));
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
    fn secret_ref_from_args_rejects_redis_url_outside_namespace() {
        let matches = secret_ref_cli()
            .try_get_matches_from([
                "ref",
                "redis.url",
                "--vault-path",
                "config/prod/mail/password",
            ])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("wrong namespace");
        let message = err.to_string();
        assert!(message.contains("redis.url"));
        assert!(message.contains("redis/url"));
        assert!(!message.contains("config/prod/mail/password"));
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
                "config/prod/mail/password",
            ])
            .unwrap();

        let err = secret_ref_from_args(&matches).expect_err("wrong namespace");
        let message = err.to_string();
        assert!(message.contains("object_storage.s3.secret_access_key"));
        assert!(message.contains("object_storage/secret_access_key"));
        assert!(!message.contains("config/prod/mail/password"));
    }

    #[test]
    fn config_secret_check_ref_rejects_wrong_mail_namespace_without_leaking_ref() {
        let matches = cli()
            .try_get_matches_from([
                "config",
                "secret",
                "check",
                "mail.password",
                "--ref",
                "vault://secret/config/prod/database/password#value",
            ])
            .unwrap();
        let Some(("secret", secret_args)) = matches.subcommand() else {
            panic!("secret subcommand should parse");
        };
        let Some(("check", check_args)) = secret_args.subcommand() else {
            panic!("secret check subcommand should parse");
        };
        let name = check_args
            .get_one::<String>("name")
            .expect("required by clap");
        let secret_ref = check_args
            .get_one::<String>("ref")
            .map(SecretRef::parse)
            .expect("--ref should exist")
            .expect("SecretRef should parse");

        let err = validate_secret_field_ref(name, &secret_ref).expect_err("wrong namespace");
        let message = err.to_string();

        assert!(message.contains("mail.password"));
        assert!(message.contains("vault://secret/config/<profile>/mail/password#<field>"));
        assert!(message.contains("value is redacted"));
        assert!(!message.contains("config/prod/database/password"));
        assert!(!message.contains("#value"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn validate_rejects_mail_password_and_password_ref_together() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.mail = Some(MailConfig {
            enabled: false,
            provider: crate::config::MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: Some(crate::config::secret::SecretString::new("plain")),
            password_ref: Some(
                SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap(),
            ),
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        });

        let err = validate_config(&config, None, None, false, false, false)
            .await
            .expect_err("mutual exclusion should fail");
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn validate_config_denies_source_warnings_when_requested() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [mail]
            smtp_tls = false
            "#,
        )
        .expect("write config source");
        let config = isolated_config(temp_dir.path().join("base"));

        validate_config(&config, Some(&config_path), None, false, false, false)
            .await
            .expect("warnings should not fail by default");

        let err = validate_config(&config, Some(&config_path), None, false, true, false)
            .await
            .expect_err("deny warnings should fail");

        assert!(err.to_string().contains("source diagnostics produced"));
        assert!(err.to_string().contains("--deny-warnings"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn validate_config_denies_deprecated_mail_password_source_without_leaking_value() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
            [mail]
            {} = "plain-text-password"
            "#,
                "password"
            ),
        )
        .expect("write config source");
        let config = isolated_config(temp_dir.path().join("base"));

        let err = validate_config(&config, Some(&config_path), None, false, true, false)
            .await
            .expect_err("deprecated mail password should fail under deny warnings");
        let message = err.to_string();

        assert!(message.contains("source diagnostics produced"));
        assert!(!message.contains("plain-text-password"));
    }

    #[test]
    fn show_sources_lines_include_source_warnings_without_values() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("config.toml");
        let profile_path = temp_dir.path().join("config.prod.toml");
        fs::write(
            &config_path,
            r#"
            [log]
            level = "info"

            [database]
            db_url = "postgres://localhost:5432/base"

            [monorepo]
            root_dirs = ["base-root"]
            "#,
        )
        .expect("write config source");
        fs::write(
            &profile_path,
            format!(
                r#"
            [log]
            level = "debug"

            [monorepo]
            root_dirs = ["profile-root"]

            [mail]
            {} = "plain-text-password"
            "#,
                "password"
            ),
        )
        .expect("write profile source");

        let diagnostics = collect_source_diagnostics_from_keys(
            Some(&config_path),
            Some(&profile_path),
            [
                "MEGA_DATABASE__DB_URL",
                "MEGA_MAIL__PASSWORD",
                "MEGA_UNKNOWN__VALUE",
            ],
        )
        .expect("diagnostics should collect");
        let output = source_diagnostic_lines(&diagnostics).join("\n");

        assert!(output.contains("source warning: file"));
        assert!(output.contains(&profile_path.display().to_string()));
        assert!(output.contains("mail.password"));
        assert!(output.contains("mail.password_ref"));
        assert!(output.contains("source warning: environment variable MEGA_MAIL__PASSWORD"));
        assert!(output.contains("source warning: environment variable MEGA_UNKNOWN__VALUE"));
        assert!(output.contains("source field:"));
        assert!(output.contains("source override:"));
        assert!(output.contains("suggested fix"));
        assert!(output.contains("values are omitted"));
        assert!(output.contains("sensitive values are omitted"));
        assert!(output.contains("deployment/environment secrets"));
        assert!(output.contains("arrays replace lower-precedence values rather than append"));
        assert!(output.contains("unset MEGA_MAIL__PASSWORD"));
        assert!(!output.contains("plain-text-password"));
        assert!(!output.contains("postgres://localhost"));
        assert!(!output.contains("debug"));
        assert!(!output.contains("info"));
        assert!(!output.contains("base-root"));
        assert!(!output.contains("profile-root"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_config_secrets_reports_missing_mail_password_ref_without_leaking_ref() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let secret_ref =
            SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap();
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.mail = Some(MailConfig {
            enabled: false,
            provider: crate::config::MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: Some(secret_ref),
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        });
        let resolver = TestSecretResolver::new();

        let err = resolve_config_secrets(&config, &resolver)
            .await
            .expect_err("missing SecretRef should fail");
        let message = err.to_string();

        assert!(message.contains("test secret not found"));
        assert!(message.contains("vault://secret/***#***"));
        assert!(!message.contains("config/test/mail/password"));
        assert!(!message.contains("#value"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolve_config_secrets_reports_permission_denied_without_leaking_ref() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let secret_ref =
            SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap();
        let mut config = isolated_config(temp_dir.path().join("base"));
        config.mail = Some(MailConfig {
            enabled: false,
            provider: crate::config::MailProvider::Smtp,
            smtp_host: "smtp.example.com".to_string(),
            smtp_port: 587,
            username: None,
            password: None,
            password_ref: Some(secret_ref.clone()),
            from: "no-reply@example.com".to_string(),
            starttls: true,
            ..Default::default()
        });
        let resolver = TestSecretResolver::new()
            .with_secret(&secret_ref, "smtp-test-value")
            .expect("secret should insert")
            .with_denied_secret(&secret_ref)
            .expect("secret should be denied");

        let err = resolve_config_secrets(&config, &resolver)
            .await
            .expect_err("denied SecretRef should fail");
        let message = err.to_string();

        assert!(message.contains("test secret access denied"));
        assert!(message.contains("vault://secret/***#***"));
        assert!(!message.contains("config/test/mail/password"));
        assert!(!message.contains("#value"));
        assert!(!message.contains("smtp-test-value"));
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
}
