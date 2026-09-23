//! Monorepo path creation policy (plan-20260923 ADR-FU-04 item 2): the one
//! classifier shared by path provisioning, product writes and receive-pack.

use crate::{
    common::{errors::PathPolicyError, utils::canonicalize_mono_ref_path},
    config::MonoConfig,
};

/// Classify a write that would create `canonical_path`.
///
/// `Ok(())` means the path may be created. The import namespace is checked
/// first (import first: a path under `import_dir` is an ImportRepo even when
/// `root_dirs` has an entry with the same name), then the root itself, then
/// membership of the first component in `root_dirs`. The input must already
/// be canonical; anything else is `Invalid`.
pub fn classify_creation_path(
    config: &MonoConfig,
    canonical_path: &str,
) -> Result<(), PathPolicyError> {
    if canonical_path.contains('\0') {
        return Err(invalid(canonical_path, "path must not contain NUL"));
    }
    if canonicalize_mono_ref_path(canonical_path).ok().as_deref() != Some(canonical_path) {
        return Err(invalid(canonical_path, "path is not canonical"));
    }
    if in_import_namespace(config, canonical_path) {
        return Err(not_allowed(config, canonical_path));
    }
    if canonical_path == "/" {
        return Err(invalid(
            canonical_path,
            "the monorepo root cannot be created",
        ));
    }
    let first = canonical_path[1..].split('/').next().unwrap_or_default();
    if !config.root_dirs.iter().any(|root| root == first) {
        return Err(not_allowed(config, canonical_path));
    }
    Ok(())
}

/// Strict entry for client JSON input: rejects NUL and `\` explicitly, then
/// requires the raw input to already be canonical (no relative path, `.` /
/// `..` segment, repeated or trailing slash). Git protocol URL paths keep the
/// canonicalize-then-use rule (TP-08) and do not go through this function.
pub fn strict_creation_path_input(input: &str) -> Result<String, PathPolicyError> {
    if input.contains('\0') {
        return Err(invalid(input, "path must not contain NUL"));
    }
    if input.contains('\\') {
        return Err(invalid(input, "path must use '/' as the separator"));
    }
    match canonicalize_mono_ref_path(input) {
        Ok(canonical) if canonical == input => Ok(canonical),
        Ok(canonical) => Err(invalid(
            input,
            &format!("path must be canonical (did you mean {canonical:?}?)"),
        )),
        Err(_) => Err(invalid(
            input,
            "path must be an absolute path without '.' or '..' segments",
        )),
    }
}

/// True when `canonical_path` is `import_dir` or below it (component-level).
/// Product writes use this as the unconditional ImportRepo namespace guard.
pub fn in_import_namespace(config: &MonoConfig, canonical_path: &str) -> bool {
    is_same_or_under(canonical_path, &config.import_dir.to_string_lossy())
}

/// True when `canonical_path` is a strict ancestor of `import_dir` (only
/// possible with a nested `import_dir` such as `/third-party/vendor`). Such a
/// path must never get a materialized `main` ref: the ImportRepo attach
/// precheck refuses imports below a materialized ancestor (GC-FU-04).
pub fn is_import_dir_ancestor(config: &MonoConfig, canonical_path: &str) -> bool {
    let import_dir = config.import_dir.to_string_lossy();
    canonical_path == "/"
        || (canonical_path != import_dir && is_same_or_under(&import_dir, canonical_path))
}

/// Component-level prefix match: `/a/b` is under `/a`, `/ab` is not.
fn is_same_or_under(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The `NotAllowed` error for `path`, listing the configured roots (sorted)
/// and the ImportRepo directory.
pub fn not_allowed(config: &MonoConfig, path: &str) -> PathPolicyError {
    let mut allowed_roots: Vec<String> = config
        .root_dirs
        .iter()
        .map(|root| format!("/{root}"))
        .collect();
    allowed_roots.sort();
    PathPolicyError::NotAllowed {
        path: path.to_owned(),
        allowed_roots,
        import_dir: config.import_dir.to_string_lossy().into_owned(),
    }
}

fn invalid(path: &str, reason: &str) -> PathPolicyError {
    PathPolicyError::Invalid {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn config(root_dirs: &[&str], import_dir: &str) -> MonoConfig {
        MonoConfig {
            root_dirs: root_dirs.iter().map(|root| root.to_string()).collect(),
            import_dir: PathBuf::from(import_dir),
            ..MonoConfig::default()
        }
    }

    fn assert_not_allowed(config: &MonoConfig, path: &str) {
        match classify_creation_path(config, path) {
            Err(PathPolicyError::NotAllowed { path: got, .. }) => assert_eq!(got, path),
            other => panic!("{path}: expected NotAllowed, got {other:?}"),
        }
    }

    fn assert_invalid(result: Result<impl std::fmt::Debug, PathPolicyError>, what: &str) {
        match result {
            Err(PathPolicyError::Invalid { .. }) => {}
            other => panic!("{what}: expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn classify_import_dir_precedence() {
        // `third-party` is both a root and the import directory: import wins.
        let config = config(&["third-party", "project"], "/third-party");
        for path in ["/third-party", "/third-party/lib", "/third-party/lib/sub"] {
            assert_not_allowed(&config, path);
        }
        let nested = self::config(&["third-party"], "/third-party/vendor");
        assert_not_allowed(&nested, "/third-party/vendor");
        assert_not_allowed(&nested, "/third-party/vendor/lib");
        classify_creation_path(&nested, "/third-party").expect("parent of nested import dir");
        classify_creation_path(&nested, "/third-party/tools")
            .expect("sibling of nested import dir");
        classify_creation_path(&nested, "/third-party/vendorx").expect("component boundary");
    }

    #[test]
    fn classify_outside_roots_not_allowed() {
        let config = config(&["third-party", "project"], "/third-party");
        for path in ["/vendor", "/vendor/lib", "/projects/x", "/.cedar/x"] {
            assert_not_allowed(&config, path);
        }
        let err = classify_creation_path(&config, "/vendor/lib").unwrap_err();
        match &err {
            PathPolicyError::NotAllowed {
                allowed_roots,
                import_dir,
                ..
            } => {
                assert_eq!(allowed_roots, &["/project", "/third-party"]);
                assert_eq!(import_dir, "/third-party");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn import_namespace_helpers() {
        let nested = config(&["third-party", "project"], "/third-party/vendor");
        assert!(in_import_namespace(&nested, "/third-party/vendor"));
        assert!(in_import_namespace(&nested, "/third-party/vendor/lib"));
        assert!(!in_import_namespace(&nested, "/third-party/vendorx"));
        assert!(!in_import_namespace(&nested, "/third-party"));
        assert!(is_import_dir_ancestor(&nested, "/"));
        assert!(is_import_dir_ancestor(&nested, "/third-party"));
        assert!(!is_import_dir_ancestor(&nested, "/third-party/vendor"));
        assert!(!is_import_dir_ancestor(&nested, "/third-party/tools"));
        assert!(!is_import_dir_ancestor(&nested, "/project"));
        let flat = config(&["third-party", "project"], "/third-party");
        assert!(!is_import_dir_ancestor(&flat, "/project"));
        assert!(!is_import_dir_ancestor(&flat, "/third-party"));
    }

    #[test]
    fn classify_allowed_root() {
        let config = config(&["third-party", "project"], "/third-party");
        for path in ["/project", "/project/new", "/project/a/b/c"] {
            classify_creation_path(&config, path).unwrap_or_else(|e| panic!("{path}: {e}"));
        }
        let custom = self::config(&["apps"], "/apps/vendor");
        classify_creation_path(&custom, "/apps/web").expect("custom root");
        assert_not_allowed(&custom, "/project/web");
    }

    #[test]
    fn classify_invalid_paths() {
        let config = config(&["third-party", "project"], "/third-party");
        assert_invalid(classify_creation_path(&config, "/"), "root");
        for path in [
            "project/x",
            "/project//x",
            "/project/x/",
            "/project/./x",
            "/project/x\0",
        ] {
            assert_invalid(classify_creation_path(&config, path), path);
        }

        for input in [
            "project/x",
            "/project/../x",
            "/project//x",
            "/project/x/",
            "/project/./x",
            "/project/x\0",
            "/project\\x",
            "",
        ] {
            assert_invalid(strict_creation_path_input(input), input);
        }
        assert_eq!(
            strict_creation_path_input("/project/x").expect("canonical"),
            "/project/x"
        );
        assert_eq!(
            strict_creation_path_input("/").expect("root is canonical"),
            "/"
        );
    }
}
