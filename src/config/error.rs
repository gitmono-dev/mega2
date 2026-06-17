use thiserror::Error;

use crate::config::c::ConfigError;

#[derive(Debug, Error)]
pub enum ConfigDiagnostic {
    #[error("failed to build config during placeholder expansion ({stage}): {source}")]
    PlaceholderBuild {
        stage: &'static str,
        source: ConfigError,
    },
    #[error("failed to collect config values during placeholder expansion ({stage}): {source}")]
    PlaceholderCollect {
        stage: &'static str,
        source: ConfigError,
    },
    #[error("failed to expand placeholder for `{key}`: {source}")]
    PlaceholderSubstitute {
        key: String,
        source: envsubst::Error,
    },
    #[error("failed to set expanded placeholder override for `{key}`: {source}")]
    PlaceholderSetOverride { key: String, source: ConfigError },
    #[error("failed to read placeholder value for `{key}` as string: {source}")]
    PlaceholderStringValue { key: String, source: ConfigError },
    #[error("failed to finalize placeholder expansion: shared traversal state still has owners")]
    PlaceholderTraversalState,
    #[error(
        "invalid environment variable `{variable}` for `{key}`: expected {expected}; value is redacted"
    )]
    EnvironmentType {
        variable: String,
        key: String,
        expected: &'static str,
    },
}

impl From<ConfigDiagnostic> for ConfigError {
    fn from(error: ConfigDiagnostic) -> Self {
        ConfigError::Message(error.to_string())
    }
}
