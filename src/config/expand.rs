use std::{cell::RefCell, collections::HashMap, rc::Rc};

use c::{ConfigError, Source, ValueKind, builder::DefaultState};

use super::error::ConfigDiagnostic;
use crate::config::c;

/// supports braces-delimited variables (i.e. ${foo}) in config.
/// ### Example:
/// ```toml
/// base_dir = "/tmp/.mega"
/// [log]
/// level = "info"
/// ```
/// ### Limitations:
/// - only support `String` type.
/// - vars apply from up to down
pub(crate) fn variable_placeholder_substitute(
    mut builder: c::ConfigBuilder<DefaultState>,
) -> Result<c::Config, ConfigError> {
    // `Config::set` is deprecated, use `ConfigBuilder::set_override` instead
    let config = builder
        .clone()
        .build()
        .map_err(|source| ConfigDiagnostic::PlaceholderBuild {
            stage: "initial",
            source,
        })?; // initial config
    let mut vars = HashMap::new();
    // top-level variables
    for (k, mut v) in config
        .collect()
        .map_err(|source| ConfigDiagnostic::PlaceholderCollect {
            stage: "top-level",
            source,
        })?
    {
        // a copy
        if let ValueKind::String(str) = &v.kind {
            let origin = value_origin(&v);
            let expanded = expand_value(&k, &origin, str, &vars)?;
            if expanded.value != *str {
                v.kind = ValueKind::String(expanded.value.clone());
                builder = builder.set_override(&k, v).map_err(|source| {
                    ConfigDiagnostic::PlaceholderSetOverride {
                        key: k.clone(),
                        source,
                    }
                })?;
            }
            // File-mounted secrets are final: never register them as a `${var}`
            // substitution source, so their bytes can never reach `envsubst`.
            if !expanded.from_file {
                vars.insert(k, expanded.value);
            }
        }
    }
    // second-level or nested variables
    // extract all config k-v
    let map = Rc::new(RefCell::new(HashMap::new()));
    for (k, v) in config
        .collect()
        .map_err(|source| ConfigDiagnostic::PlaceholderCollect {
            stage: "nested",
            source,
        })?
    {
        if let ValueKind::Table(_) = v.kind {
            let map_c = map.clone();
            traverse_config(&k, &v, &move |key: &str, value: &c::Value| {
                if let ValueKind::String(_) = value.kind {
                    map_c.borrow_mut().insert(key.to_string(), value.clone());
                }
            });
        }
    }

    // do substitution: ${} -> real value
    let values = Rc::try_unwrap(map)
        .map_err(|_| ConfigDiagnostic::PlaceholderTraversalState)?
        .into_inner();
    for (k, mut v) in values {
        let str =
            v.clone()
                .into_string()
                .map_err(|source| ConfigDiagnostic::PlaceholderStringValue {
                    key: k.clone(),
                    source,
                })?;
        let origin = value_origin(&v);
        let expanded = expand_value(&k, &origin, &str, &vars)?;
        if expanded.value != str {
            v.kind = ValueKind::String(expanded.value.clone());
            builder = builder.set_override(&k, v).map_err(|source| {
                ConfigDiagnostic::PlaceholderSetOverride {
                    key: k.clone(),
                    source,
                }
            })?;
        }
        // File-mounted secrets are final: never register them as a `${var}`
        // substitution source, so their bytes can never reach `envsubst`.
        if !expanded.from_file {
            vars.insert(k, expanded.value);
        }
    }

    builder
        .build()
        .map_err(|source| ConfigDiagnostic::PlaceholderBuild {
            stage: "final",
            source,
        })
        .map_err(ConfigError::from)
}

fn substitute_placeholder(
    key: &str,
    origin: &str,
    template: &str,
    vars: &HashMap<String, String>,
) -> Result<String, ConfigError> {
    let expanded = envsubst::substitute(template, vars).map_err(|_| {
        ConfigDiagnostic::PlaceholderSubstitute {
            origin: origin.to_string(),
            key: key.to_string(),
        }
    })?;

    if envsubst::is_templated(&expanded) {
        return Err(ConfigDiagnostic::PlaceholderUnresolved {
            origin: origin.to_string(),
            key: key.to_string(),
        }
        .into());
    }

    Ok(expanded)
}

const FILE_PLACEHOLDER_PREFIX: &str = "${file:";

/// Expand a single config string value.
///
/// Handles two placeholder kinds:
/// - `${file:/path/to/secret}` — replaced with the contents of the named file
///   (a single trailing newline, `\n` or `\r\n`, is stripped). Intended for
///   file-mounted secrets such as Kubernetes Secret volumes or systemd
///   `LoadCredential=`. The file content is read **verbatim** and is never
///   re-expanded as a template, so a secret may safely contain `$` or `${...}`.
/// - `${var}` — config-internal variable references resolved against previously
///   expanded values (handled by `envsubst`; unchanged behavior).
///
/// A value that contains a `${file:...}` placeholder must consist *only* of
/// `${file:...}` placeholders and literal text — mixing it with `${var}`
/// references is rejected ([`ConfigDiagnostic::FilePlaceholderMixed`]) so that
/// secret file content is never fed back through `envsubst`. An unterminated
/// `${file:` (missing `}`) is rejected as well.
///
/// A value resolved from `${file:...}` is reported as [`ExpandedValue::from_file`]
/// so the caller can treat it as **final**: it is never re-expanded and is never
/// registered as a `${var}` substitution source. Consequently file-mounted secret
/// bytes can never reach `envsubst`, and referencing a file-secret field from
/// another field via `${that.field}` fails to resolve (fail-closed) rather than
/// leaking the secret. Keep `${file:...}` secrets in dedicated fields.
fn expand_value(
    key: &str,
    origin: &str,
    raw: &str,
    vars: &HashMap<String, String>,
) -> Result<ExpandedValue, ConfigError> {
    if raw.contains(FILE_PLACEHOLDER_PREFIX) {
        // Reject mixing `${file:...}` with `${var}` (and unterminated `${file:`)
        // before reading any file. Once the file placeholders are stripped,
        // anything still templated means a `${var}` reference is present, which
        // would force secret content through `envsubst`.
        let residue = map_file_placeholders(key, origin, raw, |_| Ok(String::new()))?;
        if envsubst::is_templated(&residue) {
            return Err(ConfigDiagnostic::FilePlaceholderMixed {
                origin: origin.to_string(),
                key: key.to_string(),
            }
            .into());
        }
        let value =
            map_file_placeholders(key, origin, raw, |path| read_file_secret(key, origin, path))?;
        return Ok(ExpandedValue {
            value,
            from_file: true,
        });
    }

    if envsubst::is_templated(raw) {
        let value = substitute_placeholder(key, origin, raw, vars)?;
        return Ok(ExpandedValue {
            value,
            from_file: false,
        });
    }

    Ok(ExpandedValue {
        value: raw.to_string(),
        from_file: false,
    })
}

/// Outcome of [`expand_value`] for a single config string.
struct ExpandedValue {
    /// The expanded string.
    value: String,
    /// True when the value was produced from a `${file:...}` file-mounted secret.
    /// Such values are final: callers must not re-expand them and must not insert
    /// them into the `${var}` substitution map, so secret bytes never reach
    /// `envsubst`.
    from_file: bool,
}

/// Replace every `${file:PATH}` token in `input` using `replace(PATH)`.
///
/// Non-placeholder text is copied verbatim. A `${file:` without a closing `}`
/// is rejected with [`ConfigDiagnostic::FilePlaceholderUnterminated`] (the path
/// is redacted) rather than silently surviving as a literal. A `${file:...}`
/// whose path contains a nested `${...}` (e.g. `${file:${base_dir}/secret}`) is
/// rejected with [`ConfigDiagnostic::FilePlaceholderMixed`]: file paths are not
/// variable-expanded, and the inner `}` would otherwise be mis-parsed as the
/// closing brace, so the secret file path is never derived from another
/// placeholder.
fn map_file_placeholders(
    key: &str,
    origin: &str,
    input: &str,
    mut replace: impl FnMut(&str) -> Result<String, ConfigError>,
) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(FILE_PLACEHOLDER_PREFIX) {
        out.push_str(&rest[..start]);
        let after = &rest[start + FILE_PLACEHOLDER_PREFIX.len()..];
        let Some(end) = after.find('}') else {
            return Err(ConfigDiagnostic::FilePlaceholderUnterminated {
                origin: origin.to_string(),
                key: key.to_string(),
            }
            .into());
        };
        let path = &after[..end];
        // `${...}` inside the path means either a nested variable placeholder or
        // a brace that the first `}` scan mis-took for the closing brace; either
        // way file paths are not variable-expanded, so reject it.
        if path.contains("${") {
            return Err(ConfigDiagnostic::FilePlaceholderMixed {
                origin: origin.to_string(),
                key: key.to_string(),
            }
            .into());
        }
        out.push_str(&replace(path)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Read a file-mounted secret for a `${file:...}` placeholder.
///
/// Strips a single trailing newline (`\n` or `\r\n`), which secret-provisioning
/// tooling commonly appends. The file content is never included in error output.
///
/// The path comes from the (trusted) operator-provided configuration: it may be
/// absolute or relative and is not sandboxed to a base directory, by design —
/// the same trust level as every other path in the config file (e.g. `base_dir`,
/// object-storage roots). It is never variable-expanded (see
/// [`map_file_placeholders`]), so it cannot be derived from another placeholder.
/// Surrounding whitespace on the path is intentionally trimmed as a convenience
/// (so `${file: /run/secrets/x }` works); this normalization applies to the
/// path only — the file *content* is used verbatim (bar the single trailing
/// newline stripped above).
fn read_file_secret(key: &str, origin: &str, path: &str) -> Result<String, ConfigError> {
    let path = path.trim();
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content
                .strip_suffix('\n')
                .map(|stripped| stripped.strip_suffix('\r').unwrap_or(stripped))
                .unwrap_or(content.as_str());
            Ok(trimmed.to_string())
        }
        Err(error) => Err(ConfigDiagnostic::FilePlaceholderRead {
            origin: origin.to_string(),
            key: key.to_string(),
            path: path.to_string(),
            reason: io_error_reason(error.kind()),
        }
        .into()),
    }
}

fn io_error_reason(kind: std::io::ErrorKind) -> String {
    match kind {
        std::io::ErrorKind::NotFound => "file not found".to_string(),
        std::io::ErrorKind::PermissionDenied => "permission denied".to_string(),
        other => format!("{other:?}"),
    }
}

fn value_origin(value: &c::Value) -> String {
    value.origin().unwrap_or("merged config").to_string()
}

/// visitor pattern: traverse each config & execute the closure `f`
fn traverse_config(key: &str, value: &c::Value, f: &impl Fn(&str, &c::Value)) {
    match &value.kind {
        ValueKind::Table(table) => {
            for (k, v) in table.iter() {
                // join keys by '.'
                let new_key = if key.is_empty() {
                    k.clone()
                } else {
                    format!("{key}.{k}")
                };
                traverse_config(&new_key, v, f);
            }
        }
        _ => f(key, value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_expansion_error_is_returned_instead_of_panicking() {
        let builder = c::Config::builder()
            .set_override("base_dir", "/tmp/${missing}")
            .expect("set base_dir")
            .set_override("log.path", "${base_dir}")
            .expect("set nested template");

        let err = match variable_placeholder_substitute(builder) {
            Ok(_) => panic!("placeholder expansion should fail"),
            Err(err) => err,
        };

        let message = err.to_string();
        assert!(message.contains("unresolved placeholder"));
        assert!(message.contains("base_dir"));
        assert!(message.contains("value is redacted"));
        assert!(message.contains("remove the placeholder"));
        assert!(!message.contains("/tmp/${missing}"));
    }

    #[test]
    fn placeholder_substitution_error_redacts_context_values() {
        let builder = c::Config::builder()
            .set_override("base_dir", "/tmp/private$value")
            .expect("set base_dir")
            .set_override("log.path", "${base_dir}")
            .expect("set nested template");

        let err = match variable_placeholder_substitute(builder) {
            Ok(_) => panic!("placeholder expansion should fail"),
            Err(err) => err,
        };

        let message = err.to_string();
        assert!(message.contains("failed to expand placeholder"));
        assert!(message.contains("log.path"));
        assert!(message.contains("value is redacted"));
        assert!(message.contains("without nested `${...}`"));
        assert!(!message.contains("/tmp/private$value"));
    }

    #[test]
    fn file_placeholder_expands_file_contents_and_strips_trailing_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret_path = dir.path().join("mail_password");
        std::fs::write(&secret_path, "s3cr3t-value\n").expect("write secret");
        let placeholder = format!("${{file:{}}}", secret_path.display());

        let builder = c::Config::builder()
            .set_override("mail.password", placeholder)
            .expect("set value");

        let config = variable_placeholder_substitute(builder).expect("expand");
        assert_eq!(config.get_string("mail.password").unwrap(), "s3cr3t-value");
    }

    #[test]
    fn file_placeholder_content_is_not_re_expanded_as_template() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret_path = dir.path().join("weird_password");
        // The secret itself looks like a template; it must be used verbatim and
        // must not trigger placeholder resolution.
        std::fs::write(&secret_path, "p@ss${not_a_var}word").expect("write secret");
        let placeholder = format!("${{file:{}}}", secret_path.display());

        let builder = c::Config::builder()
            .set_override("mail.password", placeholder)
            .expect("set value");

        let config = variable_placeholder_substitute(builder).expect("expand");
        assert_eq!(
            config.get_string("mail.password").unwrap(),
            "p@ss${not_a_var}word"
        );
    }

    #[test]
    fn file_placeholder_content_with_known_var_is_not_expanded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret_path = dir.path().join("password");
        // The secret references `${base_dir}`, a variable that IS defined below.
        // It must still be used verbatim and never expanded on the first or the
        // second (nested) pass.
        std::fs::write(&secret_path, "p@ss${base_dir}word").expect("write secret");
        let placeholder = format!("${{file:{}}}", secret_path.display());

        let builder = c::Config::builder()
            .set_override("base_dir", "/tmp/known")
            .expect("set base_dir")
            .set_override("mail.password", placeholder)
            .expect("set value");

        let config = variable_placeholder_substitute(builder).expect("expand");
        assert_eq!(
            config.get_string("mail.password").unwrap(),
            "p@ss${base_dir}word"
        );
        // Non-file `${var}` substitution still works for ordinary fields.
        assert_eq!(config.get_string("base_dir").unwrap(), "/tmp/known");
    }

    #[test]
    fn file_placeholder_secret_is_not_exposed_as_substitution_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret_path = dir.path().join("topsecret");
        std::fs::write(&secret_path, "s3cr3t").expect("write secret");
        let placeholder = format!("${{file:{}}}", secret_path.display());

        // `leak` tries to pull the file secret in via `${secret}`. File-mounted
        // secrets are never registered as substitution sources, so this must fail
        // to resolve (fail-closed) instead of leaking the secret into the value.
        let builder = c::Config::builder()
            .set_override("secret", placeholder)
            .expect("set secret")
            .set_override("leak", "${secret}")
            .expect("set leak");

        let err = variable_placeholder_substitute(builder)
            .expect_err("referencing a file secret must not resolve");
        let message = err.to_string();
        assert!(!message.contains("s3cr3t"));
    }

    #[test]
    fn file_placeholder_unterminated_is_rejected() {
        let builder = c::Config::builder()
            .set_override("mail.password", "${file:/run/secrets/password")
            .expect("set value");

        let err = variable_placeholder_substitute(builder)
            .expect_err("unterminated file placeholder should be rejected");
        let message = err.to_string();
        assert!(message.contains("mail.password"));
        assert!(message.contains("unterminated"));
        // The path must be redacted from the unterminated diagnostic.
        assert!(!message.contains("/run/secrets/password"));
    }

    #[test]
    fn file_placeholder_mixed_with_var_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret_path = dir.path().join("password");
        std::fs::write(&secret_path, "abc").expect("write secret");
        // Mixing ${file:...} with a ${var} reference is unsupported so that
        // secret content is never re-expanded as a template.
        let value = format!("${{file:{}}}-${{base_dir}}", secret_path.display());

        let builder = c::Config::builder()
            .set_override("base_dir", "/tmp/x")
            .expect("set base_dir")
            .set_override("mail.password", value)
            .expect("set value");

        let err = variable_placeholder_substitute(builder)
            .expect_err("mixed file/var value should be rejected");
        let message = err.to_string();
        assert!(message.contains("mail.password"));
        assert!(message.contains("mixes"));
    }

    #[test]
    fn file_placeholder_with_nested_var_in_path_is_rejected() {
        // A nested `${var}` inside the file path must be rejected, not parsed as
        // a path of `${base_dir` (the inner `}` is not the closing brace).
        let builder = c::Config::builder()
            .set_override("base_dir", "/tmp/x")
            .expect("set base_dir")
            .set_override("mail.password", "${file:${base_dir}/secret}")
            .expect("set value");

        let err = variable_placeholder_substitute(builder)
            .expect_err("nested var in file path should be rejected");
        let message = err.to_string();
        assert!(message.contains("mail.password"));
        // Must be rejected as mixed/nested, not attempted as a file read.
        assert!(message.contains("mixes"));
        assert!(!message.contains("file not found"));
    }

    #[test]
    fn file_placeholder_missing_file_errors_without_leaking_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent_secret");
        let placeholder = format!("${{file:{}}}", missing.display());

        let builder = c::Config::builder()
            .set_override("mail.password", placeholder)
            .expect("set value");

        let err = variable_placeholder_substitute(builder)
            .expect_err("missing file-mounted secret should error");
        let message = err.to_string();
        assert!(message.contains("mail.password"));
        assert!(message.contains("file not found"));
        assert!(message.contains("never logged"));
    }
}
