use std::{fs, path::Path};

use c::{ConfigError, FileFormat};
use toml::Value;

use super::expand::variable_placeholder_substitute;
use crate::config::{c, validate::reject_unknown_fields};

fn validate_toml_file(path: &str) -> Result<(), ConfigError> {
    let content = fs::read_to_string(path)
        .map_err(|e| ConfigError::Message(format!("failed to read config file {}: {e}", path)))?;
    // Intentionally ignore TOML parse errors here: the `config` crate will
    // re-read the file and produce the redacted, source-aware parse error that
    // tests and CLI diagnostics rely on. We only enforce the strict
    // unknown-field whitelist when the file already parses.
    if let Ok(value) = toml::from_str::<Value>(&content) {
        reject_unknown_fields(&value).map_err(|e| ConfigError::Message(e.to_string()))?;
    }
    Ok(())
}

pub(crate) fn config_from_path(path: &str) -> Result<c::Config, ConfigError> {
    config_from_path_with_profile(path, None)
}

pub(crate) fn config_from_path_with_profile(
    path: &str,
    profile_path: Option<&Path>,
) -> Result<c::Config, ConfigError> {
    validate_toml_file(path)?;

    let mut builder = c::Config::builder().add_source(c::File::new(path, FileFormat::Toml));

    if let Some(profile_path) = profile_path {
        let profile_path_str = profile_path.to_str().ok_or_else(|| {
            ConfigError::Message(format!(
                "profile config path contains invalid UTF-8: {:?}",
                profile_path
            ))
        })?;
        validate_toml_file(profile_path_str)?;
        builder = builder.add_source(c::File::new(profile_path_str, FileFormat::Toml));
    }

    builder = builder.add_source(mega_environment_source());

    variable_placeholder_substitute(builder)
}

pub(crate) fn mega_environment_source() -> c::Environment {
    c::Environment::with_prefix("mega")
        .prefix_separator("_")
        .separator("__")
        .try_parsing(true)
        .with_list_parse_key("oauth.allowed_cors_origins")
        .with_list_parse_key("monorepo.admin")
        .with_list_parse_key("monorepo.root_dirs")
        .list_separator(",")
}
