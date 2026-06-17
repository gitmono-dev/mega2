use c::{ConfigError, FileFormat};

use super::expand::variable_placeholder_substitute;
use crate::config::c;

pub(crate) fn config_from_path(path: &str) -> Result<c::Config, ConfigError> {
    let builder = c::Config::builder()
        .add_source(c::File::new(path, FileFormat::Toml))
        .add_source(
            c::Environment::with_prefix("mega")
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true)
                .with_list_parse_key("oauth.allowed_cors_origins")
                .with_list_parse_key("monorepo.admin")
                .with_list_parse_key("monorepo.root_dirs")
                .list_separator(","),
        );

    variable_placeholder_substitute(builder)
}
