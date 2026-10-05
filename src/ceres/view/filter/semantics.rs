use super::{Filter, canonicalize};

pub fn invert(filter: &Filter) -> Filter {
    let inverted = match filter {
        Filter::Subdir(path) => Filter::Prefix(path.clone()),
        Filter::Prefix(path) => Filter::Subdir(path.clone()),
        Filter::Exclude(selectors) => Filter::Exclude(selectors.clone()),
        Filter::Compose(members) => Filter::Compose(members.iter().map(invert).collect()),
        Filter::Chain(ops) => Filter::Chain(ops.iter().rev().map(invert).collect()),
        Filter::Nop => Filter::Nop,
        Filter::Empty => Filter::Empty,
    };
    canonicalize(inverted)
}

/// Computes the exact source paths for a filter already accepted for registration.
///
/// Callers handling request-supplied filters must validate them before calling this
/// function, because the exact result itself can be large for an unvalidated AST.
pub fn src_paths(filter: &Filter) -> Vec<String> {
    let mut paths = normalized_paths(pull(filter, vec![Vec::new()]))
        .into_iter()
        .map(|path| display_path(&path))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

pub(crate) fn src_paths_limited(
    filter: &Filter,
    max_paths: usize,
) -> Result<Vec<String>, SrcPathLimitExceeded> {
    let paths = pull_limited(filter, vec![Vec::new()], max_paths)?;
    let paths = normalized_paths(paths);
    if paths.len() > max_paths {
        return Err(SrcPathLimitExceeded);
    }
    let mut paths = paths
        .into_iter()
        .map(|path| display_path(&path))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SrcPathLimitExceeded;

fn normalized_paths(mut paths: Vec<Vec<String>>) -> Vec<Vec<String>> {
    paths.sort();
    paths.dedup();
    let mut minimal: Vec<Vec<String>> = Vec::new();
    for path in paths {
        if !minimal.iter().any(|ancestor| is_prefix(ancestor, &path)) {
            minimal.push(path);
        }
    }
    minimal
}

fn pull_limited(
    filter: &Filter,
    paths: Vec<Vec<String>>,
    max_paths: usize,
) -> Result<Vec<Vec<String>>, SrcPathLimitExceeded> {
    let paths = match filter {
        Filter::Subdir(path) => paths
            .into_iter()
            .map(|suffix| join(path.segments(), &suffix))
            .collect(),
        Filter::Prefix(path) => paths
            .into_iter()
            .filter_map(|path_at_output| pull_prefix(path.segments(), &path_at_output))
            .collect(),
        Filter::Exclude(_) | Filter::Nop => paths,
        Filter::Empty => Vec::new(),
        Filter::Compose(members) => {
            let mut output = Vec::new();
            for member in members {
                output.extend(pull_limited(member, paths.clone(), max_paths)?);
                output = normalized_paths(output);
                if output.len() > max_paths {
                    return Err(SrcPathLimitExceeded);
                }
            }
            output
        }
        Filter::Chain(ops) => {
            let mut output = paths;
            for op in ops.iter().rev() {
                output = pull_limited(op, output, max_paths)?;
                output = normalized_paths(output);
                if output.len() > max_paths {
                    return Err(SrcPathLimitExceeded);
                }
            }
            output
        }
    };
    let paths = normalized_paths(paths);
    if paths.len() > max_paths {
        return Err(SrcPathLimitExceeded);
    }
    Ok(paths)
}

fn pull(filter: &Filter, paths: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let paths = match filter {
        Filter::Subdir(path) => paths
            .into_iter()
            .map(|suffix| join(path.segments(), &suffix))
            .collect(),
        Filter::Prefix(path) => paths
            .into_iter()
            .filter_map(|path_at_output| pull_prefix(path.segments(), &path_at_output))
            .collect(),
        Filter::Exclude(_) | Filter::Nop => paths,
        Filter::Empty => Vec::new(),
        Filter::Compose(members) => {
            let mut output = Vec::new();
            for member in members {
                output.extend(pull(member, paths.clone()));
                output = normalized_paths(output);
            }
            output
        }
        Filter::Chain(ops) => {
            let mut output = paths;
            for op in ops.iter().rev() {
                output = normalized_paths(pull(op, output));
            }
            output
        }
    };
    normalized_paths(paths)
}

fn pull_prefix(prefix: &[String], output: &[String]) -> Option<Vec<String>> {
    if is_prefix(prefix, output) {
        return Some(output[prefix.len()..].to_vec());
    }
    if is_prefix(output, prefix) {
        return Some(Vec::new());
    }
    None
}

fn join(prefix: &[String], suffix: &[String]) -> Vec<String> {
    prefix.iter().chain(suffix).cloned().collect::<Vec<_>>()
}

pub(crate) fn is_prefix(ancestor: &[String], descendant: &[String]) -> bool {
    ancestor.len() <= descendant.len()
        && ancestor
            .iter()
            .zip(descendant)
            .all(|(left, right)| left == right)
}

pub(crate) fn display_path(path: &[String]) -> String {
    if path.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", path.join("/"))
    }
}

pub(crate) fn path_segments(path: &str) -> Vec<String> {
    path.strip_prefix('/')
        .filter(|path| !path.is_empty())
        .map(|path| path.split('/').map(ToOwned::to_owned).collect())
        .unwrap_or_default()
}

pub(crate) fn all_compose_source_paths_limited(
    filter: &Filter,
    max_paths: usize,
) -> Result<Vec<Vec<Vec<String>>>, SrcPathLimitExceeded> {
    let mut groups = Vec::new();
    collect_compose_source_paths_limited(filter, max_paths, &mut groups)?;
    Ok(groups)
}

fn collect_compose_source_paths_limited(
    filter: &Filter,
    max_paths: usize,
    groups: &mut Vec<Vec<Vec<String>>>,
) -> Result<(), SrcPathLimitExceeded> {
    match filter {
        Filter::Compose(members) => {
            let mut paths = Vec::new();
            for member in members {
                paths.extend(
                    src_paths_limited(member, max_paths)?
                        .into_iter()
                        .map(|path| path_segments(&path)),
                );
            }
            groups.push(paths);
            for member in members {
                collect_compose_source_paths_limited(member, max_paths, groups)?;
            }
        }
        Filter::Chain(ops) => {
            for op in ops {
                collect_compose_source_paths_limited(op, max_paths, groups)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        super::{canonicalize, parse, print},
        *,
    };

    fn filter(text: &str) -> Filter {
        canonicalize(parse(text).unwrap())
    }

    #[test]
    fn invert_table() {
        for (input, expected) in [
            (":/p", ":prefix=p"),
            (":prefix=p", ":/p"),
            (":nop", ":nop"),
            (":empty", ":empty"),
            (":/a:prefix=x", ":/x:prefix=a"),
            (
                ":[:/a:prefix=x,:/b:prefix=y]",
                ":[:/x:prefix=a,:/y:prefix=b]",
            ),
            (":exclude[::b/]", ":exclude[::b/]"),
            (":/a:exclude[::b/]", ":exclude[::b/]:prefix=a"),
        ] {
            assert_eq!(print(&invert(&filter(input))), expected, "{input}");
        }
    }

    #[test]
    fn src_paths_examples() {
        for (input, expected) in [
            (":exclude[::secret]", vec!["/"]),
            (":/a:exclude[::b/]", vec!["/a"]),
            (":prefix=x", vec!["/"]),
            (":/a:prefix=x", vec!["/a"]),
            (":/a:[:/b:prefix=x,:/c:prefix=y]", vec!["/a/b", "/a/c"]),
            (":[:/a:prefix=x,:/b:prefix=y]:/x", vec!["/a"]),
            (":prefix=x:[:/w:prefix=b,:/z:prefix=a]", Vec::<&str>::new()),
            (":exclude[::a]:/a", vec!["/a"]),
            (":nop", vec!["/"]),
            (":[:/B:prefix=b,:/a:prefix=a]", vec!["/B", "/a"]),
            (":[:/a/b:prefix=x,:/a.c:prefix=y]", vec!["/a.c", "/a/b"]),
        ] {
            assert_eq!(src_paths(&filter(input)), expected, "{input}");
        }
    }
}
