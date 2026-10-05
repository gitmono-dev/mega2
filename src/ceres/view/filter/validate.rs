use super::{
    Filter, canonicalize,
    semantics::{
        all_compose_source_paths_limited, display_path, is_prefix, path_segments, src_paths_limited,
    },
};
use crate::{
    ceres::{
        pack::path_policy::{in_import_namespace, is_import_dir_ancestor},
        view::VIEW_URL_RESERVED_NAMES,
    },
    config::MonoConfig,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScaleLimits {
    pub members: usize,
    pub selectors: usize,
    pub k: usize,
}

pub const REGISTER_SCALE_LIMITS: ScaleLimits = ScaleLimits {
    members: 64,
    selectors: 256,
    k: 64,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleLimitKind {
    Members,
    Selectors,
    K,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RegistrationRejection {
    #[error("filter has no registrable source paths")]
    Trivial,
    #[error("filter exceeds {which:?} scale limit {limit} with {actual}")]
    ScaleLimit {
        which: ScaleLimitKind,
        limit: usize,
        actual: usize,
    },
    #[error("compose members overlap at {first} and {second}")]
    ComposeOverlap { first: String, second: String },
    #[error("source path is in the ImportRepo namespace: {path}")]
    ImportNamespace { path: String },
    #[error("source path uses a reserved first segment: {path}")]
    ReservedFirstSegment { path: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationCheck {
    pub src_paths: Vec<String>,
    pub push_enabled: bool,
}

pub fn validate_for_registration(
    filter: &Filter,
    config: &MonoConfig,
) -> Result<RegistrationCheck, RegistrationRejection> {
    validate_with_limits(filter, config, REGISTER_SCALE_LIMITS)
}

fn validate_with_limits(
    filter: &Filter,
    config: &MonoConfig,
    limits: ScaleLimits,
) -> Result<RegistrationCheck, RegistrationRejection> {
    let filter = canonicalize(filter.clone());
    if matches!(filter, Filter::Nop | Filter::Empty) {
        return Err(RegistrationRejection::Trivial);
    }

    let (members, selectors) = count_scale(&filter);
    if members > limits.members {
        return Err(RegistrationRejection::ScaleLimit {
            which: ScaleLimitKind::Members,
            limit: limits.members,
            actual: members,
        });
    }
    if selectors > limits.selectors {
        return Err(RegistrationRejection::ScaleLimit {
            which: ScaleLimitKind::Selectors,
            limit: limits.selectors,
            actual: selectors,
        });
    }

    let src_paths = src_paths_limited(&filter, limits.k)
        .map_err(|_| scale_limit(ScaleLimitKind::K, limits.k))?;
    for group in all_compose_source_paths_limited(&filter, limits.k)
        .map_err(|_| scale_limit(ScaleLimitKind::K, limits.k))?
    {
        if let Some((first, second)) = first_overlap(&group) {
            return Err(RegistrationRejection::ComposeOverlap { first, second });
        }
    }

    if src_paths.is_empty() {
        return Err(RegistrationRejection::Trivial);
    }
    for path in &src_paths {
        if in_import_namespace(config, path) {
            return Err(RegistrationRejection::ImportNamespace { path: path.clone() });
        }
        if let Some(first) = path_segments(path).first()
            && VIEW_URL_RESERVED_NAMES.contains(&first.as_str())
        {
            return Err(RegistrationRejection::ReservedFirstSegment { path: path.clone() });
        }
    }

    Ok(RegistrationCheck {
        push_enabled: src_paths.iter().all(|path| push_enabled_for(path, config)),
        src_paths,
    })
}

fn scale_limit(which: ScaleLimitKind, limit: usize) -> RegistrationRejection {
    RegistrationRejection::ScaleLimit {
        which,
        limit,
        actual: limit.saturating_add(1),
    }
}

fn count_scale(filter: &Filter) -> (usize, usize) {
    match filter {
        Filter::Exclude(selectors) => (0, selectors.len()),
        Filter::Compose(members) => members.iter().fold(
            (members.len(), 0),
            |(member_count, selector_count), member| {
                let (nested_members, nested_selectors) = count_scale(member);
                (
                    member_count + nested_members,
                    selector_count + nested_selectors,
                )
            },
        ),
        Filter::Chain(ops) => ops
            .iter()
            .fold((0, 0), |(member_count, selector_count), op| {
                let (nested_members, nested_selectors) = count_scale(op);
                (
                    member_count + nested_members,
                    selector_count + nested_selectors,
                )
            }),
        _ => (0, 0),
    }
}

fn first_overlap(paths: &[Vec<String>]) -> Option<(String, String)> {
    let mut sorted = paths.to_vec();
    sorted.sort();
    for pair in sorted.windows(2) {
        let [first, second] = pair else {
            continue;
        };
        if is_prefix(first, second) {
            return Some((display_path(first), display_path(second)));
        }
    }
    None
}

fn push_enabled_for(path: &str, config: &MonoConfig) -> bool {
    let segments = path_segments(path);
    path != "/"
        && !in_import_namespace(config, path)
        && !is_import_dir_ancestor(config, path)
        && segments
            .first()
            .is_some_and(|first| config.root_dirs.iter().any(|root| root == first))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{super::parse_for_registration, *};
    use crate::ceres::view::filter::semantics::src_paths;

    fn check(text: &str, config: &MonoConfig) -> Result<RegistrationCheck, RegistrationRejection> {
        let parsed =
            parse_for_registration(text).unwrap_or_else(|error| panic!("{text}: {error:?}"));
        validate_for_registration(&parsed.filter, config)
    }

    #[test]
    fn compose_overlap_rejected() {
        let config = MonoConfig::default();
        for input in [
            ":[:/a:prefix=x,:/a/b:prefix=y]",
            ":[:/a:exclude[::b/]:prefix=x,:/a/b:prefix=y]",
            ":[:/a:prefix=x,:/a:prefix=x]",
            ":[:nop,:/a:prefix=x]",
            ":/r:[:/a:prefix=x,:/a/b:prefix=y]",
            ":prefix=x:[:/w:prefix=b,:/w/z:prefix=a]",
        ] {
            assert!(
                matches!(
                    check(input, &config),
                    Err(RegistrationRejection::ComposeOverlap { .. })
                ),
                "{input}"
            );
        }
        for input in [
            ":[:/a:prefix=x,:/b:prefix=y]",
            ":[:/a:prefix=x,:/ab:prefix=y]",
        ] {
            assert!(check(input, &config).is_ok(), "{input}");
        }
    }

    #[test]
    fn trivial_filters_rejected() {
        let config = MonoConfig::default();
        for input in [
            ":nop",
            ":prefix=p:/p",
            ":empty",
            ":/a:empty",
            ":prefix=a:/b",
            ":prefix=x:[:/w:prefix=b,:/z:prefix=a]",
        ] {
            assert!(
                matches!(check(input, &config), Err(RegistrationRejection::Trivial)),
                "{input}"
            );
        }
    }

    #[test]
    fn import_namespace_rejected() {
        let config = MonoConfig::default();
        for input in [
            ":/third-party",
            ":/third-party/x",
            ":[:/project:prefix=p,:/third-party/x:prefix=t]",
        ] {
            assert!(
                matches!(
                    check(input, &config),
                    Err(RegistrationRejection::ImportNamespace { .. })
                ),
                "{input}"
            );
        }
        assert!(check(":/third-partyx", &config).is_ok());
        assert!(check(":exclude[::secret]", &config).is_ok());
        let nested = MonoConfig {
            import_dir: PathBuf::from("/third-party/vendor"),
            ..config
        };
        assert!(check(":/third-party", &nested).is_ok());
        assert!(matches!(
            check(":/third-party/vendor/lib", &nested),
            Err(RegistrationRejection::ImportNamespace { .. })
        ));
    }

    #[test]
    fn reserved_first_segment_rejected() {
        let config = MonoConfig::default();
        assert_eq!(VIEW_URL_RESERVED_NAMES, [".view", ".filter"]);
        for input in [
            ":/.view",
            ":/.filter/x",
            ":[:/.view:prefix=v,:/a:prefix=x]",
            ":[:/.view:prefix=v,:/third-party/x:prefix=t]",
        ] {
            assert!(
                matches!(
                    check(input, &config),
                    Err(RegistrationRejection::ReservedFirstSegment { .. })
                ),
                "{input}"
            );
        }
        assert!(check(":/a/.view", &config).is_ok());
        assert!(check(":/.viewx", &config).is_ok());
        let import_view = MonoConfig {
            import_dir: PathBuf::from("/.view"),
            ..config
        };
        assert!(matches!(
            check(":/.view/x", &import_view),
            Err(RegistrationRejection::ImportNamespace { .. })
        ));
    }

    #[test]
    fn scale_limits() {
        let config = MonoConfig::default();
        assert_eq!(
            REGISTER_SCALE_LIMITS,
            ScaleLimits {
                members: 64,
                selectors: 256,
                k: 64,
            }
        );

        assert!(check(&compose(64), &config).is_ok());
        assert!(matches!(
            check(&compose(65), &config),
            Err(RegistrationRejection::ScaleLimit {
                which: ScaleLimitKind::Members,
                ..
            })
        ));

        let chained = format!(
            "{}{}",
            compose_named(33, "m", "n"),
            compose_named(33, "n", "p")
        );
        assert!(matches!(
            check(&chained, &config),
            Err(RegistrationRejection::ScaleLimit {
                which: ScaleLimitKind::Members,
                ..
            })
        ));
        assert!(check(&exclude(256), &config).is_ok());
        assert!(matches!(
            check(&exclude(257), &config),
            Err(RegistrationRejection::ScaleLimit {
                which: ScaleLimitKind::Selectors,
                ..
            })
        ));
        assert!(matches!(
            check(&format!("{}{}", compose(65), exclude(257)), &config),
            Err(RegistrationRejection::ScaleLimit {
                which: ScaleLimitKind::Members,
                ..
            })
        ));
        assert!(matches!(
            check(
                &format!(
                    ":[:nop,{}]",
                    compose(64).trim_start_matches(":[").trim_end_matches(']')
                ),
                &config
            ),
            Err(RegistrationRejection::ScaleLimit {
                which: ScaleLimitKind::Members,
                ..
            })
        ));
        assert!(matches!(
            check(&chain_bomb(30), &config),
            Err(RegistrationRejection::ScaleLimit {
                which: ScaleLimitKind::K,
                limit: 64,
                actual: 65,
            })
        ));

        let limits = ScaleLimits {
            members: 64,
            selectors: 256,
            k: 2,
        };
        for input in [
            ":/a:[:/b:prefix=x,:/c:prefix=y,:/d:prefix=z]",
            ":/third-party:[:/b:prefix=x,:/c:prefix=y,:/d:prefix=z]",
        ] {
            let filter = parse_for_registration(input).unwrap().filter;
            assert!(matches!(
                validate_with_limits(&filter, &config, limits),
                Err(RegistrationRejection::ScaleLimit {
                    which: ScaleLimitKind::K,
                    limit: 2,
                    actual: 3,
                })
            ));
        }
        for input in [
            ":[:/a:prefix=x,:/a:prefix=y,:/b:prefix=z,:/c:prefix=w]",
            ":[:[:/a:prefix=p,:/b:prefix=q,:/c:prefix=r]:prefix=z,:/d:prefix=w]:/w",
        ] {
            let filter = parse_for_registration(input).unwrap().filter;
            assert!(matches!(
                validate_with_limits(&filter, &config, limits),
                Err(RegistrationRejection::ScaleLimit {
                    which: ScaleLimitKind::K,
                    limit: 2,
                    actual: 3,
                })
            ));
        }
        let filter = parse_for_registration(":/a:[:/b:prefix=x,:/c:prefix=y]")
            .unwrap()
            .filter;
        assert!(validate_with_limits(&filter, &config, limits).is_ok());
    }

    #[test]
    fn push_enabled_examples() {
        let config = MonoConfig {
            root_dirs: ["a", "project", "third-party"]
                .into_iter()
                .map(ToOwned::to_owned)
                .collect(),
            ..MonoConfig::default()
        };
        for (input, expected) in [
            (":exclude[::secret]", false),
            (":prefix=x", false),
            (":/a:exclude[::b/]", true),
            (":/a:prefix=x", true),
            (":/a:[:/b:prefix=x,:/c:prefix=y]", true),
            (":[:/a:prefix=x,:/b:prefix=y]:/x", true),
            (":/project/foo", true),
            (":/doc/x", false),
            (":[:/a:prefix=x,:/doc:prefix=y]", false),
        ] {
            let filter = parse_for_registration(input).unwrap().filter;
            let registration = validate_for_registration(&filter, &config).unwrap();
            assert_eq!(registration.push_enabled, expected, "{input}");
            assert_eq!(registration.src_paths, src_paths(&filter), "{input}");
        }
        let multi = parse_for_registration(":/a:[:/b:prefix=x,:/c:prefix=y]")
            .unwrap()
            .filter;
        assert_eq!(
            validate_for_registration(&multi, &config)
                .unwrap()
                .src_paths,
            ["/a/b", "/a/c"]
        );
        let mixed = parse_for_registration(":[:/a:prefix=x,:/doc:prefix=y]")
            .unwrap()
            .filter;
        assert_eq!(
            validate_for_registration(&mixed, &config)
                .unwrap()
                .src_paths,
            ["/a", "/doc"]
        );
        let byte_order = parse_for_registration(":[:/a/b:prefix=x,:/a.c:prefix=y]")
            .unwrap()
            .filter;
        assert_eq!(
            validate_for_registration(&byte_order, &config)
                .unwrap()
                .src_paths,
            ["/a.c", "/a/b"]
        );
        let nested = MonoConfig {
            import_dir: PathBuf::from("/third-party/vendor"),
            ..config
        };
        assert!(!check(":/third-party", &nested).unwrap().push_enabled);
    }

    fn compose(count: usize) -> String {
        compose_named(count, "m", "n")
    }

    fn compose_named(count: usize, source_prefix: &str, destination_prefix: &str) -> String {
        format!(
            ":[{}]",
            (0..count)
                .map(|index| format!(
                    ":/{source_prefix}{index:02}:prefix={destination_prefix}{index:02}"
                ))
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    fn exclude(count: usize) -> String {
        format!(
            ":exclude[{}]",
            (0..count)
                .map(|index| format!("::s{index:03}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    fn chain_bomb(repetitions: usize) -> String {
        ":[:/x/1:prefix=x,:/x/2:prefix=x]".repeat(repetitions)
    }
}
