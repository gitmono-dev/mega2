use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Map, Value};

use crate::{
    commands::{CommandContext, LoadMode, require_config, require_config_path},
    common::errors::{MegaError, MegaResult},
    config::{
        Config,
        secret::{SecretRef, SecretResolver, VaultSecretResolver},
        template::config_init_template,
        validate::{warn_known_unconsumed_file_fields, warn_unconsumed_environment_fields},
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
            Command::new("validate").about("Validate configuration").arg(
                Arg::new("resolve-secrets")
                    .long("resolve-secrets")
                    .action(ArgAction::SetTrue)
                    .help("Resolve configured SecretRef values through the minimal DB/Vault bootstrap"),
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
            Some(("set" | "check", _)) => LoadMode::VaultBootstrap,
            _ => LoadMode::ParsedConfig,
        },
        Some(("validate", _)) => LoadMode::ParsedConfig,
        _ => LoadMode::ParsedConfig,
    }
}

#[tokio::main]
pub(crate) async fn exec(ctx: CommandContext, args: &ArgMatches) -> MegaResult {
    match args.subcommand() {
        Some(("init", init_args)) => exec_init(ctx, init_args),
        Some(("secret", secret_args)) => exec_secret(ctx, secret_args).await,
        Some(("validate", validate_args)) => {
            let config_path = ctx.config_path.clone();
            let config_profile_path = ctx.config_profile_path.clone();
            let config = require_config(ctx, "config validate")?;
            validate_config(
                &config,
                config_path.as_deref(),
                config_profile_path.as_deref(),
                validate_args.get_flag("resolve-secrets"),
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
            let name = set_args
                .get_one::<String>("name")
                .expect("required by clap");
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
        Some(("check", check_args)) => {
            let config_path = require_config_path(&ctx, "config secret check")?;
            let config_profile_path = ctx.config_profile_path.as_deref();
            let name = check_args
                .get_one::<String>("name")
                .expect("required by clap");
            ensure_supported_secret_field(name)?;
            let secret_ref = if let Some(value) = check_args.get_one::<String>("ref") {
                SecretRef::parse(value)?
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
) -> Result<(), MegaError> {
    config.validate()?;
    if let Some(config_path) = config_path {
        warn_known_unconsumed_file_fields(config_path)?;
    }
    if let Some(config_profile_path) = config_profile_path {
        warn_known_unconsumed_file_fields(config_profile_path)?;
    }
    warn_unconsumed_environment_fields();

    if let Some(mail_cfg) = &config.mail {
        mail_cfg.warn_plaintext_password_deprecated();

        if resolve_secrets && let Some(secret_ref) = &mail_cfg.password_ref {
            let vault = bootstrap_vault(config).await?;
            let resolver = VaultSecretResolver::new(vault, Duration::ZERO);
            resolver.resolve(secret_ref).await?;
        }
    }

    Ok(())
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
    let name = args.get_one::<String>("name").expect("required by clap");
    ensure_supported_secret_field(name)?;
    let vault_path = args
        .get_one::<String>("vault-path")
        .ok_or_else(|| MegaError::Other("--vault-path is required".to_string()))?;
    let field = args.get_one::<String>("field").expect("defaulted by clap");

    SecretRef::from_parts(vault_path, field)
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
    fn config_init_writes_safe_skeleton() {
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn validate_rejects_mail_password_and_password_ref_together() {
        let config = Config {
            mail: Some(crate::config::MailConfig {
                enabled: false,
                smtp_host: "smtp.example.com".to_string(),
                smtp_port: 587,
                username: None,
                password: Some(crate::config::secret::SecretString::new("plain")),
                password_ref: Some(
                    SecretRef::parse("vault://secret/config/test/mail/password#value").unwrap(),
                ),
                from: "no-reply@example.com".to_string(),
                starttls: true,
            }),
            ..Config::mock()
        };

        let err = validate_config(&config, None, None, false)
            .await
            .expect_err("mutual exclusion should fail");
        assert!(err.to_string().contains("mutually exclusive"));
    }
}
