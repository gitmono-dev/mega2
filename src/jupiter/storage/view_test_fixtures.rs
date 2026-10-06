//! Shared database fixtures for history-projection storage tests.

use std::{collections::BTreeMap, sync::Arc};

use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::object::{
        blob::Blob,
        commit::Commit,
        tree::{Tree, TreeItem, TreeItemMode},
    },
};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, Value};

use crate::{
    common::utils::{MEGA_BRANCH_NAME, generate_id},
    jupiter::{
        storage::{
            base_storage::{BaseStorage, StorageConnector},
            mono_storage::MonoStorage,
        },
        utils::converter::sort_git_tree_items,
    },
};

/// One real root tree and every object needed to traverse it in storage tests.
/// Blobs deliberately remain in memory: callers that need blob content choose
/// how to persist it for their own test.
#[derive(Clone, Debug)]
pub(crate) struct RootTreeFixture {
    pub root: Tree,
    pub trees: Vec<Tree>,
    pub blobs: Vec<Blob>,
}

/// A committed root-history entry plus the tree and blob objects used to make
/// it. The tree list includes the commit root tree; blobs are returned but not
/// written to mega_blob.
#[derive(Clone, Debug)]
pub(crate) struct RootCommitFixture {
    pub commit: Commit,
    pub trees: Vec<Tree>,
    pub blobs: Vec<Blob>,
}

fn default_root_tree(kind: HashKind, label: &str) -> RootTreeFixture {
    root_tree_from_paths(
        kind,
        &[(
            "fixture-root.txt".to_owned(),
            format!("history-projection fixture {label}").into_bytes(),
        )],
    )
}

#[derive(Default)]
struct TreeNode {
    blobs: BTreeMap<String, Blob>,
    children: BTreeMap<String, TreeNode>,
}

/// Builds a root tree from path-to-bytes inputs with explicit object-hash
/// selection. Paths must be unique relative file paths and may create nested
/// trees. It returns blobs without writing them to storage.
pub(crate) fn root_tree_from_paths(kind: HashKind, paths: &[(String, Vec<u8>)]) -> RootTreeFixture {
    assert!(
        !paths.is_empty(),
        "root tree fixture requires at least one file"
    );

    let mut node = TreeNode::default();
    let mut blobs = Vec::with_capacity(paths.len());
    for (path, contents) in paths {
        let parts: Vec<_> = path.split('/').collect();
        assert!(
            !path.is_empty() && parts.iter().all(|part| !part.is_empty()),
            "fixture path must be a non-empty relative path: {path:?}"
        );
        let (name, parents) = parts
            .split_last()
            .expect("non-empty fixture paths have a final component");
        let mut current = &mut node;
        for parent in parents {
            assert!(
                !current.blobs.contains_key(*parent),
                "fixture path uses file as a directory: {path:?}"
            );
            current = current.children.entry((*parent).to_owned()).or_default();
        }
        assert!(
            !current.children.contains_key(*name),
            "fixture path uses directory as a file: {path:?}"
        );
        let blob = Blob::from_content_bytes_with_kind(kind, contents.clone())
            .expect("fixture blob uses an explicit supported hash kind");
        assert!(
            current
                .blobs
                .insert((*name).to_owned(), blob.clone())
                .is_none(),
            "duplicate fixture path: {path:?}"
        );
        blobs.push(blob);
    }

    let mut trees = Vec::new();
    let root = build_tree_node(kind, node, &mut trees);
    RootTreeFixture { root, trees, blobs }
}

fn build_tree_node(kind: HashKind, node: TreeNode, trees: &mut Vec<Tree>) -> Tree {
    let mut items = Vec::with_capacity(node.blobs.len() + node.children.len());
    for (name, blob) in node.blobs {
        items.push(TreeItem::new(TreeItemMode::Blob, blob.id, name));
    }
    for (name, child) in node.children {
        let tree = build_tree_node(kind, child, trees);
        items.push(TreeItem::new(TreeItemMode::Tree, tree.id, name));
    }
    sort_git_tree_items(&mut items);
    let tree = Tree::from_tree_items_with_kind(kind, items)
        .expect("fixture tree uses an explicit supported hash kind");
    trees.push(tree.clone());
    tree
}

/// Persists one commit and all of its tree layers. It deliberately does not
/// persist its returned blob objects.
async fn persist_root_commit(
    db: &DatabaseConnection,
    kind: HashKind,
    tree: RootTreeFixture,
    parents: Vec<ObjectHash>,
    message: &str,
) -> RootCommitFixture {
    let commit = Commit::from_tree_id_with_kind(kind, tree.root.id, parents, message)
        .expect("fixture commit uses an explicit supported hash kind");
    let mono = MonoStorage {
        base: BaseStorage::new(Arc::new(db.clone())),
    };
    mono.save_mega_trees(tree.trees.clone(), commit.id, None)
        .await
        .expect("persist fixture trees");
    mono.save_mega_commits(vec![commit.clone()], None)
        .await
        .expect("persist fixture commit");
    RootCommitFixture {
        commit,
        trees: tree.trees,
        blobs: tree.blobs,
    }
}

/// Persists a caller-built root tree list as a linear history and points root
/// main to its final commit.
pub(crate) async fn seed_linear_root_history_with_trees(
    db: &DatabaseConnection,
    kind: HashKind,
    trees: Vec<RootTreeFixture>,
) -> Vec<RootCommitFixture> {
    assert!(!trees.is_empty(), "linear history needs a root commit");
    let mut commits = Vec::with_capacity(trees.len());
    let mut parent = None;
    for (index, tree) in trees.into_iter().enumerate() {
        let parents = parent.into_iter().collect();
        let fixture = persist_root_commit(
            db,
            kind,
            tree,
            parents,
            &format!("fixture root commit {}", index + 1),
        )
        .await;
        parent = Some(fixture.commit.id);
        commits.push(fixture);
    }
    insert_fixture_main(
        db,
        &commits[0].commit.id.to_string(),
        &commits[0].commit.tree_id.to_string(),
    )
    .await;
    let tip = commits.last().expect("linear history is not empty");
    set_fixture_main(
        db,
        &tip.commit.id.to_string(),
        &tip.commit.tree_id.to_string(),
    )
    .await;
    commits
}

/// Builds caller-provided path content into every root tree in a linear
/// history. All object constructors receive kind explicitly.
pub(crate) async fn seed_linear_root_history_with_paths(
    db: &DatabaseConnection,
    kind: HashKind,
    commits: usize,
    paths: &[(String, Vec<u8>)],
) -> Vec<RootCommitFixture> {
    assert!(commits > 0, "linear history needs a root commit");
    let trees = (0..commits)
        .map(|_| root_tree_from_paths(kind, paths))
        .collect();
    seed_linear_root_history_with_trees(db, kind, trees).await
}

/// Persists one single-parent root commit with a default root tree without
/// moving root main. Callers can use [`cas_fixture_main`] to choose when it
/// becomes the active root.
pub(crate) async fn seed_single_parent_root_commit(
    db: &DatabaseConnection,
    kind: HashKind,
    parent: &RootCommitFixture,
    message: &str,
) -> RootCommitFixture {
    seed_single_parent_root_commit_with_tree(
        db,
        kind,
        default_root_tree(kind, message),
        parent,
        message,
    )
    .await
}

/// Persists one single-parent root commit using the caller's tree without
/// moving root main.
pub(crate) async fn seed_single_parent_root_commit_with_tree(
    db: &DatabaseConnection,
    kind: HashKind,
    tree: RootTreeFixture,
    parent: &RootCommitFixture,
    message: &str,
) -> RootCommitFixture {
    persist_root_commit(db, kind, tree, vec![parent.commit.id], message).await
}

/// Builds a linear history whose default one-file root tree changes per
/// commit, so each root commit contains a distinct object graph.
pub(crate) async fn seed_linear_root_history(
    db: &DatabaseConnection,
    commits: usize,
) -> Vec<RootCommitFixture> {
    assert!(commits > 0, "linear history needs a root commit");
    let kind = HashKind::Sha1;
    let trees = (1..=commits)
        .map(|number| {
            root_tree_from_paths(
                kind,
                &[(
                    format!("fixture-{number}.txt"),
                    format!("fixture root {number}").into_bytes(),
                )],
            )
        })
        .collect();
    seed_linear_root_history_with_trees(db, kind, trees).await
}

/// Persists a merge-shaped root commit with a default root tree without moving
/// main; callers can update it through [`cas_fixture_main`].
pub(crate) async fn seed_multi_parent_root_commit(
    db: &DatabaseConnection,
    kind: HashKind,
    first_parent: &RootCommitFixture,
    second_parent: &RootCommitFixture,
) -> RootCommitFixture {
    seed_multi_parent_root_commit_with_tree(
        db,
        kind,
        default_root_tree(kind, "merge"),
        first_parent,
        second_parent,
    )
    .await
}

/// Persists a merge-shaped root commit using the caller's tree without moving
/// main.
pub(crate) async fn seed_multi_parent_root_commit_with_tree(
    db: &DatabaseConnection,
    kind: HashKind,
    tree: RootTreeFixture,
    first_parent: &RootCommitFixture,
    second_parent: &RootCommitFixture,
) -> RootCommitFixture {
    persist_root_commit(
        db,
        kind,
        tree,
        vec![first_parent.commit.id, second_parent.commit.id],
        "fixture merge root commit",
    )
    .await
}

/// Persists a commit with a default root tree whose declared first parent is
/// intentionally absent from mega_commit.
pub(crate) async fn seed_missing_first_parent_root_commit(
    db: &DatabaseConnection,
    kind: HashKind,
    missing_parent: ObjectHash,
) -> RootCommitFixture {
    seed_missing_first_parent_root_commit_with_tree(
        db,
        kind,
        default_root_tree(kind, "missing-first-parent"),
        missing_parent,
    )
    .await
}

/// Persists a commit whose declared first parent is intentionally absent from
/// mega_commit, using the caller's tree.
pub(crate) async fn seed_missing_first_parent_root_commit_with_tree(
    db: &DatabaseConnection,
    kind: HashKind,
    tree: RootTreeFixture,
    missing_parent: ObjectHash,
) -> RootCommitFixture {
    persist_root_commit(
        db,
        kind,
        tree,
        vec![missing_parent],
        "fixture missing-first-parent root commit",
    )
    .await
}

/// Persists a parentless, unrelated root history with a default tree without
/// changing main.
pub(crate) async fn seed_unrelated_root_history(
    db: &DatabaseConnection,
    kind: HashKind,
) -> RootCommitFixture {
    seed_unrelated_root_history_with_tree(db, kind, default_root_tree(kind, "unrelated")).await
}

/// Persists a parentless, unrelated root history using the caller's tree
/// without changing main.
pub(crate) async fn seed_unrelated_root_history_with_tree(
    db: &DatabaseConnection,
    kind: HashKind,
    tree: RootTreeFixture,
) -> RootCommitFixture {
    persist_root_commit(db, kind, tree, Vec::new(), "fixture unrelated root commit").await
}

/// Moves root main only when its old commit and tree still match. This also
/// models a fixture rollback when next is an ancestor.
pub(crate) async fn cas_fixture_main(
    db: &DatabaseConnection,
    expected: &RootCommitFixture,
    next: &RootCommitFixture,
) -> bool {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mega_refs SET ref_commit_hash = $1, ref_tree_hash = $2, updated_at = now() \
         WHERE path = '/' AND ref_name = $3 AND ref_commit_hash = $4 AND ref_tree_hash = $5",
        [
            Value::from(next.commit.id.to_string()),
            Value::from(next.commit.tree_id.to_string()),
            Value::from(MEGA_BRANCH_NAME.to_owned()),
            Value::from(expected.commit.id.to_string()),
            Value::from(expected.commit.tree_id.to_string()),
        ],
    ))
    .await
    .expect("CAS fixture main")
    .rows_affected()
        == 1
}

async fn insert_fixture_main(db: &DatabaseConnection, commit_id: &str, tree_id: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO mega_refs \
         (id, path, ref_name, ref_commit_hash, ref_tree_hash, created_at, updated_at, is_cl) \
         VALUES ($1, '/', $2, $3, $4, now(), now(), false)",
        [
            Value::from(generate_id()),
            Value::from(MEGA_BRANCH_NAME.to_owned()),
            Value::from(commit_id.to_owned()),
            Value::from(tree_id.to_owned()),
        ],
    ))
    .await
    .expect("insert fixture main ref");
}

/// Moves root main with a compare-free fixture update. Production paths use
/// their own CAS helper; fixtures use this only to construct history shapes.
pub(crate) async fn set_fixture_main(db: &DatabaseConnection, commit_id: &str, tree_id: &str) {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE mega_refs SET ref_commit_hash = $1, ref_tree_hash = $2 \
         WHERE path = '/' AND ref_name = $3",
        [
            Value::from(commit_id.to_owned()),
            Value::from(tree_id.to_owned()),
            Value::from(MEGA_BRANCH_NAME.to_owned()),
        ],
    ))
    .await
    .expect("update fixture main ref");
}

#[cfg(test)]
mod tests {
    use git_internal::internal::object::ObjectTrait;
    use sea_orm::{DbBackend, Statement};

    use super::*;
    use crate::jupiter::{migration::apply_migrations, tests::test_db_connection};

    #[test]
    fn root_tree_fixture_builds_nested_explicit_kind_objects() {
        let fixture = root_tree_from_paths(
            HashKind::Sha1,
            &[
                ("README.md".to_owned(), b"root".to_vec()),
                ("dir/file.txt".to_owned(), b"nested".to_vec()),
            ],
        );
        assert_eq!(fixture.blobs.len(), 2);
        assert_eq!(fixture.trees.len(), 2);
        assert_eq!(fixture.root.id.kind(), HashKind::Sha1);
        assert!(!fixture.root.to_data().unwrap().is_empty());
        assert!(
            fixture
                .root
                .tree_items
                .iter()
                .any(|item| item.name == "dir")
        );
    }

    #[tokio::test]
    async fn history_fixtures_persist_tree_layers_and_model_history_shapes() {
        let temp = tempfile::tempdir().unwrap();
        let db = test_db_connection(temp.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let kind = HashKind::Sha1;
        let commits = seed_linear_root_history_with_trees(
            &db,
            kind,
            vec![
                root_tree_from_paths(kind, &[("one.txt".to_owned(), b"one".to_vec())]),
                root_tree_from_paths(kind, &[("two.txt".to_owned(), b"two".to_vec())]),
            ],
        )
        .await;
        assert_eq!(commits.len(), 2);
        assert_eq!(
            commits[1].commit.parent_commit_ids,
            vec![commits[0].commit.id]
        );
        assert!(cas_fixture_main(&db, &commits[1], &commits[0]).await);
        assert!(!cas_fixture_main(&db, &commits[1], &commits[0]).await);

        let merge = seed_multi_parent_root_commit_with_tree(
            &db,
            kind,
            root_tree_from_paths(kind, &[("merge.txt".to_owned(), b"merge".to_vec())]),
            &commits[1],
            &commits[0],
        )
        .await;
        assert_eq!(merge.commit.parent_commit_ids.len(), 2);
        let missing_parent = Blob::from_content_with_kind(kind, "missing").unwrap().id;
        let missing = seed_missing_first_parent_root_commit_with_tree(
            &db,
            kind,
            root_tree_from_paths(kind, &[("missing.txt".to_owned(), b"missing".to_vec())]),
            missing_parent,
        )
        .await;
        assert_eq!(missing.commit.parent_commit_ids, vec![missing_parent]);
        let unrelated = seed_unrelated_root_history_with_tree(
            &db,
            kind,
            root_tree_from_paths(kind, &[("other.txt".to_owned(), b"other".to_vec())]),
        )
        .await;
        assert!(unrelated.commit.parent_commit_ids.is_empty());

        let tree_count: i64 = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*)::bigint AS count FROM mega_tree".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "count")
            .unwrap();
        let blob_count: i64 = db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT count(*)::bigint AS count FROM mega_blob".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "count")
            .unwrap();
        assert_eq!(tree_count, 5);
        assert_eq!(blob_count, 0);
    }
}
