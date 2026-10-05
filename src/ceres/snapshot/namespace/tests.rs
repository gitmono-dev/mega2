use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use tempfile::TempDir;

use crate::ceres::snapshot::{
    error::{SnapshotError, SnapshotErrorCode},
    namespace::{
        BindingPolicy, FixedBinding, FixedNamespaceIndex, FixedSourceReader, NamespaceError,
        NodeClass, SourceEntry, SourceKind, SourceNode, SourceOutcome,
    },
    resolver::FsKind,
};

#[derive(Default)]
struct MemoryReader {
    nodes: BTreeMap<(String, String), SourceNode>,
    fail_path: Option<String>,
    fail_listing: bool,
    resolves: usize,
    lists: usize,
}

impl MemoryReader {
    fn add(&mut self, source: &str, path: &str, fs_kind: FsKind) {
        self.nodes.insert(
            (source.to_string(), path.to_string()),
            SourceNode {
                fs_kind,
                oid: format!("{source}:{path}"),
            },
        );
    }

    fn roots() -> Self {
        let mut reader = Self::default();
        for source in ["native", "a20", "a21", "b30"] {
            reader.add(source, "", FsKind::Directory);
        }
        reader
    }
}

impl FixedSourceReader<String> for MemoryReader {
    fn resolve(&mut self, source: &String, path: &str) -> Result<SourceOutcome, SnapshotError> {
        self.resolves += 1;
        if self.fail_path.as_deref() == Some(path) {
            return Err(SnapshotError::new(
                SnapshotErrorCode::ObjectUnavailable,
                "injected missing backend object",
            ));
        }
        Ok(self
            .nodes
            .get(&(source.clone(), path.to_string()))
            .cloned()
            .map_or(SourceOutcome::AbsentProven, SourceOutcome::Found))
    }

    fn list(&mut self, source: &String, path: &str) -> Result<Vec<SourceEntry>, SnapshotError> {
        self.lists += 1;
        if self.fail_listing {
            return Err(SnapshotError::new(
                SnapshotErrorCode::ObjectUnavailable,
                "injected listing failure",
            ));
        }
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{path}/")
        };
        Ok(self
            .nodes
            .iter()
            .filter_map(|((owner, node_path), node)| {
                if owner != source {
                    return None;
                }
                let suffix = node_path.strip_prefix(&prefix)?;
                if suffix.is_empty() || suffix.contains('/') {
                    return None;
                }
                Some(SourceEntry {
                    name: suffix.to_string(),
                    node: node.clone(),
                })
            })
            .collect())
    }
}

fn binding(path: &str, source: &str, subpath: &str) -> FixedBinding<String> {
    FixedBinding {
        mount_path: path.to_string(),
        source: source.to_string(),
        source_kind: SourceKind::Import,
        source_subpath: subpath.to_string(),
        policy: BindingPolicy::Mutable,
    }
}

#[test]
fn component_prefix_source_subpath_and_nested_routing_match_manual_table() {
    let mut reader = MemoryReader::roots();
    reader.add("a20", "src", FsKind::Directory);
    let index = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![
            binding("/x/lib", "a20", "src"),
            binding("/x/lib/vendor/b", "b30", ""),
        ],
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    for (path, expected_source, expected_path) in [
        ("/", "native", ""),
        ("/x/lib", "a20", "src"),
        ("/x/lib/a", "a20", "src/a"),
        ("/x/library/a", "native", "x/library/a"),
        ("/x/lib/vendor", "a20", "src/vendor"),
        ("/x/lib/vendor/b/z", "b30", "z"),
        ("/x/lib/vendor/beta/z", "a20", "src/vendor/beta/z"),
        ("/x/lib/é+\\name", "a20", "src/é+\\name"),
    ] {
        let route = index.route(path).unwrap();
        assert_eq!(route.source, expected_source, "{path}");
        assert_eq!(route.source_path, expected_path, "{path}");
    }
}

#[test]
fn synthetic_ancestors_have_no_source_and_exact_mount_has_no_native_union() {
    let mut reader = MemoryReader::roots();
    reader.add("native", "lib", FsKind::Directory);
    reader.add("native", "lib/native-only", FsKind::Regular);
    reader.add("a20", "src", FsKind::Directory);
    reader.add("a20", "src/import-only", FsKind::Executable);
    let index = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![
            binding("/lib", "a20", "src"),
            binding("/missing/deep/mount", "b30", ""),
        ],
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    let synthetic = index.list("/missing", &mut reader).unwrap().unwrap();
    assert_eq!(synthetic.node_class, NodeClass::Aggregate);
    assert!(synthetic.source_context.is_none());
    assert_eq!(synthetic.entries[0].name, "deep");
    assert!(synthetic.entries[0].oid.is_none());
    assert!(synthetic.entries[0].source_context.is_none());
    let imported = index.list("/lib", &mut reader).unwrap().unwrap();
    assert_eq!(imported.node_class, NodeClass::ImportRoot);
    assert_eq!(imported.entries.len(), 1);
    assert_eq!(imported.entries[0].name, "import-only");
    assert_eq!(imported.entries[0].fs_kind, FsKind::Executable);
    assert!(index.list("/absent", &mut reader).unwrap().is_none());
}

#[test]
fn node_class_depends_on_direct_composition_and_explicit_checkout() {
    let mut reader = MemoryReader::roots();
    reader.add("native", "app", FsKind::Directory);
    reader.add("native", "plain", FsKind::Directory);
    let index = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![binding("/app/vendor/lib", "a20", "")],
        BTreeSet::from(["/app".to_string(), "/plain".to_string()]),
        &mut reader,
    )
    .unwrap();
    assert_eq!(
        index.list("/", &mut reader).unwrap().unwrap().node_class,
        NodeClass::NativeTree
    );
    let app = index.list("/app", &mut reader).unwrap().unwrap();
    assert_eq!(app.node_class, NodeClass::Aggregate);
    assert!(app.source_context.is_some());
    assert_eq!(
        index
            .list("/plain", &mut reader)
            .unwrap()
            .unwrap()
            .node_class,
        NodeClass::NativeCheckoutRoot
    );
}

#[test]
fn file_and_symlink_crossings_and_exact_mount_targets_are_conflicts() {
    for kind in [FsKind::Regular, FsKind::Executable, FsKind::Symlink] {
        for mount in ["/blocked", "/blocked/child"] {
            let mut reader = MemoryReader::roots();
            reader.add("native", "blocked", kind);
            let error = FixedNamespaceIndex::new(
                "native".to_string(),
                vec![binding(mount, "a20", "")],
                BTreeSet::new(),
                &mut reader,
            )
            .unwrap_err();
            assert!(matches!(error, NamespaceError::Conflict(path) if path == "/blocked"));
        }
    }
    let mut reader = MemoryReader::roots();
    reader.add("a20", "file", FsKind::Regular);
    assert!(matches!(
        FixedNamespaceIndex::new(
            "native".to_string(),
            vec![binding("/import", "a20", "file")],
            BTreeSet::new(),
            &mut reader,
        ),
        Err(NamespaceError::SourceSubpathNotDirectory(_))
    ));
}

#[test]
fn nested_binding_cannot_hide_an_outer_source_file() {
    let mut reader = MemoryReader::roots();
    reader.add("a20", "blocked", FsKind::Symlink);
    let result = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![
            binding("/import", "a20", ""),
            binding("/import/blocked/nested", "b30", ""),
        ],
        BTreeSet::new(),
        &mut reader,
    );
    assert!(matches!(result, Err(NamespaceError::Conflict(path)) if path == "/import/blocked"));
}

#[test]
fn removing_binding_restores_native_in_new_index_and_preserves_old_index() {
    let mut reader = MemoryReader::roots();
    reader.add("native", "lib", FsKind::Directory);
    reader.add("native", "lib/native-only", FsKind::Regular);
    reader.add("a20", "old-only", FsKind::Regular);
    reader.add("a21", "new-only", FsKind::Regular);
    let old = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![binding("/lib", "a20", "")],
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    let new = FixedNamespaceIndex::new(
        "native".to_string(),
        Vec::new(),
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    assert_eq!(
        old.list("/lib", &mut reader).unwrap().unwrap().entries[0].name,
        "old-only"
    );
    assert_eq!(
        new.list("/lib", &mut reader).unwrap().unwrap().entries[0].name,
        "native-only"
    );
    assert_eq!(old.route("/lib/unvisited/deep").unwrap().source, "a20");
}

#[test]
fn duplicate_mounts_and_noncanonical_paths_are_rejected() {
    let mut reader = MemoryReader::roots();
    assert!(matches!(
        FixedNamespaceIndex::new(
            "native".to_string(),
            vec![binding("/same", "a20", ""), binding("/same", "b30", "")],
            BTreeSet::new(),
            &mut reader,
        ),
        Err(NamespaceError::DuplicateMount(_))
    ));
    for path in ["", "relative", "/a/", "/a//b", "/a/..", "/a\0"] {
        assert!(
            FixedNamespaceIndex::new(
                "native".to_string(),
                vec![binding(path, "a20", "")],
                BTreeSet::new(),
                &mut reader,
            )
            .is_err(),
            "{path:?}"
        );
    }
    for subpath in ["/src", "src/", "src//a", "src/..", "src\0"] {
        assert!(
            FixedNamespaceIndex::new(
                "native".to_string(),
                vec![binding("/lib", "a20", subpath)],
                BTreeSet::new(),
                &mut reader,
            )
            .is_err(),
            "{subpath:?}"
        );
    }
}

#[test]
fn composed_source_path_limits_are_checked_after_prefix_replacement() {
    let mut reader = MemoryReader::roots();
    let subpath = vec!["a"; 256].join("/");
    reader.add("a20", &subpath, FsKind::Directory);
    let index = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![binding("/lib", "a20", &subpath)],
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    assert!(index.route("/lib").is_ok());
    assert!(index.route("/lib/extra").is_err());
    assert!(index.route(&format!("/{}", "a".repeat(256))).is_err());
    assert!(
        index
            .route(&format!("/{}", vec!["a"; 257].join("/")))
            .is_err()
    );
    let max_bytes = vec!["a".repeat(255); 16].join("/");
    assert_eq!(max_bytes.len() + 1, 4096);
    reader.add("a20", &max_bytes, FsKind::Directory);
    let index = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![binding("/lib", "a20", &max_bytes)],
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    assert!(index.route("/lib").is_ok());
    assert!(index.route("/lib/x").is_err());
}

#[test]
fn import_root_class_yields_to_direct_binding_and_import_tree_class_is_retained() {
    let mut reader = MemoryReader::roots();
    reader.add("a20", "src", FsKind::Directory);
    reader.add("a20", "src/plain", FsKind::Directory);
    let index = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![
            binding("/lib", "a20", "src"),
            binding("/lib/nested", "b30", ""),
        ],
        BTreeSet::from(["/lib".to_string()]),
        &mut reader,
    )
    .unwrap();
    let root = index.list("/lib", &mut reader).unwrap().unwrap();
    assert_eq!(root.node_class, NodeClass::Aggregate);
    assert_eq!(root.source_context.as_ref().unwrap().source, "a20");
    assert_eq!(
        index
            .list("/lib/plain", &mut reader)
            .unwrap()
            .unwrap()
            .node_class,
        NodeClass::ImportTree
    );
    assert_eq!(
        index
            .list("/lib/nested", &mut reader)
            .unwrap()
            .unwrap()
            .node_class,
        NodeClass::ImportRoot
    );
}

#[test]
fn storage_failure_is_preserved_in_construction_and_listing() {
    let mut reader = MemoryReader::roots();
    let index = FixedNamespaceIndex::new(
        "native".to_string(),
        Vec::new(),
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    reader.fail_path = Some("missing".to_string());
    assert!(matches!(
        index.list("/missing", &mut reader),
        Err(NamespaceError::Snapshot(error)) if error.code == SnapshotErrorCode::ObjectUnavailable
    ));
    assert!(matches!(
        FixedNamespaceIndex::new(
            "native".to_string(),
            vec![binding("/lib", "a20", "missing")],
            BTreeSet::new(),
            &mut reader,
        ),
        Err(NamespaceError::Snapshot(error)) if error.code == SnapshotErrorCode::ObjectUnavailable
    ));
    reader.fail_path = None;
    reader.fail_listing = true;
    assert!(matches!(
        index.list("/", &mut reader),
        Err(NamespaceError::Snapshot(error)) if error.code == SnapshotErrorCode::ObjectUnavailable
    ));
}

#[test]
fn root_binding_replaces_entire_native_source() {
    let mut reader = MemoryReader::roots();
    reader.add("native", "native-only", FsKind::Regular);
    reader.add("a20", "import-only", FsKind::Regular);
    let index = FixedNamespaceIndex::new(
        "native".to_string(),
        vec![binding("/", "a20", "")],
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    assert_eq!(index.route("/").unwrap().source, "a20");
    assert_eq!(index.route("/anything").unwrap().source_path, "anything");
    let root = index.list("/", &mut reader).unwrap().unwrap();
    assert_eq!(root.node_class, NodeClass::ImportRoot);
    assert_eq!(root.entries.len(), 1);
    assert_eq!(root.entries[0].name, "import-only");
}

#[test]
fn invalid_and_duplicate_source_entries_are_rejected() {
    struct MalformedReader(Vec<String>);
    impl FixedSourceReader<String> for MalformedReader {
        fn resolve(&mut self, _: &String, _: &str) -> Result<SourceOutcome, SnapshotError> {
            Ok(SourceOutcome::Found(SourceNode {
                fs_kind: FsKind::Directory,
                oid: "root".to_string(),
            }))
        }

        fn list(&mut self, _: &String, _: &str) -> Result<Vec<SourceEntry>, SnapshotError> {
            Ok(self
                .0
                .iter()
                .map(|name| SourceEntry {
                    name: name.clone(),
                    node: SourceNode {
                        fs_kind: FsKind::Regular,
                        oid: "blob".to_string(),
                    },
                })
                .collect())
        }
    }
    for names in [vec![""], vec![".."], vec!["a/b"], vec!["same", "same"]] {
        let mut reader = MalformedReader(names.iter().map(|s| s.to_string()).collect());
        let index = FixedNamespaceIndex::new(
            "native".to_string(),
            Vec::new(),
            BTreeSet::new(),
            &mut reader,
        )
        .unwrap();
        assert!(matches!(
            index.list("/", &mut reader),
            Err(NamespaceError::InvalidSourceEntry(_))
        ));
    }
}

#[test]
fn unrelated_binding_count_does_not_expand_source_reads_for_a_listing() {
    let mut reader = MemoryReader::roots();
    reader.add("native", "app", FsKind::Directory);
    let bindings = (0..512)
        .map(|i| binding(&format!("/elsewhere-{i}/lib"), "a20", ""))
        .collect();
    let index =
        FixedNamespaceIndex::new("native".to_string(), bindings, BTreeSet::new(), &mut reader)
            .unwrap();
    reader.resolves = 0;
    reader.lists = 0;
    index.list("/app", &mut reader).unwrap().unwrap();
    assert_eq!((reader.resolves, reader.lists), (1, 1));
}

fn git(repo: &Path, args: &[&str], input: Option<&[u8]>) -> String {
    let mut child = Command::new("git")
        .current_dir(repo)
        .env("GIT_AUTHOR_NAME", "MST2 Oracle")
        .env("GIT_AUTHOR_EMAIL", "mst2-oracle@example.invalid")
        .env("GIT_COMMITTER_NAME", "MST2 Oracle")
        .env("GIT_COMMITTER_EMAIL", "mst2-oracle@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-09-15T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-09-15T00:00:00Z")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input).unwrap();
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn make_tree(repo: &Path, files: &[(&str, &str)]) -> String {
    let mut items = Vec::new();
    let mut directories: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for (path, bytes) in files {
        if let Some((head, tail)) = path.split_once('/') {
            directories.entry(head).or_default().push((tail, bytes));
        } else {
            let oid = git(
                repo,
                &["hash-object", "-w", "--stdin"],
                Some(bytes.as_bytes()),
            );
            items.push(format!("100644 blob {}\t{path}\n", oid.trim()));
        }
    }
    for (name, children) in directories {
        let oid = make_tree(repo, &children);
        items.push(format!("040000 tree {oid}\t{name}\n"));
    }
    git(repo, &["mktree"], Some(items.concat().as_bytes()))
        .trim()
        .to_string()
}

#[derive(Debug, Clone)]
struct GitSource {
    root: String,
    commit: String,
    scope_path: String,
}

struct GitReader<'a> {
    repo: &'a Path,
}

impl FixedSourceReader<GitSource> for GitReader<'_> {
    fn resolve(&mut self, source: &GitSource, path: &str) -> Result<SourceOutcome, SnapshotError> {
        if path.is_empty() {
            return Ok(SourceOutcome::Found(SourceNode {
                fs_kind: FsKind::Directory,
                oid: source.root.clone(),
            }));
        }
        let mut tree = source.root.clone();
        let components: Vec<_> = path.split('/').collect();
        for (position, component) in components.iter().enumerate() {
            let Some(entry) = self
                .read_tree(&tree)
                .into_iter()
                .find(|entry| entry.name == *component)
            else {
                return Ok(SourceOutcome::AbsentProven);
            };
            if position + 1 == components.len() {
                return Ok(SourceOutcome::Found(entry.node));
            }
            if entry.node.fs_kind != FsKind::Directory {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::NotDirectory,
                    "fixed Git path crossing",
                ));
            }
            tree = entry.node.oid;
        }
        unreachable!()
    }

    fn list(&mut self, source: &GitSource, path: &str) -> Result<Vec<SourceEntry>, SnapshotError> {
        match self.resolve(source, path)? {
            SourceOutcome::Found(node) if node.fs_kind == FsKind::Directory => {
                Ok(self.read_tree(&node.oid))
            }
            _ => Err(SnapshotError::new(
                SnapshotErrorCode::NotDirectory,
                "fixed Git listing",
            )),
        }
    }
}

impl GitReader<'_> {
    fn read_tree(&self, oid: &str) -> Vec<SourceEntry> {
        git(self.repo, &["ls-tree", "-z", oid], None)
            .split('\0')
            .filter(|row| !row.is_empty())
            .map(|row| {
                let (metadata, name) = row.split_once('\t').unwrap();
                let fields: Vec<_> = metadata.split_whitespace().collect();
                SourceEntry {
                    name: name.to_string(),
                    node: SourceNode {
                        fs_kind: if fields[0] == "040000" {
                            FsKind::Directory
                        } else {
                            FsKind::Regular
                        },
                        oid: fields[2].to_string(),
                    },
                }
            })
            .collect()
    }
}

fn fixed_source(repo: &Path, files: &[(&str, &str)]) -> GitSource {
    let root = make_tree(repo, files);
    let commit = git(
        repo,
        &["commit-tree", &root],
        Some(b"fixed oracle fixture\n"),
    )
    .trim()
    .to_string();
    GitSource {
        root,
        commit,
        scope_path: "/".to_string(),
    }
}

#[test]
fn attested_scoped_root_is_not_prefixed_again_when_subpath_and_nested_mount_are_used() {
    let repo = TempDir::new().unwrap();
    git(
        repo.path(),
        &["init", "--bare", "--object-format=sha1", "--quiet"],
        None,
    );
    let native = fixed_source(repo.path(), &[("native.txt", "native")]);
    let full = fixed_source(
        repo.path(),
        &[
            ("scope/src/deep/leaf.txt", "scoped"),
            ("other/outside.txt", "outside"),
        ],
    );
    let scoped = GitSource {
        root: git(
            repo.path(),
            &["rev-parse", &format!("{}:scope", full.commit)],
            None,
        )
        .trim()
        .to_string(),
        commit: full.commit.clone(),
        scope_path: "/scope".to_string(),
    };
    let nested = fixed_source(repo.path(), &[("inside/nested.txt", "nested")]);
    let mut reader = GitReader { repo: repo.path() };
    let index = FixedNamespaceIndex::new(
        native,
        vec![
            git_binding("/lib", &scoped, "src"),
            git_binding("/lib/deep/nested", &nested, "inside"),
        ],
        BTreeSet::new(),
        &mut reader,
    )
    .unwrap();
    let route = index.route("/lib/deep/leaf.txt").unwrap();
    assert_eq!(route.source.scope_path, "/scope");
    assert_eq!(route.source_path, "src/deep/leaf.txt");
    let route = index.route("/lib/deep/nested/nested.txt").unwrap();
    assert_eq!(route.source_path, "inside/nested.txt");
    let mut files = BTreeMap::new();
    traverse(&index, &mut reader, "/", &mut files);
    assert_eq!(
        files,
        BTreeMap::from([
            ("/native.txt".to_string(), b"native".to_vec()),
            ("/lib/deep/leaf.txt".to_string(), b"scoped".to_vec()),
            (
                "/lib/deep/nested/nested.txt".to_string(),
                b"nested".to_vec()
            ),
        ])
    );
}

fn git_binding(path: &str, source: &GitSource, subpath: &str) -> FixedBinding<GitSource> {
    FixedBinding {
        mount_path: path.to_string(),
        source: source.clone(),
        source_kind: SourceKind::Import,
        source_subpath: subpath.to_string(),
        policy: BindingPolicy::Mutable,
    }
}

/// Independently flatten commits using Git, then compose a handwritten fixture.
/// This oracle does not call the production trie or source reader to select paths.
fn flatten_commit(repo: &Path, source: &GitSource) -> BTreeMap<String, Vec<u8>> {
    git(
        repo,
        &["ls-tree", "-rz", "--full-tree", &source.commit],
        None,
    )
    .split('\0')
    .filter(|row| !row.is_empty())
    .map(|row| {
        let (metadata, path) = row.split_once('\t').unwrap();
        let oid = metadata.split_whitespace().nth(2).unwrap();
        (
            path.to_string(),
            git(repo, &["cat-file", "blob", oid], None).into_bytes(),
        )
    })
    .collect()
}

fn traverse(
    index: &FixedNamespaceIndex<GitSource>,
    reader: &mut GitReader<'_>,
    path: &str,
    files: &mut BTreeMap<String, Vec<u8>>,
) {
    for entry in index.list(path, reader).unwrap().unwrap().entries {
        let child = if path == "/" {
            format!("/{}", entry.name)
        } else {
            format!("{path}/{}", entry.name)
        };
        if entry.fs_kind == FsKind::Directory {
            traverse(index, reader, &child, files);
        } else {
            files.insert(
                child,
                git(
                    reader.repo,
                    &["cat-file", "blob", entry.oid.as_deref().unwrap()],
                    None,
                )
                .into_bytes(),
            );
        }
    }
}

#[test]
fn composed_listing_matches_independent_fixed_git_oracle_after_ref_and_path_reuse() {
    let repo = TempDir::new().unwrap();
    git(
        repo.path(),
        &["init", "--bare", "--object-format=sha1", "--quiet"],
        None,
    );
    let native = fixed_source(
        repo.path(),
        &[
            ("project/app/native.txt", "native"),
            ("lib/native-hidden.txt", "hidden"),
            ("library/keep.txt", "neighbor"),
        ],
    );
    let a20 = fixed_source(
        repo.path(),
        &[
            ("src/deep/old.txt", "A20"),
            ("src/vendor/b/hidden.txt", "outer-hidden"),
            ("elsewhere/not-visible.txt", "outside-subpath"),
        ],
    );
    let a21 = fixed_source(repo.path(), &[("src/deep/new.txt", "A21")]);
    let b30 = fixed_source(repo.path(), &[("nested.txt", "B30")]);
    let bindings = vec![
        git_binding("/lib", &a20, "src"),
        git_binding("/lib/vendor/b", &b30, ""),
        git_binding("/missing/deep/import", &b30, ""),
    ];
    let mut reader = GitReader { repo: repo.path() };
    let old =
        FixedNamespaceIndex::new(native.clone(), bindings, BTreeSet::new(), &mut reader).unwrap();
    // Simulate advancing current refs and reassigning a registry path. The
    // reader has only the captured fixed handle; no registry/ref read is possible.
    git(
        repo.path(),
        &["update-ref", "refs/heads/import-current", &a21.commit],
        None,
    );
    let current_registry = BTreeMap::from([("/lib", a21.clone())]);
    assert_ne!(
        current_registry["/lib"].commit,
        old.route("/lib/deep/old.txt").unwrap().source.commit
    );
    let mut expected = BTreeMap::new();
    for (path, bytes) in flatten_commit(repo.path(), &native) {
        if !path.starts_with("lib/") {
            expected.insert(format!("/{path}"), bytes);
        }
    }
    for (path, bytes) in flatten_commit(repo.path(), &a20) {
        if let Some(suffix) = path.strip_prefix("src/")
            && !suffix.starts_with("vendor/b/")
        {
            expected.insert(format!("/lib/{suffix}"), bytes);
        }
    }
    for (path, bytes) in flatten_commit(repo.path(), &b30) {
        expected.insert(format!("/lib/vendor/b/{path}"), bytes.clone());
        expected.insert(format!("/missing/deep/import/{path}"), bytes);
    }
    let mut actual = BTreeMap::new();
    traverse(&old, &mut reader, "/", &mut actual);
    assert_eq!(actual, expected);
    assert!(actual.contains_key("/lib/deep/old.txt"));
    assert!(!actual.contains_key("/lib/deep/new.txt"));
    let unbound =
        FixedNamespaceIndex::new(native.clone(), Vec::new(), BTreeSet::new(), &mut reader).unwrap();
    let mut restored = BTreeMap::new();
    traverse(&unbound, &mut reader, "/", &mut restored);
    let expected_native = flatten_commit(repo.path(), &native)
        .into_iter()
        .map(|(path, bytes)| (format!("/{path}"), bytes))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(restored, expected_native);
    let mut old_again = BTreeMap::new();
    traverse(&old, &mut reader, "/", &mut old_again);
    assert_eq!(old_again, expected);
}
