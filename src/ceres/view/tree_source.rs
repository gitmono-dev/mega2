use std::collections::{HashMap, HashSet};

use git_internal::{
    errors::GitError,
    hash::{HashError, HashKind, ObjectHash},
    internal::object::{
        ObjectTrait,
        tree::{Tree, TreeItem, TreeItemMode},
        types::ObjectType,
    },
};

use crate::jupiter::utils::converter::sort_git_tree_items;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MissingObjectReason {
    Absent,
    Malformed,
    Unprefetched,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MissingObject {
    pub tree_id: String,
    pub reason: MissingObjectReason,
}

/// Reads only data that has already been loaded by a caller.
///
/// IDs outside that data must report `Unprefetched`. `Absent` is reserved for
/// IDs whose missing row has already been confirmed. Implementations parse
/// loaded bytes with `parse_tree_bytes`, so a tree with duplicate names never
/// returns successfully.
pub(crate) trait TreeSource {
    fn read_tree(&self, tree_id: &str) -> Result<Tree, MissingObject>;
}

pub(crate) struct InMemoryTreeSource {
    kind: HashKind,
    tree_bytes: HashMap<String, Vec<u8>>,
    missing: HashSet<String>,
}

impl InMemoryTreeSource {
    pub(crate) fn new(
        kind: HashKind,
        tree_bytes: HashMap<String, Vec<u8>>,
        missing: HashSet<String>,
    ) -> Self {
        Self {
            kind,
            tree_bytes,
            missing,
        }
    }
}

impl TreeSource for InMemoryTreeSource {
    fn read_tree(&self, tree_id: &str) -> Result<Tree, MissingObject> {
        if self.missing.contains(tree_id) {
            return Err(missing(tree_id, MissingObjectReason::Absent));
        }
        if let Some(bytes) = self.tree_bytes.get(tree_id) {
            return parse_tree_bytes(self.kind, tree_id, bytes);
        }
        Err(missing(tree_id, MissingObjectReason::Unprefetched))
    }
}

pub(crate) fn parse_tree_bytes(
    kind: HashKind,
    tree_id: &str,
    bytes: &[u8],
) -> Result<Tree, MissingObject> {
    let hash = ObjectHash::from_hex_for_kind(kind, tree_id)
        .map_err(|_| missing(tree_id, MissingObjectReason::Malformed))?;
    let tree = <Tree as ObjectTrait>::from_bytes(bytes, hash)
        .map_err(|_| missing(tree_id, MissingObjectReason::Malformed))?;
    ensure_unique_item_names(&tree.tree_items)
        .map_err(|_| missing(tree_id, MissingObjectReason::Malformed))?;
    Ok(tree)
}

pub(crate) fn read_tree<S: TreeSource + ?Sized>(
    kind: HashKind,
    source: &S,
    tree_id: &str,
) -> Result<Tree, MissingObject> {
    if let Ok(empty_id) = empty_tree_id(kind)
        && ObjectHash::from_hex_for_kind(kind, tree_id).is_ok_and(|id| id == empty_id)
    {
        return Ok(Tree {
            id: empty_id,
            tree_items: Vec::new(),
        });
    }
    source.read_tree(tree_id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PathLookup {
    Tree(String),
    Absent,
    NotTree,
}

/// An empty path selects `root` without dereferencing it.
pub(crate) fn lookup_path<S: TreeSource + ?Sized>(
    kind: HashKind,
    source: &S,
    root: &str,
    path: &[String],
) -> Result<PathLookup, MissingObject> {
    let (last, parents) = match path.split_last() {
        Some(parts) => parts,
        None => return Ok(PathLookup::Tree(root.to_owned())),
    };
    let mut current = read_tree(kind, source, root)?;
    for segment in parents {
        let Some(item) = current
            .tree_items
            .iter()
            .find(|item| item.name == segment.as_str())
        else {
            return Ok(PathLookup::Absent);
        };
        if item.mode != TreeItemMode::Tree {
            return Ok(PathLookup::NotTree);
        }
        current = read_tree(kind, source, &item.id.to_string())?;
    }

    let Some(item) = current
        .tree_items
        .iter()
        .find(|item| item.name == last.as_str())
    else {
        return Ok(PathLookup::Absent);
    };
    Ok(if item.mode == TreeItemMode::Tree {
        PathLookup::Tree(item.id.to_string())
    } else {
        PathLookup::NotTree
    })
}

pub(crate) fn empty_tree_id(kind: HashKind) -> Result<ObjectHash, HashError> {
    ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Tree, b"")
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BuildTreeError {
    #[error("tree contains duplicate item name: {0}")]
    DuplicateName(String),
    #[error(transparent)]
    EmptyTreeId(#[from] HashError),
    #[error(transparent)]
    InvalidTree(#[from] GitError),
}

pub(crate) fn build_tree(
    kind: HashKind,
    mut tree_items: Vec<TreeItem>,
) -> Result<Tree, BuildTreeError> {
    ensure_unique_item_names(&tree_items).map_err(BuildTreeError::DuplicateName)?;
    if tree_items.is_empty() {
        return Ok(Tree {
            id: empty_tree_id(kind)?,
            tree_items,
        });
    }
    sort_git_tree_items(&mut tree_items);
    Tree::from_tree_items_with_kind(kind, tree_items).map_err(BuildTreeError::InvalidTree)
}

fn missing(tree_id: &str, reason: MissingObjectReason) -> MissingObject {
    MissingObject {
        tree_id: tree_id.to_owned(),
        reason,
    }
}

fn ensure_unique_item_names(tree_items: &[TreeItem]) -> Result<(), String> {
    let mut names = HashSet::with_capacity(tree_items.len());
    for item in tree_items {
        if !names.insert(item.name.as_str()) {
            return Err(item.name.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use git_internal::{
        hash::{HashKind, ObjectHash, set_hash_kind_for_test},
        internal::object::{
            ObjectTrait,
            tree::{Tree, TreeItem, TreeItemMode},
            types::ObjectType,
        },
    };

    use super::{
        BuildTreeError, InMemoryTreeSource, MissingObject, MissingObjectReason, PathLookup,
        build_tree, empty_tree_id, lookup_path, read_tree,
    };

    fn opposite(kind: HashKind) -> HashKind {
        match kind {
            HashKind::Sha1 => HashKind::Sha256,
            HashKind::Sha256 | HashKind::Blake3 => HashKind::Sha1,
        }
    }

    fn object_id(kind: HashKind, label: &[u8]) -> ObjectHash {
        ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Blob, label).unwrap()
    }

    fn item(kind: HashKind, mode: TreeItemMode, name: &str, label: &[u8]) -> TreeItem {
        TreeItem {
            mode,
            id: object_id(kind, label),
            name: name.to_owned(),
        }
    }

    fn raw_item(mode: &[u8], name: &str, id: ObjectHash) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(mode);
        bytes.push(b' ');
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(id.as_ref());
        bytes
    }

    fn tree_bytes(tree: &Tree) -> Vec<u8> {
        tree.to_data().unwrap()
    }

    fn source(
        kind: HashKind,
        entries: Vec<(String, Vec<u8>)>,
        missing: Vec<String>,
    ) -> InMemoryTreeSource {
        InMemoryTreeSource::new(
            kind,
            entries.into_iter().collect::<HashMap<_, _>>(),
            missing.into_iter().collect::<HashSet<_>>(),
        )
    }

    fn valid_tree_bytes(kind: HashKind, label: &[u8]) -> Vec<u8> {
        tree_bytes(&build_tree(kind, vec![item(kind, TreeItemMode::Blob, "file", label)]).unwrap())
    }

    fn assert_missing(
        kind: HashKind,
        source: &InMemoryTreeSource,
        tree_id: &str,
        reason: MissingObjectReason,
    ) {
        assert_eq!(
            read_tree(kind, source, tree_id).unwrap_err(),
            MissingObject {
                tree_id: tree_id.to_owned(),
                reason,
            }
        );
    }

    #[test]
    fn read_tree_missing_object_reasons() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            let requested = object_id(kind, b"requested").to_string();

            let absent = source(kind, Vec::new(), vec![requested.clone()]);
            assert_missing(kind, &absent, &requested, MissingObjectReason::Absent);

            let mut truncated = valid_tree_bytes(kind, b"truncated");
            truncated.pop();
            let malformed = source(kind, vec![(requested.clone(), truncated)], Vec::new());
            assert_missing(kind, &malformed, &requested, MissingObjectReason::Malformed);

            let invalid_mode = source(
                kind,
                vec![(
                    requested.clone(),
                    raw_item(b"100600", "file", object_id(kind, b"invalid-mode")),
                )],
                Vec::new(),
            );
            assert_missing(
                kind,
                &invalid_mode,
                &requested,
                MissingObjectReason::Malformed,
            );

            let foreign_id = object_id(opposite(kind), b"foreign").to_string();
            let foreign = source(
                kind,
                vec![(foreign_id.clone(), valid_tree_bytes(kind, b"foreign"))],
                Vec::new(),
            );
            assert_missing(kind, &foreign, &foreign_id, MissingObjectReason::Malformed);

            let nonadjacent = [
                raw_item(b"100644", "foo", object_id(kind, b"foo-file")),
                raw_item(b"100644", "foo-bar", object_id(kind, b"foo-bar")),
                raw_item(b"100644", "foo.txt", object_id(kind, b"foo-txt")),
                raw_item(b"40000", "foo", object_id(kind, b"foo-tree")),
            ]
            .concat();
            let nonadjacent = source(kind, vec![(requested.clone(), nonadjacent)], Vec::new());
            assert_missing(
                kind,
                &nonadjacent,
                &requested,
                MissingObjectReason::Malformed,
            );

            let adjacent = [
                raw_item(b"100644", "bar", object_id(kind, b"bar-one")),
                raw_item(b"100644", "bar", object_id(kind, b"bar-two")),
            ]
            .concat();
            let adjacent = source(kind, vec![(requested.clone(), adjacent)], Vec::new());
            assert_missing(kind, &adjacent, &requested, MissingObjectReason::Malformed);

            let unprefetched = source(kind, Vec::new(), Vec::new());
            assert_missing(
                kind,
                &unprefetched,
                &requested,
                MissingObjectReason::Unprefetched,
            );
        }
    }

    #[test]
    fn read_tree_empty_tree_without_source() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            let empty = empty_tree_id(kind).unwrap();
            for (entries, missing) in [
                (Vec::new(), Vec::new()),
                (Vec::new(), vec![empty.to_string()]),
                (vec![(empty.to_string(), b"malformed".to_vec())], Vec::new()),
            ] {
                let tree =
                    read_tree(kind, &source(kind, entries, missing), &empty.to_string()).unwrap();
                assert_eq!(tree.id, empty);
                assert!(tree.tree_items.is_empty());
            }
            let uppercase = empty.to_string().to_ascii_uppercase();
            let tree = read_tree(kind, &source(kind, Vec::new(), Vec::new()), &uppercase).unwrap();
            assert_eq!(tree.id, empty);
            assert!(tree.tree_items.is_empty());
        }
    }

    #[test]
    fn lookup_path_three_states() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            let b =
                build_tree(kind, vec![item(kind, TreeItemMode::Blob, "leaf", b"leaf")]).unwrap();
            let a = build_tree(
                kind,
                vec![TreeItem {
                    mode: TreeItemMode::Tree,
                    id: b.id,
                    name: "b".to_owned(),
                }],
            )
            .unwrap();
            let root = build_tree(
                kind,
                vec![TreeItem {
                    mode: TreeItemMode::Tree,
                    id: a.id,
                    name: "a".to_owned(),
                }],
            )
            .unwrap();
            let path_ab = ["a".to_owned(), "b".to_owned()];
            for tail in [
                source(
                    kind,
                    vec![
                        (root.id.to_string(), tree_bytes(&root)),
                        (a.id.to_string(), tree_bytes(&a)),
                        (b.id.to_string(), tree_bytes(&b)),
                    ],
                    Vec::new(),
                ),
                source(
                    kind,
                    vec![
                        (root.id.to_string(), tree_bytes(&root)),
                        (a.id.to_string(), tree_bytes(&a)),
                    ],
                    vec![b.id.to_string()],
                ),
                source(
                    kind,
                    vec![
                        (root.id.to_string(), tree_bytes(&root)),
                        (a.id.to_string(), tree_bytes(&a)),
                    ],
                    Vec::new(),
                ),
            ] {
                assert_eq!(
                    lookup_path(kind, &tail, &root.id.to_string(), &path_ab).unwrap(),
                    PathLookup::Tree(b.id.to_string())
                );
            }

            let root_without_a =
                build_tree(kind, vec![item(kind, TreeItemMode::Blob, "c", b"c")]).unwrap();
            let without_a = source(
                kind,
                vec![(root_without_a.id.to_string(), tree_bytes(&root_without_a))],
                Vec::new(),
            );
            assert_eq!(
                lookup_path(
                    kind,
                    &without_a,
                    &root_without_a.id.to_string(),
                    &["a".to_owned()]
                )
                .unwrap(),
                PathLookup::Absent
            );
            assert_eq!(
                lookup_path(kind, &without_a, &root_without_a.id.to_string(), &path_ab).unwrap(),
                PathLookup::Absent
            );

            let empty_source = source(kind, Vec::new(), Vec::new());
            assert_eq!(
                lookup_path(
                    kind,
                    &empty_source,
                    &empty_tree_id(kind).unwrap().to_string(),
                    &["a".to_owned()]
                )
                .unwrap(),
                PathLookup::Absent
            );

            for mode in [
                TreeItemMode::Blob,
                TreeItemMode::BlobExecutable,
                TreeItemMode::Link,
                TreeItemMode::Commit,
            ] {
                let root = build_tree(kind, vec![item(kind, mode, "a", b"not-a-tree")]).unwrap();
                let source = source(
                    kind,
                    vec![(root.id.to_string(), tree_bytes(&root))],
                    Vec::new(),
                );
                assert_eq!(
                    lookup_path(kind, &source, &root.id.to_string(), &["a".to_owned()]).unwrap(),
                    PathLookup::NotTree
                );
            }

            let root_file =
                build_tree(kind, vec![item(kind, TreeItemMode::Blob, "a", b"file")]).unwrap();
            let source_file = source(
                kind,
                vec![(root_file.id.to_string(), tree_bytes(&root_file))],
                Vec::new(),
            );
            assert_eq!(
                lookup_path(kind, &source_file, &root_file.id.to_string(), &path_ab).unwrap(),
                PathLookup::NotTree
            );

            let a_file =
                build_tree(kind, vec![item(kind, TreeItemMode::Blob, "b", b"b-file")]).unwrap();
            let root_tree = build_tree(
                kind,
                vec![TreeItem {
                    mode: TreeItemMode::Tree,
                    id: a_file.id,
                    name: "a".to_owned(),
                }],
            )
            .unwrap();
            let source_tree = source(
                kind,
                vec![
                    (root_tree.id.to_string(), tree_bytes(&root_tree)),
                    (a_file.id.to_string(), tree_bytes(&a_file)),
                ],
                Vec::new(),
            );
            assert_eq!(
                lookup_path(kind, &source_tree, &root_tree.id.to_string(), &path_ab).unwrap(),
                PathLookup::NotTree
            );
        }
    }

    #[test]
    fn lookup_path_empty_path_selects_root() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            let root = object_id(kind, b"root").to_string();
            let source = source(kind, Vec::new(), vec![root.clone()]);
            assert_eq!(
                lookup_path(kind, &source, &root, &[]).unwrap(),
                PathLookup::Tree(root)
            );
        }
    }

    #[test]
    fn lookup_path_read_error_propagates() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            let root_id = object_id(kind, b"root-id").to_string();
            for (source, reason) in [
                (
                    source(kind, Vec::new(), vec![root_id.clone()]),
                    MissingObjectReason::Absent,
                ),
                (
                    source(kind, Vec::new(), Vec::new()),
                    MissingObjectReason::Unprefetched,
                ),
            ] {
                let expected = MissingObject {
                    tree_id: root_id.clone(),
                    reason,
                };
                assert_eq!(read_tree(kind, &source, &root_id).unwrap_err(), expected);
                assert_eq!(
                    lookup_path(kind, &source, &root_id, &["a".to_owned()]).unwrap_err(),
                    expected
                );
            }

            let child_id = object_id(kind, b"child-id").to_string();
            let root = build_tree(
                kind,
                vec![TreeItem {
                    mode: TreeItemMode::Tree,
                    id: ObjectHash::from_hex_for_kind(kind, &child_id).unwrap(),
                    name: "a".to_owned(),
                }],
            )
            .unwrap();
            let root_entry = (root.id.to_string(), tree_bytes(&root));
            let mut truncated = valid_tree_bytes(kind, b"child");
            truncated.pop();
            let duplicate = [
                raw_item(b"100644", "b", object_id(kind, b"b-file")),
                raw_item(b"40000", "b", object_id(kind, b"b-tree")),
            ]
            .concat();
            for (source, reason) in [
                (
                    source(kind, vec![root_entry.clone()], vec![child_id.clone()]),
                    MissingObjectReason::Absent,
                ),
                (
                    source(kind, vec![root_entry.clone()], Vec::new()),
                    MissingObjectReason::Unprefetched,
                ),
                (
                    source(
                        kind,
                        vec![root_entry.clone(), (child_id.clone(), truncated)],
                        Vec::new(),
                    ),
                    MissingObjectReason::Malformed,
                ),
                (
                    source(
                        kind,
                        vec![root_entry.clone(), (child_id.clone(), duplicate)],
                        Vec::new(),
                    ),
                    MissingObjectReason::Malformed,
                ),
            ] {
                let expected = MissingObject {
                    tree_id: child_id.clone(),
                    reason,
                };
                assert_eq!(read_tree(kind, &source, &child_id).unwrap_err(), expected);
                assert_eq!(
                    lookup_path(
                        kind,
                        &source,
                        &root.id.to_string(),
                        &["a".to_owned(), "b".to_owned()],
                    )
                    .unwrap_err(),
                    expected
                );
            }
        }
    }

    #[test]
    fn empty_tree_id_per_kind() {
        for (kind, expected) in [
            (HashKind::Sha1, "4b825dc642cb6eb9a060e54bf8d69288fbee4904"),
            (
                HashKind::Sha256,
                "6ef19b41225c5369f1c104d45d8d85efa9b057b53b14b4b9b939dd74decc5321",
            ),
        ] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            assert_eq!(empty_tree_id(kind).unwrap().to_string(), expected);
            assert_eq!(
                build_tree(kind, Vec::new()).unwrap().id.to_string(),
                expected
            );
        }
    }

    #[test]
    fn build_tree_sorted_and_checked() {
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let _guard = set_hash_kind_for_test(opposite(kind));
            let directory = empty_tree_id(kind).unwrap();
            let foo_txt = object_id(kind, b"foo.txt");
            let foo_bar = object_id(kind, b"foo-bar");
            let tree = build_tree(
                kind,
                vec![
                    TreeItem {
                        mode: TreeItemMode::Tree,
                        id: directory,
                        name: "foo".to_owned(),
                    },
                    TreeItem {
                        mode: TreeItemMode::Blob,
                        id: foo_txt,
                        name: "foo.txt".to_owned(),
                    },
                    TreeItem {
                        mode: TreeItemMode::Blob,
                        id: foo_bar,
                        name: "foo-bar".to_owned(),
                    },
                ],
            )
            .unwrap();
            let expected_bytes = [
                raw_item(b"100644", "foo-bar", foo_bar),
                raw_item(b"100644", "foo.txt", foo_txt),
                raw_item(b"40000", "foo", directory),
            ]
            .concat();
            let expected_id =
                ObjectHash::from_type_and_data_for_kind(kind, ObjectType::Tree, &expected_bytes)
                    .unwrap();
            assert_eq!(tree.to_data().unwrap(), expected_bytes);
            assert_eq!(tree.id, expected_id);
            assert_eq!(
                tree.tree_items
                    .iter()
                    .map(|item| item.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["foo-bar", "foo.txt", "foo"]
            );

            assert!(matches!(
                build_tree(
                    kind,
                    vec![
                        TreeItem {
                            mode: TreeItemMode::Tree,
                            id: directory,
                            name: "foo".to_owned(),
                        },
                        TreeItem {
                            mode: TreeItemMode::Blob,
                            id: object_id(kind, b"duplicate-directory"),
                            name: "foo".to_owned(),
                        },
                        TreeItem {
                            mode: TreeItemMode::Blob,
                            id: foo_txt,
                            name: "foo.txt".to_owned(),
                        },
                        TreeItem {
                            mode: TreeItemMode::Blob,
                            id: foo_bar,
                            name: "foo-bar".to_owned(),
                        },
                    ],
                ),
                Err(BuildTreeError::DuplicateName(name)) if name == "foo"
            ));
            assert!(matches!(
                build_tree(
                    kind,
                    vec![
                        item(kind, TreeItemMode::Blob, "same", b"same-one"),
                        item(kind, TreeItemMode::Blob, "same", b"same-two"),
                    ],
                ),
                Err(BuildTreeError::DuplicateName(name)) if name == "same"
            ));
            assert!(matches!(
                build_tree(
                    kind,
                    vec![TreeItem {
                        mode: TreeItemMode::Blob,
                        id: object_id(opposite(kind), b"foreign-kind"),
                        name: "foreign".to_owned(),
                    }],
                ),
                Err(BuildTreeError::InvalidTree(_))
            ));
        }
    }
}
