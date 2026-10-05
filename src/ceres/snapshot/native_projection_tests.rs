//! Real persisted commits and object storage, with an independent raw-content
//! oracle. This exercises on-demand memoization, not durable publication.

use std::{collections::BTreeMap, sync::Arc};

use git_internal::{
    hash::{HashKind, ObjectHash},
    internal::object::{
        blob::Blob,
        commit::Commit,
        tree::{Tree, TreeItem, TreeItemMode},
        types::ObjectType,
    },
};
use sha2::{Digest, Sha256};

use super::{FsKind, ProjectionWork, build_directory_page, build_directory_page_with_work};
use crate::{
    ceres::api_service::{ApiHandler, cache::GitObjectCache, mono_api_service::MonoApiService},
    config::testing::isolated_config,
    jupiter::{
        service::git_service::GitService,
        storage::object_storage::build_object_storage,
        tests::{test_redis_manager, test_storage_with_config},
    },
};

type Files = BTreeMap<String, Vec<u8>>;

#[tokio::test]
async fn mst2_native_retention_prepares_full_shared_radix_closure_once() {
    use crate::ceres::snapshot::retention_dag::MetadataDagLimits;

    let temp = tempfile::tempdir().unwrap();
    let handler = handler(temp.path()).await;
    let mut files = Files::new();
    for prefix in ["/project/alpha", "/project/beta"] {
        for index in 0..129 {
            files.insert(
                format!("{prefix}/f{index:03}"),
                format!("raw-{index}").into_bytes(),
            );
        }
        files.insert(format!("{prefix}/nested/file"), b"shared nested".to_vec());
    }
    let (_, root) = persist_commit(&handler, &files, vec![], "retention closure").await;
    let scope = build_directory_page(&handler, &root, "/project")
        .await
        .unwrap();
    let prepared = super::prepare_native_metadata_retention(
        &handler,
        &root,
        "/project",
        MetadataDagLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(prepared.fixed_root_tree_oid(), root.id.to_tagged_string());
    assert_eq!(prepared.scope(), "/project");
    assert_eq!(prepared.metadata_codec(), 1);
    assert_eq!(prepared.schema_version(), 2);
    assert_eq!(prepared.dag().root(), scope.page_id);
    assert!(prepared.dag().payloads().iter().any(|payload| {
        matches!(
            mst2_codec::metapage::Page::decode(&payload.bytes)
                .unwrap()
                .0,
            mst2_codec::metapage::Page::Branch { .. }
        )
    }));
    assert!(prepared.dag().payloads().len() > 3);
    let root_node = format!("page:sha256:{}", super::hex(&scope.page_id));
    assert_eq!(
        prepared
            .dag()
            .edges()
            .iter()
            .filter(|edge| edge.parent == root_node)
            .count(),
        1
    );

    // Remove all directory memo entries: the prepared hit must return its Arc
    // without rebuilding or walking even this fixed view's child Git trees.
    {
        let mut state = handler
            .storage
            .native_projection_cache
            .state
            .lock()
            .unwrap();
        state.pages.clear();
        state.retained_payload_bytes = 0;
    }
    let again = super::prepare_native_metadata_retention(
        &handler,
        &root,
        "/project",
        MetadataDagLimits::default(),
    )
    .await
    .unwrap();
    assert!(Arc::ptr_eq(prepared.dag(), again.dag()));
    {
        let state = handler
            .storage
            .native_projection_cache
            .state
            .lock()
            .unwrap();
        assert!(
            state.pages.is_empty(),
            "prepare hit must not rebuild directory projections"
        );
    }
    let tightened = super::prepare_native_metadata_retention(
        &handler,
        &root,
        "/project",
        MetadataDagLimits {
            nodes: 1,
            ..MetadataDagLimits::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        tightened.code,
        crate::ceres::snapshot::error::SnapshotErrorCode::LimitExceeded
    );
}

#[tokio::test]
async fn mst2_native_retention_checks_source_budgets_before_unknown_child_fetch() {
    use crate::ceres::snapshot::{error::SnapshotErrorCode, retention_dag::MetadataDagLimits};

    let temp = tempfile::tempdir().unwrap();
    let handler = handler(temp.path()).await;
    let unknown = ObjectHash::from_hex_for_kind(HashKind::Sha1, &"a".repeat(40)).unwrap();
    let root = Tree::from_tree_items_with_kind(
        HashKind::Sha1,
        vec![TreeItem::new(
            TreeItemMode::Tree,
            unknown,
            "unknown-child".into(),
        )],
    )
    .unwrap();
    for limits in [
        MetadataDagLimits {
            nodes: 1,
            ..MetadataDagLimits::default()
        },
        MetadataDagLimits {
            edges: 0,
            ..MetadataDagLimits::default()
        },
        MetadataDagLimits {
            entries: 0,
            ..MetadataDagLimits::default()
        },
        MetadataDagLimits {
            payload_bytes: 19,
            ..MetadataDagLimits::default()
        },
    ] {
        let error = super::prepare_native_metadata_retention(&handler, &root, "/", limits)
            .await
            .unwrap_err();
        assert_eq!(
            error.code,
            SnapshotErrorCode::LimitExceeded,
            "budget must reject before querying the nonexistent tree: {error}"
        );
    }
    let state = handler
        .storage
        .native_projection_cache
        .state
        .lock()
        .unwrap();
    assert!(state.retention_dags.is_empty());
}

async fn handler(temp: &std::path::Path) -> MonoApiService {
    let config = isolated_config(temp.join("config"));
    let object_storage = build_object_storage(&config.object_storage).await.unwrap();
    let mut storage = test_storage_with_config(temp, config).await;
    storage.git_service = GitService {
        obj_storage: object_storage,
    };
    MonoApiService {
        storage,
        git_object_cache: Arc::new(GitObjectCache {
            connection: test_redis_manager().await,
            prefix: String::new(),
        }),
    }
}

#[derive(Default)]
struct Directory {
    children: BTreeMap<String, Directory>,
    files: BTreeMap<String, ObjectHash>,
}

fn build_trees(directory: Directory, trees: &mut Vec<Tree>) -> Tree {
    let mut items: BTreeMap<String, TreeItem> = directory
        .files
        .into_iter()
        .map(|(name, oid)| {
            let item = TreeItem::new(TreeItemMode::Blob, oid, name.clone());
            (name, item)
        })
        .collect();
    for (name, child) in directory.children {
        let child = build_trees(child, trees);
        items.insert(
            name.clone(),
            TreeItem::new(TreeItemMode::Tree, child.id, name),
        );
    }
    let tree =
        Tree::from_tree_items_with_kind(HashKind::Sha1, items.into_values().collect()).unwrap();
    trees.push(tree.clone());
    tree
}

async fn persist_commit(
    handler: &MonoApiService,
    files: &Files,
    parents: Vec<ObjectHash>,
    message: &str,
) -> (Commit, Tree) {
    let mut directory = Directory::default();
    for (path, raw) in files {
        let blob = Blob::from_content_bytes_with_kind(HashKind::Sha1, raw.clone()).unwrap();
        handler
            .storage
            .git_service
            .save_object_from_model(raw.clone(), &blob.id.to_string())
            .await
            .unwrap();
        let components: Vec<_> = path.trim_start_matches('/').split('/').collect();
        let mut parent = &mut directory;
        for component in &components[..components.len() - 1] {
            parent = parent.children.entry((*component).to_string()).or_default();
        }
        parent
            .files
            .insert(components.last().unwrap().to_string(), blob.id);
    }
    let mut trees = Vec::new();
    let root = build_trees(directory, &mut trees);
    persist_tree_commit(handler, root, trees, parents, message).await
}

async fn persist_tree_commit(
    handler: &MonoApiService,
    root: Tree,
    trees: Vec<Tree>,
    parents: Vec<ObjectHash>,
    message: &str,
) -> (Commit, Tree) {
    let commit = Commit::from_tree_id_with_kind(HashKind::Sha1, root.id, parents, message).unwrap();
    let mono = handler.storage.mono_storage();
    mono.save_mega_trees(trees, commit.id, None).await.unwrap();
    mono.save_mega_commits(vec![commit.clone()], None)
        .await
        .unwrap();
    // Read back the real persisted commit and its fixed tree; no fake blob OID
    // is used as a commit identity and no live ref is required by the builder.
    let persisted = handler
        .get_commit_by_hash(&commit.id.to_string())
        .await
        .unwrap();
    assert_eq!(persisted.tree_id, root.id);
    let root = handler
        .get_tree_by_hash(&persisted.tree_id.to_string())
        .await
        .unwrap();
    (persisted, root)
}

fn fixture(modules: usize, buckets: usize, files_per_bucket: usize, bytes: usize) -> Files {
    let mut files = Files::new();
    for module in 0..modules {
        for bucket in 0..buckets {
            for file in 0..files_per_bucket {
                let path = format!("/project/m{module:03}/d{bucket:02}/f{file:03}");
                let mut raw = vec![b'x'; bytes.max(path.len())];
                raw[..path.len()].copy_from_slice(path.as_bytes());
                files.insert(path, raw);
            }
        }
    }
    files
}

async fn assert_raw_oracle(handler: &MonoApiService, root: &Tree, scope: &str, files: &Files) {
    let mut paths = vec![scope.to_string()];
    let mut projected = BTreeMap::new();
    while let Some(path) = paths.pop() {
        let directory = build_directory_page(handler, root, &path).await.unwrap();
        for entry in &directory.entries {
            let child = format!("{}/{name}", path.trim_end_matches('/'), name = entry.name);
            if entry.fs_kind == FsKind::Directory {
                paths.push(child);
            } else {
                assert_eq!(entry.fs_kind, FsKind::Regular);
                projected.insert(child, (entry.size.unwrap(), entry.content_digest.unwrap()));
            }
        }
    }
    let expected: BTreeMap<_, _> = files
        .iter()
        .map(|(path, raw)| {
            (
                path.clone(),
                (raw.len() as u64, <[u8; 32]>::from(Sha256::digest(raw))),
            )
        })
        .collect();
    assert_eq!(projected, expected);
}

async fn leaf_update_case(
    modules: usize,
    buckets: usize,
    files_per_bucket: usize,
    bytes: usize,
) -> ProjectionWork {
    let temp = tempfile::tempdir().unwrap();
    let handler = handler(temp.path()).await;
    let mut files = fixture(modules, buckets, files_per_bucket, bytes);
    let original = files.clone();
    let (v1, root1) = persist_commit(&handler, &files, vec![], "native projection V1").await;
    let (page1, cold) = build_directory_page_with_work(&handler, &root1, "/project")
        .await
        .unwrap();
    assert_eq!(
        cold.directories_rebuilt,
        (1 + modules + modules * buckets) as u64
    );
    assert_eq!(cold.reused_subtree_roots, 0);
    assert_eq!(cold.directory_root_pages_built, cold.directories_rebuilt);
    assert_eq!(cold.verified_blob_misses, files.len() as u64);

    files.get_mut("/project/m000/d00/f000").unwrap().push(b'2');
    let (v2, root2) = persist_commit(&handler, &files, vec![v1.id], "native projection V2").await;
    let (page2, update) = build_directory_page_with_work(&handler, &root2, "/project")
        .await
        .unwrap();
    assert_ne!(page1.page_id, page2.page_id);
    assert_eq!(update.directories_rebuilt, 3);
    assert_eq!(update.directory_root_pages_built, 3);
    assert_eq!(
        update.tree_fetches, 3,
        "scope + two changed child trees only"
    );
    assert_eq!(
        update.reused_subtree_roots,
        (modules - 1 + buckets - 1) as u64
    );
    assert_eq!(
        update.directory_entries_scanned,
        (modules + buckets + files_per_bucket) as u64
    );
    assert_eq!(update.verified_blob_hits, (files_per_bucket - 1) as u64);
    assert_eq!(update.verified_blob_misses, 1);
    assert_eq!(
        update.raw_bytes_fetched,
        files["/project/m000/d00/f000"].len() as u64
    );
    assert_eq!(update.raw_bytes_hashed, update.raw_bytes_fetched);
    let scope_tree =
        super::fetch_tree_with_work(&handler, &root2, "/project", &mut ProjectionWork::default())
            .await
            .unwrap();
    let mut full_work = ProjectionWork::default();
    let full = super::build_subtree(
        &handler,
        &handler.storage,
        &scope_tree,
        "/project",
        false,
        &mut full_work,
    )
    .await
    .unwrap();
    assert_eq!(
        page2.page_id, full.page_id,
        "cache reuse equals a fresh full projection"
    );
    assert_eq!(full_work.directories_rebuilt, cold.directories_rebuilt);
    assert_eq!(full_work.reused_subtree_roots, 0);
    assert_raw_oracle(&handler, &root2, "/project", &files).await;

    // A third real commit repeats the operation; this cannot be a special
    // first-update shortcut. Reading older fixed roots remains correct.
    let second_files = files.clone();
    files.get_mut("/project/m000/d00/f000").unwrap().push(b'3');
    let (_, root3) = persist_commit(&handler, &files, vec![v2.id], "native projection V3").await;
    let (_, third) = build_directory_page_with_work(&handler, &root3, "/project")
        .await
        .unwrap();
    assert_eq!(third.directories_rebuilt, update.directories_rebuilt);
    assert_eq!(
        third.directory_entries_scanned,
        update.directory_entries_scanned
    );
    assert_eq!(third.reused_subtree_roots, update.reused_subtree_roots);
    assert_eq!(third.verified_blob_misses, 1);
    assert_raw_oracle(&handler, &root3, "/project", &files).await;
    assert_raw_oracle(&handler, &root2, "/project", &second_files).await;

    let (old, old_work) = build_directory_page_with_work(&handler, &root1, "/project")
        .await
        .unwrap();
    assert_eq!(old.page_id, page1.page_id);
    assert_eq!(old_work.directories_rebuilt, 0);
    assert_eq!(old_work.directory_entries_scanned, 0);
    assert_eq!(old_work.reused_subtree_roots, 1);
    assert_raw_oracle(&handler, &root1, "/project", &original).await;
    let old_file = super::resolve_abs(&handler, &root1, "/project/m000/d00/f000")
        .await
        .unwrap();
    assert!(
        matches!(old_file, super::WalkOutcome::FoundFile { raw, .. } if raw == original["/project/m000/d00/f000"])
    );
    update
}

#[tokio::test]
async fn mst2_native_projection_real_commit_leaf_update_only_rebuilds_ancestors() {
    leaf_update_case(4, 3, 4, 64).await;
}

#[tokio::test]
#[ignore = "opt-in 16,384-file/256MiB real object-storage projection workload"]
async fn mst2_native_projection_medium_real_commit_leaf_update() {
    let update = leaf_update_case(64, 8, 32, 16 * 1024).await;
    assert_eq!(update.directory_entries_scanned, 104);
    assert_eq!(update.reused_subtree_roots, 70);
    eprintln!("native projection memoization work: {update:?}");
}

#[tokio::test]
async fn mst2_native_projection_same_oid_isolated_storage_does_not_share_cache() {
    let temp_a = tempfile::tempdir().unwrap();
    let temp_b = tempfile::tempdir().unwrap();
    let a = handler(temp_a.path()).await;
    let b = handler(temp_b.path()).await;
    let files = fixture(2, 2, 2, 64);
    let (_, root) = persist_commit(&a, &files, vec![], "independent storage A").await;
    let (cached, _) = build_directory_page_with_work(&a, &root, "/")
        .await
        .unwrap();
    assert!(
        build_directory_page_with_work(&b, &root, "/")
            .await
            .is_err(),
        "B must fetch its own missing child tree"
    );
    let (_, own_root) = persist_commit(&b, &files, vec![], "independent storage B").await;
    assert_eq!(root.id, own_root.id);
    let (rebuilt, work) = build_directory_page_with_work(&b, &own_root, "/")
        .await
        .unwrap();
    assert_eq!(cached.page_id, rebuilt.page_id);
    assert_eq!(work.directories_rebuilt, 8);
    assert_eq!(work.reused_subtree_roots, 0);
    let clone = a.clone();
    let (_, clone_work) = build_directory_page_with_work(&clone, &root, "/")
        .await
        .unwrap();
    assert_eq!(clone_work.directories_rebuilt, 0);
    assert_eq!(clone_work.tree_fetches, 0);
}

#[tokio::test]
async fn mst2_native_projection_real_commit_directory_move_reuses_identical_tree() {
    let temp = tempfile::tempdir().unwrap();
    let handler = handler(temp.path()).await;
    let files = Files::from([("/project/old/nested/file".into(), b"move me".to_vec())]);
    let (v1, root1) = persist_commit(&handler, &files, vec![], "before move").await;
    let (before, _) = build_directory_page_with_work(&handler, &root1, "/project")
        .await
        .unwrap();
    let moved = Files::from([("/project/new/nested/file".into(), b"move me".to_vec())]);
    let (_, root2) = persist_commit(&handler, &moved, vec![v1.id], "after move").await;
    let (after, work) = build_directory_page_with_work(&handler, &root2, "/project")
        .await
        .unwrap();
    assert_eq!(
        before.entries[0].directory_root,
        after.entries[0].directory_root
    );
    assert_eq!(work.directories_rebuilt, 1);
    assert_eq!(work.reused_subtree_roots, 1);
    assert_eq!(
        work.tree_fetches, 1,
        "only scope; moved root is reused by OID"
    );
    assert_eq!(work.verified_blob_hits + work.verified_blob_misses, 0);
    assert_raw_oracle(&handler, &root2, "/project", &moved).await;
}

fn byte_prefix(bytes: usize) -> String {
    let mut path = "/project".to_string();
    while path.len() < bytes {
        let length = (bytes - path.len() - 1).min(255);
        assert_ne!(length, 0);
        path.push('/');
        path.extend(std::iter::repeat_n('a', length));
    }
    path
}

fn wrap_tree_at(prefix: &str, mut child: Tree, trees: &mut Vec<Tree>) -> Tree {
    for name in prefix[1..].split('/').rev() {
        child = Tree::from_tree_items_with_kind(
            HashKind::Sha1,
            vec![TreeItem::new(
                TreeItemMode::Tree,
                child.id,
                name.to_string(),
            )],
        )
        .unwrap();
        trees.push(child.clone());
    }
    child
}

#[tokio::test]
async fn mst2_native_projection_cached_empty_child_move_rechecks_path_budgets() {
    let temp = tempfile::tempdir().unwrap();
    let handler = handler(temp.path()).await;
    let empty = Tree {
        id: ObjectHash::from_type_and_data_for_kind(HashKind::Sha1, ObjectType::Tree, &[]).unwrap(),
        tree_items: vec![],
    };
    let source = Tree::from_tree_items_with_kind(
        HashKind::Sha1,
        vec![TreeItem::new(
            TreeItemMode::Tree,
            empty.id,
            "empty".to_string(),
        )],
    )
    .unwrap();
    let mut trees = vec![empty.clone(), source.clone()];
    let root = wrap_tree_at("/project/source", source.clone(), &mut trees);
    let (mut commit, root) =
        persist_tree_commit(&handler, root, trees, vec![], "empty source").await;
    build_directory_page_with_work(&handler, &root, "/project/source")
        .await
        .unwrap();
    for (index, (prefix, allowed)) in [
        (byte_prefix(4090), true),
        (byte_prefix(4091), false),
        (format!("/project/{}", vec!["a"; 254].join("/")), true),
        (format!("/project/{}", vec!["a"; 255].join("/")), false),
    ]
    .into_iter()
    .enumerate()
    {
        let mut trees = vec![empty.clone(), source.clone()];
        let root = wrap_tree_at(&prefix, source.clone(), &mut trees);
        let (next, root) = persist_tree_commit(
            &handler,
            root,
            trees,
            vec![commit.id],
            &format!("empty move {index}"),
        )
        .await;
        commit = next;
        let result = build_directory_page_with_work(&handler, &root, &prefix).await;
        if allowed {
            let (directory, work) = result.unwrap();
            assert_eq!(directory.entries[0].name, "empty");
            assert_eq!(work.directories_rebuilt, 0);
            assert_eq!(work.reused_subtree_roots, 1);
        } else {
            assert!(
                matches!(result, Err(error) if error.code == super::SnapshotErrorCode::ScopeInvalid)
            );
        }
    }
}

#[test]
fn mst2_native_projection_cache_separates_equal_hex_different_hash_kinds() {
    let cache = super::NativeProjectionCache::default();
    let sha = ObjectHash::from_bytes_for_kind(HashKind::Sha256, &[7; 32]).unwrap();
    let blake = ObjectHash::from_bytes_for_kind(HashKind::Blake3, &[7; 32]).unwrap();
    assert_eq!(sha.to_string(), blake.to_string());
    let page_bytes = mst2_codec::metapage::Page::build(&[]).unwrap();
    let directory = Arc::new(super::BuiltDirectory {
        page_id: mst2_codec::metapage::page_id(&page_bytes),
        page_bytes,
        entries: vec![],
        codec_entries: vec![],
        path_budget: super::DescendantPathBudget::default(),
    });
    cache.insert(sha, Arc::clone(&directory));
    assert!(cache.get(blake).is_none());
    assert!(Arc::ptr_eq(&cache.get(sha).unwrap(), &directory));
}

#[tokio::test]
async fn mst2_native_projection_cached_move_rechecks_full_scope_path_budgets() {
    let temp = tempfile::tempdir().unwrap();
    let handler = handler(temp.path()).await;
    let raw = b"multibyte basename".to_vec();
    let files = Files::from([("/project/source/é".into(), raw.clone())]);
    let (mut commit, root) = persist_commit(&handler, &files, vec![], "warm source").await;
    build_directory_page_with_work(&handler, &root, "/project/source")
        .await
        .unwrap();
    let prefixes = [
        (byte_prefix(4093), true),
        (byte_prefix(4094), false),
        (format!("/project/{}", vec!["a"; 254].join("/")), true),
        (format!("/project/{}", vec!["a"; 255].join("/")), false),
    ];
    for (index, (prefix, allowed)) in prefixes.into_iter().enumerate() {
        let files = Files::from([(format!("{prefix}/é"), raw.clone())]);
        let (next, root) =
            persist_commit(&handler, &files, vec![commit.id], &format!("move {index}")).await;
        commit = next;
        let result = build_directory_page_with_work(&handler, &root, &prefix).await;
        if allowed {
            let (_, work) = result.unwrap();
            assert_eq!(work.directories_rebuilt, 0);
            assert_eq!(work.reused_subtree_roots, 1);
            assert_eq!(work.directory_entries_scanned, 0);
        } else {
            assert!(
                matches!(result, Err(error) if error.code == super::SnapshotErrorCode::ScopeInvalid)
            );
        }
    }
}
