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
    #[error(
        "failed to expand placeholder from `{origin}` for `{key}`: value is redacted; ensure referenced placeholders resolve to plain strings without nested `${{...}}` or remove the placeholder"
    )]
    PlaceholderSubstitute { origin: String, key: String },
    #[error(
        "unresolved placeholder from `{origin}` for `{key}`: value is redacted; define the referenced placeholder before `{key}` or remove the placeholder"
    )]
    PlaceholderUnresolved { origin: String, key: String },
    #[error("failed to set expanded placeholder override for `{key}`: {source}")]
    PlaceholderSetOverride { key: String, source: ConfigError },
    #[error("failed to read placeholder value for `{key}` as string: {source}")]
    PlaceholderStringValue { key: String, source: ConfigError },
    #[error("failed to finalize placeholder expansion: shared traversal state still has owners")]
    PlaceholderTraversalState,
    #[error(
        "invalid environment variable `{variable}` for `{key}`: expected {expected}; value is redacted; set `{variable}` to {expected} or remove the override"
    )]
    EnvironmentType {
        variable: String,
        key: String,
        expected: &'static str,
    },
    #[error(
        "invalid config value from `{origin}` for `{key}`: expected {expected}; value is redacted; set `{key}` to {expected} in `{origin}` or remove the override"
    )]
    SourceType {
        origin: String,
        key: String,
        expected: &'static str,
    },
}

impl From<ConfigDiagnostic> for ConfigError {
    fn from(error: ConfigDiagnostic) -> Self {
        ConfigError::Message(error.to_string())
    }
}
