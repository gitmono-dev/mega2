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
        secret::{SecretRef, SecretResolver, VaultSecretResolver},
        template::config_init_template,
        validate::{
            ConfigSourceDiagnostics, collect_source_diagnostics, validate_mail_password_secret_ref,
        },
    },
    contract::vault::integration::vault_core::{VaultCore, VaultCoreInterface},
};

const MAIL_PASSWORD_FIELD: &str = "mail.password";

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

fn secret_name_arg() -> Arg {
    Arg::new("name")
        .value_name("CONFIG_FIELD")
        .required(true)
        .help("Supported config secret field, currently mail.password")
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
        Some(("validate", _)) => LoadMode::RawSources,
        _ => LoadMode::ParsedConfig,
    }
}

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("init", init_args)) => exec_init(ctx, init_args),
        Some(("secret", secret_args)) => exec_secret(ctx, secret_args).await,
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
            vault
                .write_secret(secret_ref.secret_name(), Some(data))
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
            vault
                .write_secret(secret_ref.secret_name(), Some(data))
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
                validate_mail_password_secret_ref(name, &secret_ref)?;
                secret_ref
            } else {
                secret_ref_from_args(check_args)?
            };

            let vault = bootstrap_vault_from_path(&config_path, config_profile_path).await?;
            let resolver = VaultSecretResolver::new(vault, Duration::ZERO);
            resolver.resolve(&secret_ref).await?;

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
        resolver.resolve(secret_ref).await?;
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
    validate_mail_password_secret_ref(name, &secret_ref)?;
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
    if name == MAIL_PASSWORD_FIELD {
        return Ok(());
    }

    Err(MegaError::Other(format!(
        "{name} cannot be stored in monoengine vault; only {MAIL_PASSWORD_FIELD} is currently supported. Database, Redis, and object storage credentials must stay in deployment/environment secrets."
    )))
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
        assert!(err.to_string().contains("only mail.password"));
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
        assert!(err.to_string().contains("only mail.password"));
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

        let err =
            validate_mail_password_secret_ref(name, &secret_ref).expect_err("wrong namespace");
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
}
