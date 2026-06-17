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
            if envsubst::is_templated(str) {
                let new_str = envsubst::substitute(str, &vars).map_err(|source| {
                    ConfigDiagnostic::PlaceholderSubstitute {
                        key: k.clone(),
                        source,
                    }
                })?;
                v.kind = ValueKind::String(new_str.clone());
                builder = builder.set_override(&k, v).map_err(|source| {
                    ConfigDiagnostic::PlaceholderSetOverride {
                        key: k.clone(),
                        source,
                    }
                })?;
                vars.insert(k, new_str);
            } else {
                vars.insert(k, str.clone());
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
        let mut str =
            v.clone()
                .into_string()
                .map_err(|source| ConfigDiagnostic::PlaceholderStringValue {
                    key: k.clone(),
                    source,
                })?;
        if envsubst::is_templated(&str) {
            let new_str = envsubst::substitute(&str, &vars).map_err(|source| {
                ConfigDiagnostic::PlaceholderSubstitute {
                    key: k.clone(),
                    source,
                }
            })?;
            // println!("{}: {} -> {}", k, str, &new_str);
            v.kind = ValueKind::String(new_str.clone());
            builder = builder.set_override(&k, v).map_err(|source| {
                ConfigDiagnostic::PlaceholderSetOverride {
                    key: k.clone(),
                    source,
                }
            })?;
            str = new_str;
        }
        vars.insert(k, str);
    }

    builder
        .build()
        .map_err(|source| ConfigDiagnostic::PlaceholderBuild {
            stage: "final",
            source,
        })
        .map_err(ConfigError::from)
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
        assert!(message.contains("failed to expand placeholder for `log.path`"));
    }
}
