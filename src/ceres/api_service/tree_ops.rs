use std::{
    collections::VecDeque,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use git_internal::{
    errors::GitError,
    internal::object::{
        ObjectTrait,
        blob::Blob,
        tree::{Tree, TreeItem, TreeItemMode},
    },
};

use crate::{
    ceres::{
        api_service::ApiHandler,
        model::git::{TreeBriefItem, TreeCommitItem, TreeHashItem},
    },
    common::errors::MegaError,
    jupiter::utils::converter::generate_git_keep_with_timestamp,
};

pub async fn get_tree_commit_info<T: ApiHandler + ?Sized>(
    handler: &T,
    path: PathBuf,
    refs: Option<&str>,
) -> Result<Vec<TreeCommitItem>, GitError> {
    // Use refs-aware commit mapping to get individual commit info for each file/directory
    // This ensures each item shows its own last modification commit, not just the tag commit
    let commit_map = handler.item_to_commit_map(path, refs).await?;
    let mut items: Vec<TreeCommitItem> = commit_map.into_iter().map(TreeCommitItem::from).collect();
    items.sort_by(|a, b| {
        a.content_type
            .cmp(&b.content_type)
            .then(a.name.cmp(&b.name))
    });
    Ok(items)
}

pub async fn get_binary_tree_by_path<T: ApiHandler + ?Sized>(
    handler: &T,
    path: &Path,
    oid: Option<String>,
) -> Result<Vec<u8>, MegaError> {
    let Some(tree) = search_tree_by_path(handler, path, None).await? else {
        return Ok(vec![]);
    };
    if let Some(oid) = oid
        && oid != tree.id._to_string()
    {
        return Ok(vec![]);
    }
    Ok(tree.to_data()?)
}

/// Searches for a tree by a given path and refs (commit SHA or tag name). If refs is None/empty, use default root.
///
/// This function takes a `path` and searches for the corresponding tree
/// in the repository. It returns a `Result` containing an `Option<Tree>`.
/// If the tree is found, it returns `Some(Tree)`. If the path does not
/// exist, it returns `None`. In case of an error, it returns a `GitError`.
///
/// # Arguments
///
/// * `path` - A reference to the `Path` to search for the tree.
/// * `refs` - Optional commit SHA or tag name to search within. If None or empty, uses the default root.
///
/// # Returns
///
/// * `Result<Option<Tree>, GitError>` - A result containing an optional tree or a Git error.
pub async fn search_tree_by_path<T: ApiHandler + ?Sized>(
    handler: &T,
    path: &Path,
    refs: Option<&str>,
) -> Result<Option<Tree>, MegaError> {
    let relative_path = handler
        .strip_relative(path)
        .map_err(|e| MegaError::Other(e.to_string()))?;
    let root_tree = handler.get_root_tree(refs).await?;
    let mut search_tree = root_tree.clone();
    for component in relative_path.components() {
        // root tree already found
        if component != Component::RootDir {
            let target_name = component.as_os_str().to_str().unwrap();
            let search_res = search_tree
                .tree_items
                .iter()
                .find(|x| x.name == target_name);
            if let Some(search_res) = search_res {
                if !search_res.is_tree() {
                    return Ok(None);
                }
                let res = handler.get_tree_by_hash(&search_res.id.to_string()).await?;
                search_tree = res.clone();
            } else {
                return Ok(None);
            }
        }
    }
    Ok(Some(search_tree))
}

/// Searches for a tree in the Git repository by its path, creating intermediate trees if necessary,
/// and returns the trees involved in the update process.
///
/// # Arguments
///
/// * `path` - A reference to the path to search for.
///
/// # Returns
///
/// A vector of trees involved in the update process.
///
/// # Errors
///
/// Returns a `MegaError` if an error occurs during the search or tree creation process.
///
/// The returned [`Blob`] is the placeholder `.gitkeep` referenced by the new leaf tree.
/// Callers **must** persist it via object storage (`save_blobs` / `put_objects`) before
/// or with the trees; otherwise clone/fetch will 404 on that object.
/// Ported from mega@f5d22b9 `ceres/src/application/api_service/tree_ops.rs` (#2152).
pub async fn search_and_create_tree<T: ApiHandler + ?Sized>(
    handler: &T,
    path: &Path,
) -> Result<(VecDeque<Tree>, Blob), MegaError> {
    let relative_path = handler.strip_relative(path)?;
    let root_tree = handler.get_root_tree(None).await?;
    let mut search_tree = root_tree.clone();
    let mut update_item_tree = VecDeque::new();
    update_item_tree.push_back((root_tree, Component::RootDir));
    let mut saving_trees = VecDeque::new();
    let mut stack: VecDeque<_> = VecDeque::new();

    for component in relative_path.components() {
        if component == Component::RootDir {
            continue;
        }

        let target_name = component.as_os_str().to_str().unwrap();
        if let Some(search_res) = search_tree
            .tree_items
            .iter()
            .find(|x| x.name == target_name)
        {
            search_tree = handler.get_tree_by_hash(&search_res.id.to_string()).await?;
            update_item_tree.push_back((search_tree.clone(), component));
        } else {
            stack.push_back(component);
        }
    }

    let blob = generate_git_keep_with_timestamp();
    let mut last_tree = Tree::from_tree_items(vec![TreeItem {
        mode: TreeItemMode::Blob,
        id: blob.id,
        name: String::from(".gitkeep"),
    }])
    .unwrap();
    let mut last_tree_name = "";
    let mut first_element = true;

    while let Some(component) = stack.pop_back() {
        if first_element {
            first_element = false;
        } else {
            last_tree = Tree::from_tree_items(vec![TreeItem {
                mode: TreeItemMode::Tree,
                id: last_tree.id,
                name: last_tree_name.to_owned(),
            }])
            .unwrap();
        }
        saving_trees.push_back(last_tree.clone());
        last_tree_name = component.as_os_str().to_str().unwrap();
    }

    if let Some((mut new_item_tree, search_name_component)) = update_item_tree.pop_back() {
        new_item_tree.tree_items.push(TreeItem {
            mode: TreeItemMode::Tree,
            id: last_tree.id,
            name: last_tree_name.to_owned(),
        });
        last_tree = Tree::from_tree_items(new_item_tree.tree_items).unwrap();
        saving_trees.push_back(last_tree.clone());

        let mut replace_hash = last_tree.id;
        let mut search_name = search_name_component.as_os_str().to_str().unwrap();
        while let Some((mut tree, component)) = update_item_tree.pop_back() {
            if let Some(index) = tree.tree_items.iter().position(|x| x.name == search_name) {
                tree.tree_items[index].id = replace_hash;
                let new_tree = Tree::from_tree_items(tree.tree_items).unwrap();
                replace_hash = new_tree.id;
                search_name = component.as_os_str().to_str().unwrap();
                saving_trees.push_back(new_tree);
            }
        }
    }

    Ok((saving_trees, blob))
}

/// return the dir's hash only
pub async fn get_tree_dir_hash<T: ApiHandler + ?Sized>(
    handler: &T,
    path: PathBuf,
    dir_name: &str,
    refs: Option<&str>,
) -> Result<Vec<TreeHashItem>, GitError> {
    match search_tree_by_path(handler, &path, refs).await? {
        Some(tree) => {
            let items: Vec<TreeHashItem> = tree
                .tree_items
                .into_iter()
                .filter(|x| x.mode == TreeItemMode::Tree && x.name == dir_name)
                .map(TreeHashItem::from)
                .collect();
            Ok(items)
        }
        None => Ok(Vec::new()),
    }
}

pub async fn get_tree_info<T: ApiHandler + ?Sized>(
    handler: &T,
    path: &Path,
    refs: Option<&str>,
) -> Result<Vec<TreeBriefItem>, GitError> {
    match search_tree_by_path(handler, path, refs).await? {
        Some(tree) => {
            let items = tree
                .tree_items
                .into_iter()
                .map(|item| {
                    let full_path = path.join(&item.name);
                    let mut info: TreeBriefItem = item.into();
                    info.path = full_path.to_str().unwrap().to_owned();
                    info
                })
                .collect();
            Ok(items)
        }
        None => Ok(vec![]),
    }
}

/// Searches for a tree in the Git repository by its path and returns the trees involved in the update and the target tree.
///
/// # Arguments
///
/// * `path` - A reference to the path to search for.
///
/// # Returns
///
/// A tuple containing:
/// - A vector of trees involved in the update process.
/// - The target tree found at the end of the search.
///
/// # Errors
///
/// Returns a `GitError` if the path does not exist.
pub async fn search_tree_for_update<T: ApiHandler + ?Sized>(
    handler: &T,
    path: &Path,
) -> Result<Vec<Arc<Tree>>, GitError> {
    // strip repo root prefix
    let relative_path = handler
        .strip_relative(path)
        .map_err(|e| GitError::CustomError(e.to_string()))?;
    let root_tree = handler.get_root_tree(None).await?;

    // init state
    let mut current_tree = Arc::new(root_tree.clone());
    let mut update_chain = vec![Arc::new(root_tree)];

    for component in relative_path.components() {
        // root tree already found
        if component != Component::RootDir {
            let target_name = component.as_os_str().to_str().unwrap();

            // lookup child
            let search_res = current_tree
                .tree_items
                .iter()
                .find(|x| x.name == target_name)
                .ok_or_else(|| {
                    GitError::CustomError(format!(
                        "Path '{}' not exist, please create path first!",
                        target_name
                    ))
                })?;
            // fetch next tree
            current_tree = Arc::new(handler.get_tree_by_hash(&search_res.id.to_string()).await?);
            update_chain.push(current_tree.clone());
        }
    }
    Ok(update_chain)
}

/// Like [`search_tree_for_update`], but inserts missing `TreeItemMode::Tree`
/// components along the path (create semantics for trunk push).
///
/// Returns `(update_chain, placeholder_blob)` where `update_chain` has the same
/// orientation as [`search_tree_for_update`] (`[root, …, leaf]`). When new
/// components are created, the deepest existing tree in the chain is already
/// rewritten to point at the new child; shallower ancestors remain as loaded so
/// callers can roll hashes up with `update_tree_hash` / `build_result_by_chain`.
/// The optional `.gitkeep` blob is `Some` only when a new leaf was created and
/// must be persisted by the caller.
pub async fn search_tree_for_update_or_create<T: ApiHandler + ?Sized>(
    handler: &T,
    path: &Path,
) -> Result<(Vec<Arc<Tree>>, Option<Blob>), GitError> {
    let relative_path = handler
        .strip_relative(path)
        .map_err(|e| GitError::CustomError(e.to_string()))?;
    let root_tree = handler.get_root_tree(None).await?;

    let mut existing_chain: Vec<Tree> = vec![root_tree.clone()];
    let mut existing_names: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut cursor = root_tree.clone();

    for component in relative_path.components() {
        if component == Component::RootDir {
            continue;
        }
        let target_name = component
            .as_os_str()
            .to_str()
            .ok_or_else(|| GitError::CustomError("Invalid path component".into()))?
            .to_owned();

        if !missing.is_empty() {
            missing.push(target_name);
            continue;
        }

        match cursor.tree_items.iter().find(|x| x.name == target_name) {
            Some(item) if item.mode == TreeItemMode::Tree => {
                existing_names.push(target_name);
                cursor = handler.get_tree_by_hash(&item.id.to_string()).await?;
                existing_chain.push(cursor.clone());
            }
            Some(_) => {
                return Err(GitError::CustomError(format!(
                    "Path '{target_name}' exists but is not a tree"
                )));
            }
            None => {
                missing.push(target_name);
            }
        }
    }

    let blob = generate_git_keep_with_timestamp();

    if missing.is_empty() {
        let chain = existing_chain.into_iter().map(Arc::new).collect();
        // Path already exists: no placeholder blob is referenced.
        return Ok((chain, None));
    }

    let leaf =
        crate::ceres::api_service::mono_api_service::MonoServiceLogic::tree_from_items_checked(
            vec![TreeItem {
                mode: TreeItemMode::Blob,
                id: blob.id,
                name: String::from(".gitkeep"),
            }],
        )?;

    let existing_name_refs: Vec<&str> = existing_names.iter().map(String::as_str).collect();
    let missing_refs: Vec<&str> = missing.iter().map(String::as_str).collect();
    let (_new_root, produced) =
        crate::ceres::api_service::mono_api_service::MonoServiceLogic::ensure_tree_path_with_chain(
            &existing_chain,
            &existing_name_refs,
            &missing_refs,
            leaf,
        )?;

    // `produced` layout from ensure_tree_path_with_chain:
    //   [leaf, new_intermediates..., rewritten_deepest_existing, ..., rewritten_root]
    // Rebuild the search_tree_for_update shape:
    //   [original_root, ..., original ancestors above deepest_existing,
    //    rewritten_deepest_existing, new_intermediates..., leaf]
    // Only the deepest existing tree is pre-rewritten; shallower ancestors stay
    // as loaded so build_result_by_chain / update_tree_hash can roll hashes up.
    let rewritten_count = existing_chain.len();
    let create_count = produced.len().saturating_sub(rewritten_count);
    let rewritten_deepest = produced
        .get(create_count)
        .cloned()
        .ok_or_else(|| GitError::CustomError("missing rewritten deepest existing tree".into()))?;

    let mut update_chain: Vec<Arc<Tree>> = Vec::with_capacity(existing_chain.len() + create_count);
    // Unmodified ancestors from root through parent of deepest existing.
    for tree in existing_chain
        .iter()
        .take(existing_chain.len().saturating_sub(1))
    {
        update_chain.push(Arc::new(tree.clone()));
    }
    update_chain.push(Arc::new(rewritten_deepest));
    // New intermediates then leaf (produced[0..create_count] is leaf-first).
    for tree in produced.iter().take(create_count).rev() {
        update_chain.push(Arc::new(tree.clone()));
    }

    Ok((update_chain, Some(blob)))
}

pub async fn get_tree_content_hash<T: ApiHandler + ?Sized>(
    handler: &T,
    path: PathBuf,
    refs: Option<&str>,
) -> Result<Vec<TreeHashItem>, GitError> {
    match search_tree_by_path(handler, &path, refs).await? {
        Some(tree) => {
            let mut items: Vec<TreeHashItem> = tree
                .tree_items
                .into_iter()
                .map(TreeHashItem::from)
                .collect();

            // sort with type and name
            items.sort_by(|a, b| {
                a.content_type
                    .cmp(&b.content_type)
                    .then(a.name.cmp(&b.name))
            });
            Ok(items)
        }
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Arc};

    use git_internal::internal::object::tree::{TreeItem, TreeItemMode};

    use super::{search_and_create_tree, search_tree_for_update_or_create};
    use crate::{
        ceres::api_service::{
            cache::GitObjectCache,
            mono_api_service::{MonoApiService, MonoServiceLogic},
        },
        config::RedisConfig,
        jupiter::{
            redis::init_connection,
            service::{git_service::GitService, mono_service::MonoService},
            storage::object_storage::mock_object_storage,
            tests::test_storage,
            utils::converter::generate_git_keep_with_timestamp,
        },
    };

    /// Regression coverage for mega@f5d22b9 (#2152): `search_and_create_tree`
    /// returns the placeholder `.gitkeep` blob so callers can persist it together
    /// with the new trees; the leaf tree must reference exactly that blob.
    #[tokio::test]
    async fn search_and_create_tree_returns_gitkeep_blob_referenced_by_leaf_tree() {
        let temp = tempfile::tempdir().unwrap();
        let mut storage = test_storage(temp.path()).await;

        // Wire the services to the real test DB with a shared in-memory object store.
        let git_service = GitService {
            obj_storage: mock_object_storage(),
        };
        storage.git_service = git_service.clone();
        storage.mono_service = MonoService {
            mono_storage: storage.mono_storage(),
            git_service,
        };
        storage
            .mono_service
            .init_monorepo(&storage.config().monorepo)
            .await
            .unwrap();

        let handler = MonoApiService {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection: init_connection(&RedisConfig {
                    url: std::env::var("MEGA_REDIS__URL")
                        .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string()),
                })
                .await
                .expect("redis connection"),
                prefix: "disabled".to_string(),
            }),
        };

        let (trees, blob) = search_and_create_tree(&handler, Path::new("a/b"))
            .await
            .expect("search_and_create_tree on a new path");

        // "a/b" does not exist: leaf tree + intermediate "a" tree + updated root.
        assert_eq!(trees.len(), 3);

        // The leaf tree must reference the returned blob via a `.gitkeep` entry.
        let leaf = trees.front().unwrap();
        let gitkeep = leaf
            .tree_items
            .iter()
            .find(|item| item.name == ".gitkeep")
            .expect("leaf tree must contain a .gitkeep entry");
        assert_eq!(gitkeep.mode, TreeItemMode::Blob);
        assert_eq!(gitkeep.id, blob.id);

        // The updated root must link the new "a" subtree.
        let new_root = trees.back().unwrap();
        assert!(
            new_root
                .tree_items
                .iter()
                .any(|item| item.name == "a" && item.mode == TreeItemMode::Tree)
        );
    }

    fn sample_leaf() -> (
        git_internal::internal::object::blob::Blob,
        git_internal::internal::object::tree::Tree,
    ) {
        let blob = generate_git_keep_with_timestamp();
        let leaf = MonoServiceLogic::tree_from_items_checked(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: blob.id,
            name: String::from(".gitkeep"),
        }])
        .unwrap();
        (blob, leaf)
    }

    #[test]
    fn insert_or_replace_updates_existing_and_inserts_missing() {
        let (_blob, leaf) = sample_leaf();
        let root = MonoServiceLogic::tree_from_items_checked(vec![TreeItem {
            mode: TreeItemMode::Tree,
            id: leaf.id,
            name: "old".into(),
        }])
        .unwrap();

        let replaced =
            MonoServiceLogic::insert_or_replace_tree_hash(Arc::new(root.clone()), "old", leaf.id)
                .unwrap();
        assert_eq!(replaced.tree_items.len(), 1);
        assert_eq!(replaced.tree_items[0].id, leaf.id);

        let inserted =
            MonoServiceLogic::insert_or_replace_tree_hash(Arc::new(root), "new", leaf.id).unwrap();
        assert_eq!(inserted.tree_items.len(), 2);
        assert!(
            inserted
                .tree_items
                .iter()
                .any(|i| i.name == "new" && i.mode == TreeItemMode::Tree)
        );
    }

    #[test]
    fn tree_from_items_checked_rejects_duplicate_names() {
        let (_blob, leaf) = sample_leaf();
        let err = MonoServiceLogic::tree_from_items_checked(vec![
            TreeItem {
                mode: TreeItemMode::Tree,
                id: leaf.id,
                name: "dup".into(),
            },
            TreeItem {
                mode: TreeItemMode::Tree,
                id: leaf.id,
                name: "dup".into(),
            },
        ])
        .expect_err("duplicate names must fail");
        assert!(err.to_string().contains("Duplicate tree item name"));
    }

    #[test]
    fn ensure_tree_path_single_level_under_root() {
        let (_blob, leaf) = sample_leaf();
        let root = MonoServiceLogic::tree_from_items_checked(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: _blob.id,
            name: ".keep".into(),
        }])
        .unwrap();

        let (new_root, produced) =
            MonoServiceLogic::ensure_tree_path_with_chain(&[root], &[], &["a"], leaf.clone())
                .unwrap();
        assert!(
            new_root
                .tree_items
                .iter()
                .any(|i| i.name == "a" && i.id == leaf.id && i.mode == TreeItemMode::Tree)
        );
        assert!(produced.iter().any(|t| t.id == leaf.id));
        assert!(produced.iter().any(|t| t.id == new_root.id));
    }

    #[test]
    fn ensure_tree_path_multi_level_creates_full_chain() {
        let (_blob, leaf) = sample_leaf();
        let root = MonoServiceLogic::tree_from_items_checked(vec![TreeItem {
            mode: TreeItemMode::Blob,
            id: _blob.id,
            name: ".keep".into(),
        }])
        .unwrap();

        let (new_root, produced) =
            MonoServiceLogic::ensure_tree_path_with_chain(&[root], &[], &["a", "b", "c"], leaf)
                .unwrap();

        let a = new_root
            .tree_items
            .iter()
            .find(|i| i.name == "a" && i.mode == TreeItemMode::Tree)
            .expect("root must contain a/");
        let tree_a = produced
            .iter()
            .find(|t| t.id == a.id)
            .expect("produced must include tree a");
        let b = tree_a
            .tree_items
            .iter()
            .find(|i| i.name == "b" && i.mode == TreeItemMode::Tree)
            .expect("a must contain b/");
        let tree_b = produced
            .iter()
            .find(|t| t.id == b.id)
            .expect("produced must include tree b");
        assert!(
            tree_b
                .tree_items
                .iter()
                .any(|i| i.name == "c" && i.mode == TreeItemMode::Tree)
        );
    }

    async fn wired_handler(temp: &tempfile::TempDir) -> MonoApiService {
        let mut storage = test_storage(temp.path()).await;
        let git_service = GitService {
            obj_storage: mock_object_storage(),
        };
        storage.git_service = git_service.clone();
        storage.mono_service = MonoService {
            mono_storage: storage.mono_storage(),
            git_service,
        };
        storage
            .mono_service
            .init_monorepo(&storage.config().monorepo)
            .await
            .unwrap();
        MonoApiService {
            storage: storage.clone(),
            git_object_cache: Arc::new(GitObjectCache {
                connection: init_connection(&RedisConfig {
                    url: std::env::var("MEGA_REDIS__URL")
                        .unwrap_or_else(|_| "redis://127.0.0.1:16379".to_string()),
                })
                .await
                .expect("redis connection"),
                prefix: "disabled".to_string(),
            }),
        }
    }

    #[test]
    fn tree_from_items_checked_applies_git_sort_order() {
        let blob = generate_git_keep_with_timestamp();
        // Directory "foo" must sort after file "foo.txt" under Git tree rules
        // (dir key ends with '/', file with NUL).
        let tree = MonoServiceLogic::tree_from_items_checked(vec![
            TreeItem {
                mode: TreeItemMode::Tree,
                id: blob.id,
                name: "foo".into(),
            },
            TreeItem {
                mode: TreeItemMode::Blob,
                id: blob.id,
                name: "foo.txt".into(),
            },
        ])
        .unwrap();
        assert_eq!(
            tree.tree_items
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            vec!["foo.txt", "foo"]
        );
    }

    #[tokio::test]
    async fn search_tree_for_update_or_create_single_and_multi_level() {
        let temp = tempfile::tempdir().unwrap();
        let handler = wired_handler(&temp).await;

        let (chain_single, blob_single) =
            search_tree_for_update_or_create(&handler, Path::new("x"))
                .await
                .expect("single-level create");
        assert_eq!(chain_single.len(), 2);
        assert!(blob_single.is_some());
        assert!(
            chain_single[0]
                .tree_items
                .iter()
                .any(|i| i.name == "x" && i.mode == TreeItemMode::Tree)
        );
        assert!(
            chain_single[1]
                .tree_items
                .iter()
                .any(|i| i.name == ".gitkeep" && i.id == blob_single.as_ref().unwrap().id)
        );

        let (chain_multi, blob_multi) =
            search_tree_for_update_or_create(&handler, Path::new("a/b/c"))
                .await
                .expect("multi-level create");
        assert_eq!(chain_multi.len(), 4); // root + a + b + leaf
        assert!(blob_multi.is_some());
        assert!(
            chain_multi[0]
                .tree_items
                .iter()
                .any(|i| i.name == "a" && i.mode == TreeItemMode::Tree)
        );
        assert!(
            chain_multi
                .last()
                .unwrap()
                .tree_items
                .iter()
                .any(|i| i.name == ".gitkeep" && i.id == blob_multi.as_ref().unwrap().id)
        );
    }

    #[tokio::test]
    async fn search_tree_for_update_or_create_under_existing_parent() {
        use crate::ceres::api_service::mono_api_service::MonoServiceLogic as Logic;

        let temp = tempfile::tempdir().unwrap();
        let handler = wired_handler(&temp).await;

        // Monorepo init creates `project/` under root — create under that parent.
        let (chain, blob) = search_tree_for_update_or_create(&handler, Path::new("project/newdir"))
            .await
            .expect("create under existing parent");
        assert!(blob.is_some());
        // [root, rewritten project, leaf]
        assert_eq!(chain.len(), 3);
        assert!(
            chain[1]
                .tree_items
                .iter()
                .any(|i| i.name == "newdir" && i.mode == TreeItemMode::Tree)
        );
        assert!(
            chain[2]
                .tree_items
                .iter()
                .any(|i| i.name == ".gitkeep" && i.id == blob.as_ref().unwrap().id)
        );

        // Compatible with build_result_by_chain: pop leaf, roll up through parents.
        let mut roll = chain.clone();
        let leaf = roll.pop().unwrap();
        let result = Logic::build_result_by_chain(
            std::path::PathBuf::from("/project/newdir"),
            roll,
            leaf.id,
        )
        .expect("build_result_by_chain must accept create chain");
        assert!(!result.updated_trees.is_empty());
    }
}
