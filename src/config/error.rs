use std::fmt;

use crate::config::c::ConfigError;

#[derive(Debug)]
pub enum ConfigDiagnostic {
    PlaceholderBuild {
        stage: &'static str,
        source: ConfigError,
    },
    PlaceholderCollect {
        stage: &'static str,
        source: ConfigError,
    },
    PlaceholderSubstitute {
        origin: String,
        key: String,
    },
    PlaceholderUnresolved {
        origin: String,
        key: String,
    },
    PlaceholderSetOverride {
        key: String,
        source: ConfigError,
    },
    PlaceholderStringValue {
        key: String,
        source: ConfigError,
    },
    PlaceholderTraversalState,
    EnvironmentType {
        variable: String,
        key: String,
        expected: &'static str,
    },
    SourceType {
        origin: String,
        key: String,
        expected: &'static str,
    },
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlaceholderBuild { stage, source } => write!(
                f,
                "failed to build config during placeholder expansion ({stage}): {}",
                redacted_config_error(source)
            ),
            Self::PlaceholderCollect { stage, source } => write!(
                f,
                "failed to collect config values during placeholder expansion ({stage}): {}",
                redacted_config_error(source)
            ),
            Self::PlaceholderSubstitute { origin, key } => write!(
                f,
                "failed to expand placeholder from `{origin}` for `{key}`: value is redacted; ensure referenced placeholders resolve to plain strings without nested `${{...}}` or remove the placeholder"
            ),
            Self::PlaceholderUnresolved { origin, key } => write!(
                f,
                "unresolved placeholder from `{origin}` for `{key}`: value is redacted; define the referenced placeholder before `{key}` or remove the placeholder"
            ),
            Self::PlaceholderSetOverride { key, source } => write!(
                f,
                "failed to set expanded placeholder override for `{key}`: {}",
                redacted_config_error(source)
            ),
            Self::PlaceholderStringValue { key, source } => write!(
                f,
                "failed to read placeholder value for `{key}` as string: {}",
                redacted_config_error(source)
            ),
            Self::PlaceholderTraversalState => write!(
                f,
                "failed to finalize placeholder expansion: shared traversal state still has owners"
            ),
            Self::EnvironmentType {
                variable,
                key,
                expected,
            } => write!(
                f,
                "invalid environment variable `{variable}` for `{key}`: expected {expected}; value is redacted; set `{variable}` to {expected} or remove the override"
            ),
            Self::SourceType {
                origin,
                key,
                expected,
            } => write!(
                f,
                "invalid config value from `{origin}` for `{key}`: expected {expected}; value is redacted; set `{key}` to {expected} in `{origin}` or remove the override"
            ),
        }
    }
}

impl std::error::Error for ConfigDiagnostic {}

impl From<ConfigDiagnostic> for ConfigError {
    fn from(error: ConfigDiagnostic) -> Self {
        ConfigError::Message(error.to_string())
    }
}

fn redacted_config_error(source: &ConfigError) -> String {
    let message = source.to_string();
    let mut redacted_lines = Vec::new();
    let mut redacted_source_line = false;

    for line in message.lines() {
        if is_toml_source_excerpt_line(line) {
            if !redacted_source_line {
                redacted_lines.push("  | config source line redacted".to_string());
                redacted_source_line = true;
            }
            continue;
        }

        redacted_lines.push(line.to_string());
    }

    if redacted_source_line {
        redacted_lines.push("value is redacted".to_string());
    }

    redacted_lines.join("\n")
}

fn is_toml_source_excerpt_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('|') {
        return true;
    }

    let digit_count = trimmed
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();

    digit_count > 0 && trimmed[digit_count..].starts_with(" |")
}
