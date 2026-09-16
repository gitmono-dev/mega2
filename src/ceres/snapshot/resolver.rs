//! Pure path resolution over a fixed git tree (spec 02 §3 Resolve).
//!
//! The resolver only walks pinned tree OIDs. Every "not found" is proven by
//! enumerating the parent directory — network or storage failures are errors,
//! never absence.

use git_internal::internal::object::tree::{Tree, TreeItemMode};

use crate::ceres::snapshot::error::{SnapshotError, SnapshotErrorCode};

/// Filesystem kind per spec 02 §1 / 05 §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Regular,
    Executable,
    Symlink,
    Directory,
}

impl FsKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FsKind::Regular => "regular",
            FsKind::Executable => "executable",
            FsKind::Symlink => "symlink",
            FsKind::Directory => "directory",
        }
    }

    pub fn from_git_mode(mode: TreeItemMode) -> Option<FsKind> {
        match mode {
            TreeItemMode::Tree => Some(FsKind::Directory),
            TreeItemMode::Blob => Some(FsKind::Regular),
            TreeItemMode::BlobExecutable => Some(FsKind::Executable),
            TreeItemMode::Link => Some(FsKind::Symlink),
            // gitlink: rejected at the projection level (spec 07 §1); the
            // resolver surfaces it so callers can reject the scope instead of
            // silently dropping the entry.
            TreeItemMode::Commit => None,
        }
    }
}

/// Position of a resolved node in the fixed tree.
#[derive(Debug, Clone)]
pub struct ResolvedNode {
    pub name: String,
    /// Scope-relative path, "/"-prefixed.
    pub rel_path: String,
    pub fs_kind: FsKind,
    /// Raw git object id (tree oid for directories, blob oid for files).
    pub oid: String,
}

#[derive(Debug, Clone)]
pub enum ResolveOutcome {
    Found(ResolvedNode),
    /// The parent directory exists and was enumerated; the name is absent.
    AbsentProven,
}

/// Walk `rel_path` top-down, invoking `descend` to fetch each child tree.
/// Keeps the resolver pure while letting the caller decide how trees are
/// loaded (DB, cache, object store).
pub fn walk_tree<F>(
    root: &Tree,
    rel_path: &str,
    descend: &mut F,
) -> Result<ResolveOutcome, SnapshotError>
where
    F: FnMut(&str) -> Result<Tree, SnapshotError>,
{
    // Absence of a name in an enumerated directory is proven absence
    // (spec 11 lookup semantics): it is an outcome, never an error.
    let walk_level = |current: &Tree, comps: &[&str]| -> Result<ResolveOutcome, SnapshotError> {
        let Some(item) = current.tree_items.iter().find(|x| x.name == comps[0]) else {
            return Ok(ResolveOutcome::AbsentProven);
        };
        let kind = FsKind::from_git_mode(item.mode).ok_or_else(|| {
            SnapshotError::new(
                SnapshotErrorCode::UnsupportedEntry,
                "gitlink entries are not supported in this profile",
            )
        })?;
        if comps.len() == 1 {
            return Ok(ResolveOutcome::Found(ResolvedNode {
                name: comps[0].to_string(),
                rel_path: rel_path.to_string(),
                fs_kind: kind,
                oid: item.id.to_string(),
            }));
        }
        if kind != FsKind::Directory {
            return Err(SnapshotError::new(
                SnapshotErrorCode::NotDirectory,
                "intermediate component is not a directory",
            ));
        }
        Ok(ResolveOutcome::Found(ResolvedNode {
            name: comps[0].to_string(),
            rel_path: rel_path.to_string(),
            fs_kind: kind,
            oid: item.id.to_string(),
        }))
    };

    let comps: Vec<&str> = rel_path[1..].split('/').collect();
    let (head, rest): (Vec<&str>, Vec<&str>) = (comps[..1].to_vec(), comps[1..].to_vec());
    match walk_level(root, &head)? {
        ResolveOutcome::AbsentProven => Ok(ResolveOutcome::AbsentProven),
        ResolveOutcome::Found(node) if rest.is_empty() => Ok(ResolveOutcome::Found(node)),
        ResolveOutcome::Found(node) if node.fs_kind == FsKind::Directory => {
            let child = descend(&node.oid)?;
            let child_path = format!("/{}", rest.join("/"));
            walk_tree(&child, &child_path, descend)
        }
        ResolveOutcome::Found(_) => Err(SnapshotError::new(
            SnapshotErrorCode::NotDirectory,
            "intermediate component is not a directory",
        )),
    }
}

/// Enumerate direct entries of a directory tree as (name, kind, oid),
/// sorted by raw UTF-8 name bytes (spec 04 §5 ordering).
pub fn direct_entries(tree: &Tree) -> Result<Vec<(String, FsKind, String)>, SnapshotError> {
    let mut out = Vec::with_capacity(tree.tree_items.len());
    for item in &tree.tree_items {
        let kind = FsKind::from_git_mode(item.mode).ok_or_else(|| {
            SnapshotError::new(
                SnapshotErrorCode::UnsupportedEntry,
                format!(
                    "gitlink entry '{}' is not supported in this profile",
                    item.name
                ),
            )
        })?;
        out.push((item.name.clone(), kind, item.id.to_string()));
    }
    out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use git_internal::{
        hash::ObjectHash,
        internal::object::{
            blob::Blob,
            tree::{Tree, TreeItem, TreeItemMode},
        },
    };

    use super::*;

    fn blob(name: &str, content: &[u8]) -> (TreeItem, Blob) {
        let b = Blob::from_content(String::from_utf8(content.to_vec()).unwrap().as_str());
        let item = TreeItem::new(TreeItemMode::Blob, b.id, name.to_string());
        (item, b)
    }

    fn tree(items: Vec<TreeItem>) -> Tree {
        Tree::from_tree_items(items).unwrap()
    }

    #[test]
    fn resolves_nested_path_and_proves_absence() {
        let (leaf_item, _leaf) = blob("leaf.txt", b"hello");
        let sub = tree(vec![leaf_item]);
        let sub_item = TreeItem::new(TreeItemMode::Tree, sub.id, "sub".to_string());
        let root = tree(vec![sub_item]);

        let got = walk_tree(&root, "/sub/leaf.txt", &mut |oid| {
            assert_eq!(oid, &sub.id.to_string());
            Ok(sub.clone())
        })
        .unwrap();
        match got {
            ResolveOutcome::Found(n) => {
                assert_eq!(n.name, "leaf.txt");
                assert_eq!(n.fs_kind, FsKind::Regular);
            }
            _ => panic!("expected found"),
        }

        // Proving absence of /sub/missing.txt requires fetching the parent
        // (sub) to enumerate it; /nope/x fails before any descent.
        let descended = std::cell::Cell::new(0);
        let sub_for_absence = sub.clone();
        match walk_tree(&root, "/sub/missing.txt", &mut |oid| {
            assert_eq!(oid, &sub_for_absence.id.to_string());
            descended.set(descended.get() + 1);
            Ok(sub_for_absence.clone())
        })
        .unwrap()
        {
            ResolveOutcome::AbsentProven => {}
            _ => panic!("expected absent"),
        }
        assert_eq!(descended.get(), 1);
        // /nope is absent in the enumerated root: proven without descent.
        let descended2 = std::cell::Cell::new(0);
        match walk_tree(&root, "/nope/x", &mut |_| {
            descended2.set(descended2.get() + 1);
            unreachable!("no descent once the parent is proven absent");
        })
        .unwrap()
        {
            ResolveOutcome::AbsentProven => {}
            _ => panic!("expected absent"),
        }
        assert_eq!(descended2.get(), 0);
    }

    #[test]
    fn gitlink_is_rejected_not_dropped() {
        let link = TreeItem::new(
            TreeItemMode::Commit,
            ObjectHash::new(b"gitlink"),
            "mod".to_string(),
        );
        let root = tree(vec![link]);
        let err = walk_tree(&root, "/mod", &mut |_| unreachable!()).unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::UnsupportedEntry);
        let err = direct_entries(&root).unwrap_err();
        assert_eq!(err.code, SnapshotErrorCode::UnsupportedEntry);
    }

    #[test]
    fn direct_entries_sorted_by_bytes() {
        let (a, _) = blob("B.txt", b"1");
        let (b, _) = blob("a.txt", b"2");
        let root = tree(vec![a, b]);
        let names: Vec<String> = direct_entries(&root)
            .unwrap()
            .into_iter()
            .map(|e| e.0)
            .collect();
        // 'B' (0x42) sorts before 'a' (0x61) in raw byte order.
        assert_eq!(names, vec!["B.txt", "a.txt"]);
    }
}
