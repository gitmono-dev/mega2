use std::path::Path;

use c::{ConfigError, FileFormat};

use super::expand::variable_placeholder_substitute;
use crate::config::c;

pub(crate) fn config_from_path(path: &str) -> Result<c::Config, ConfigError> {
    config_from_path_with_profile(path, None)
}

pub(crate) fn config_from_path_with_profile(
    path: &str,
    profile_path: Option<&Path>,
) -> Result<c::Config, ConfigError> {
    let mut builder = c::Config::builder().add_source(c::File::new(path, FileFormat::Toml));

    if let Some(profile_path) = profile_path {
        let profile_path = profile_path.to_str().ok_or_else(|| {
            ConfigError::Message(format!(
                "profile config path contains invalid UTF-8: {:?}",
                profile_path
            ))
        })?;
        builder = builder.add_source(c::File::new(profile_path, FileFormat::Toml));
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
