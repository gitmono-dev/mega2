use std::{path::PathBuf, str::FromStr, sync::Arc};

use git_internal::{
    hash::ObjectHash,
    internal::object::{
        signature::{Signature, SignatureType},
        tree::{Tree, TreeItem, TreeItemMode},
    },
};

use super::*;
use crate::ceres::model::change_list::ClDiffFile;

#[test]
fn test_clean_path_str_edges() {
    assert_eq!(MonoServiceLogic::clean_path_str(""), "/");
    assert_eq!(MonoServiceLogic::clean_path_str("/"), "/");
    assert_eq!(MonoServiceLogic::clean_path_str("abc/"), "abc");
    assert_eq!(MonoServiceLogic::clean_path_str("abc///"), "abc");
}

#[test]
fn test_normalize_repo_path() {
    use crate::common::errors::{BuckError, MegaError};

    // Normalization: add leading slash, strip trailing
    assert_eq!(
        MonoServiceLogic::normalize_repo_path("project").unwrap(),
        "/project"
    );
    assert_eq!(
        MonoServiceLogic::normalize_repo_path("/project").unwrap(),
        "/project"
    );
    assert_eq!(
        MonoServiceLogic::normalize_repo_path("project/").unwrap(),
        "/project"
    );
    assert_eq!(
        MonoServiceLogic::normalize_repo_path("/project/").unwrap(),
        "/project"
    );
    assert_eq!(
        MonoServiceLogic::normalize_repo_path("  /project  ").unwrap(),
        "/project"
    );
    assert_eq!(MonoServiceLogic::normalize_repo_path("/").unwrap(), "/");

    // Empty / whitespace-only -> ValidationError
    assert!(MonoServiceLogic::normalize_repo_path("").is_err());
    assert!(MonoServiceLogic::normalize_repo_path("   ").is_err());
    assert!(matches!(
        MonoServiceLogic::normalize_repo_path(""),
        Err(MegaError::Buck(BuckError::ValidationError(_)))
    ));

    // Path traversal and invalid chars -> ValidationError
    assert!(MonoServiceLogic::normalize_repo_path("project/../foo").is_err());
    assert!(MonoServiceLogic::normalize_repo_path("project\\foo").is_err());

    // Middle slashes and "." segments are collapsed
    assert_eq!(
        MonoServiceLogic::normalize_repo_path("//project//foo//").unwrap(),
        "/project/foo"
    );
    assert_eq!(
        MonoServiceLogic::normalize_repo_path("project/./foo").unwrap(),
        "/project/foo"
    );
    assert_eq!(
        MonoServiceLogic::normalize_repo_path("/project/./foo").unwrap(),
        "/project/foo"
    );

    // Dot-only paths are rejected (do not silently resolve to root)
    assert!(matches!(
        MonoServiceLogic::normalize_repo_path("."),
        Err(MegaError::Buck(BuckError::ValidationError(_)))
    ));
    assert!(matches!(
        MonoServiceLogic::normalize_repo_path("./"),
        Err(MegaError::Buck(BuckError::ValidationError(_)))
    ));
    assert!(matches!(
        MonoServiceLogic::normalize_repo_path("./."),
        Err(MegaError::Buck(BuckError::ValidationError(_)))
    ));

    // Leading colon is rejected
    assert!(matches!(
        MonoServiceLogic::normalize_repo_path(":/test"),
        Err(MegaError::Buck(BuckError::ValidationError(_)))
    ));
    assert!(matches!(
        MonoServiceLogic::normalize_repo_path(":"),
        Err(MegaError::Buck(BuckError::ValidationError(_)))
    ));

    // Windows drive letters are rejected
    assert!(matches!(
        MonoServiceLogic::normalize_repo_path("C:"),
        Err(MegaError::Buck(BuckError::ValidationError(_)))
    ));
    assert!(matches!(
        MonoServiceLogic::normalize_repo_path("D:/project"),
        Err(MegaError::Buck(BuckError::ValidationError(_)))
    ));
}

#[test]
fn test_repo_root_candidates_walk_from_leaf_to_root() {
    assert_eq!(
        MonoServiceLogic::repo_root_candidates(Path::new("/project/buck2_test/src")),
        vec![
            "/project/buck2_test/src".to_string(),
            "/project/buck2_test".to_string(),
            "/project".to_string(),
            "/".to_string(),
        ]
    );
}

#[test]
fn test_repo_root_candidates_normalize_relative_paths() {
    assert_eq!(
        MonoServiceLogic::repo_root_candidates(Path::new("project/buck2_test/src")),
        vec![
            "/project/buck2_test/src".to_string(),
            "/project/buck2_test".to_string(),
            "/project".to_string(),
            "/".to_string(),
        ]
    );
}

#[test]
fn test_subtree_ref_path_keeps_parent_directory_for_file_edits() {
    assert_eq!(
        MonoServiceLogic::subtree_ref_path(Path::new("/project/buck2_test/src")).unwrap(),
        "/project/buck2_test/src".to_string()
    );
}

#[test]
fn test_subtree_ref_path_normalizes_relative_create_paths() {
    assert_eq!(
        MonoServiceLogic::subtree_ref_path(Path::new("project/buck2_test/src")).unwrap(),
        "/project/buck2_test/src".to_string()
    );
}

#[test]
fn test_save_file_edit_uses_build_repo_root_tree_from_ref_updates() {
    let build_root_tree = ObjectHash::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    let nested_tree = ObjectHash::from_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
    let result = TreeUpdateResult {
        updated_trees: vec![],
        ref_updates: vec![
            RefUpdate {
                path: "/project/buck2_test/src".to_string(),
                tree_id: nested_tree,
            },
            RefUpdate {
                path: "/project/buck2_test".to_string(),
                tree_id: build_root_tree,
            },
        ],
    };

    let selected = MonoApiService::ref_update_tree_id_for_path(&result, "/project/buck2_test");
    assert_eq!(selected, Some(build_root_tree));
}

#[test]
fn test_create_monorepo_entry_uses_normalized_build_repo_root_tree() {
    let build_root_tree = ObjectHash::from_str("cccccccccccccccccccccccccccccccccccccccc").unwrap();
    let result = TreeUpdateResult {
        updated_trees: vec![],
        ref_updates: vec![RefUpdate {
            path: "/project/buck2_test".to_string(),
            tree_id: build_root_tree,
        }],
    };

    let selected = MonoApiService::ref_update_tree_id_for_path(&result, "/project/buck2_test/");
    assert_eq!(selected, Some(build_root_tree));
}

#[test]
fn test_update_tree_hash() {
    let item = TreeItem::new(
        TreeItemMode::Blob,
        ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        "path".to_string(),
    );

    let tree = Tree::from_tree_items(vec![item]).expect("tree should build");
    let tree = Arc::new(tree);

    let new_hash = ObjectHash::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();

    let new_tree = MonoServiceLogic::update_tree_hash(tree, "path", new_hash)
        .expect("update_tree_hash should succeed");

    assert_eq!(new_tree.tree_items.len(), 1);
    assert_eq!(new_tree.tree_items[0].id, new_hash);
}

#[test]
fn test_build_result_by_chain_logic() {
    let item = TreeItem::new(
        TreeItemMode::Blob,
        ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        "path".to_string(),
    );

    let tree = Tree::from_tree_items(vec![item]).expect("tree should build");
    let tree_id = tree.id;

    let update_chain = vec![Arc::new(tree)];
    let path = PathBuf::from("/test/path");

    let result = MonoServiceLogic::build_result_by_chain(path, update_chain, tree_id)
        .expect("build_result_by_chain should succeed");

    assert_eq!(result.updated_trees.len(), 1);
    assert_eq!(result.ref_updates.len(), 2);

    let paths: Vec<&str> = result.ref_updates.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"/test/path"));
    assert!(paths.contains(&"/test"));
}

#[test]
fn test_build_result_by_chain_normalizes_relative_paths_for_ref_updates() {
    let old_hash = ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap();
    let updated_child_hash =
        ObjectHash::from_str("2222222222222222222222222222222222222222").unwrap();
    let item = TreeItem::new(TreeItemMode::Tree, old_hash, "src".to_string());
    let tree = Tree::from_tree_items(vec![item]).expect("tree should build");
    let update_chain = vec![Arc::new(tree)];

    let result = MonoServiceLogic::build_result_by_chain(
        PathBuf::from("project/buck2_test/src"),
        update_chain,
        updated_child_hash,
    )
    .expect("build_result_by_chain should succeed");

    let paths: Vec<&str> = result.ref_updates.iter().map(|r| r.path.as_str()).collect();
    assert!(paths.contains(&"/project/buck2_test/src"));
    assert!(paths.contains(&"/project/buck2_test"));

    let selected = MonoApiService::ref_update_tree_id_for_path(&result, "/project/buck2_test");
    assert!(selected.is_some());
}

#[tokio::test]
async fn test_process_ref_updates_logic() {
    let ref_update = RefUpdate {
        path: "/test".to_string(),
        tree_id: ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
    };

    let tree_update_result = TreeUpdateResult {
        updated_trees: vec![],
        ref_updates: vec![RefUpdate {
            path: ref_update.path.clone(),
            tree_id: ref_update.tree_id,
        }],
    };

    let refs = vec![mega_refs::Model {
        id: 1,
        path: "/test".to_string(),
        ref_name: "refs/heads/main".to_string(),
        ref_commit_hash: "0987654321098765432109876543210987654321".to_string(),
        ref_tree_hash: "1111111111111111111111111111111111111111".to_string(),
        created_at: chrono::Utc::now().naive_utc(),
        updated_at: chrono::Utc::now().naive_utc(),
        is_cl: false,
    }];

    let mut commits: Vec<Commit> = Vec::new();
    let mut updates: Vec<RefUpdateData> = Vec::new();
    let mut new_commit_id = String::new();

    let result = MonoServiceLogic::process_ref_updates(
        &tree_update_result,
        &refs,
        "test commit message",
        &mut commits,
        &mut updates,
        &mut new_commit_id,
    );

    assert!(result.is_ok());
    assert_eq!(commits.len(), 1);
    assert_eq!(updates.len(), 1);
    assert!(!new_commit_id.is_empty());

    let created_commit = &commits[0];

    assert_eq!(
        created_commit.tree_id,
        tree_update_result.ref_updates[0].tree_id
    );
    let expected_parent = ObjectHash::from_str(&refs[0].ref_commit_hash).unwrap();
    assert_eq!(created_commit.parent_commit_ids, vec![expected_parent]);

    assert_eq!(updates[0].ref_name, refs[0].ref_name);
    assert_eq!(updates[0].commit_id, new_commit_id);
}

#[tokio::test]
async fn test_root_merge_flow_updates_cl_ref_and_main() {
    let merged_tree_id = ObjectHash::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
    let root_result =
        MonoServiceLogic::build_result_by_chain(PathBuf::from("/"), vec![], merged_tree_id)
            .expect("root path should build a valid update result");

    assert_eq!(root_result.ref_updates.len(), 1);
    assert_eq!(root_result.ref_updates[0].path, "/");

    let refs = vec![
        mega_refs::Model {
            id: 1,
            path: "/".to_string(),
            ref_name: "refs/cl/abcd1234".to_string(),
            ref_commit_hash: "1111111111111111111111111111111111111111".to_string(),
            ref_tree_hash: "2222222222222222222222222222222222222222".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            is_cl: true,
        },
        mega_refs::Model {
            id: 2,
            path: "/".to_string(),
            ref_name: MEGA_BRANCH_NAME.to_string(),
            ref_commit_hash: "3333333333333333333333333333333333333333".to_string(),
            ref_tree_hash: "4444444444444444444444444444444444444444".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            is_cl: false,
        },
    ];

    let mut commits = Vec::new();
    let mut updates = Vec::new();
    let mut new_commit_id = String::new();

    MonoServiceLogic::process_ref_updates(
        &root_result,
        &refs,
        "merge root cl",
        &mut commits,
        &mut updates,
        &mut new_commit_id,
    )
    .expect("root merge flow should produce ref updates");

    assert_eq!(commits.len(), 1);
    assert_eq!(updates.len(), 2);
    assert!(!new_commit_id.is_empty());

    assert_eq!(updates[0].ref_name, "refs/cl/abcd1234");
    assert_eq!(updates[1].ref_name, MEGA_BRANCH_NAME);
    assert!(updates.iter().all(|u| u.path == "/"));
    assert!(
        updates
            .iter()
            .all(|u| u.tree_hash == merged_tree_id.to_string())
    );
}

#[test]
fn test_map_tree_items_to_commits() {
    let id1 = ObjectHash::Sha1([1u8; 20]);
    let id2 = ObjectHash::Sha1([2u8; 20]);
    let commit_hash = ObjectHash::Sha1([3u8; 20]);

    let item1 = TreeItem {
        id: id1,
        name: "file1.txt".into(),
        mode: TreeItemMode::Blob,
    };
    let item2 = TreeItem {
        id: id2,
        name: "file2.txt".into(),
        mode: TreeItemMode::Blob,
    };

    let tree = Tree {
        id: ObjectHash::Sha1([9u8; 20]),
        tree_items: vec![item1.clone(), item2.clone()],
    };

    let mut item_to_commit_id = HashMap::new();
    item_to_commit_id.insert(id1.to_string(), commit_hash.to_string());

    let fake_sig = Signature {
        signature_type: SignatureType::Committer,
        name: "tester".into(),
        email: "tester@example.com".into(),
        timestamp: 0,
        timezone: "+0000".into(),
    };

    let commit_a = Commit {
        id: commit_hash,
        tree_id: ObjectHash::Sha1([8u8; 20]),
        parent_commit_ids: vec![],
        author: fake_sig.clone(),
        committer: fake_sig.clone(),
        message: "test commit".into(),
    };

    let mut commit_map = HashMap::new();
    commit_map.insert(commit_hash.to_string(), commit_a.clone());

    let result = MonoServiceLogic::map_tree_items_to_commits(tree, &item_to_commit_id, &commit_map);

    assert_eq!(result.get(&item1), Some(&Some(commit_a)));
    assert_eq!(result.get(&item2), Some(&None));
}

#[test]
fn test_path_traversal_with_pop() {
    let mut full_path = PathBuf::from("/project/rust/mega");
    for _ in 0..3 {
        let cloned_path = full_path.clone(); // Clone full_path
        let name = cloned_path.file_name().unwrap().to_str().unwrap();
        full_path.pop();
        println!("name: {name}, path: {full_path:?}");
    }
}

#[test]
fn test_paging_calculation_basic() {
    let files: Vec<ClDiffFile> = vec![
        ClDiffFile::New(
            PathBuf::from("file1.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        ),
        ClDiffFile::Modified(
            PathBuf::from("file2.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
        ),
        ClDiffFile::Deleted(
            PathBuf::from("file3.txt"),
            ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap(),
        ),
    ];

    let page_size = 2u32;
    let page_id = 1u32;

    let start = (page_id.saturating_sub(1)) * page_size;
    let end = (start + page_size).min(files.len() as u32);

    assert_eq!(start, 0);
    assert_eq!(end, 2);

    let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
        let start_idx = start as usize;
        let end_idx = end as usize;
        &files[start_idx..end_idx]
    } else {
        &[]
    };

    assert_eq!(page_slice.len(), 2);
}

#[test]
fn test_paging_calculation_second_page() {
    let files: Vec<ClDiffFile> = vec![
        ClDiffFile::New(
            PathBuf::from("file1.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        ),
        ClDiffFile::Modified(
            PathBuf::from("file2.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
        ),
        ClDiffFile::Deleted(
            PathBuf::from("file3.txt"),
            ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap(),
        ),
        ClDiffFile::New(
            PathBuf::from("file4.txt"),
            ObjectHash::from_str("2222222222222222222222222222222222222222").unwrap(),
        ),
    ];

    let page_size = 2u32;
    let page_id = 2u32;

    let start = (page_id.saturating_sub(1)) * page_size;
    let end = (start + page_size).min(files.len() as u32);

    assert_eq!(start, 2);
    assert_eq!(end, 4);

    let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
        let start_idx = start as usize;
        let end_idx = end as usize;
        &files[start_idx..end_idx]
    } else {
        &[]
    };

    assert_eq!(page_slice.len(), 2);
    assert_eq!(page_slice[0].path(), &PathBuf::from("file3.txt"));
    assert_eq!(page_slice[1].path(), &PathBuf::from("file4.txt"));
}

#[test]
fn test_paging_calculation_partial_page() {
    let files: Vec<ClDiffFile> = vec![
        ClDiffFile::New(
            PathBuf::from("file1.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        ),
        ClDiffFile::Modified(
            PathBuf::from("file2.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
        ),
        ClDiffFile::Deleted(
            PathBuf::from("file3.txt"),
            ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap(),
        ),
    ];

    let page_size = 5u32;
    let page_id = 1u32;

    let start = (page_id.saturating_sub(1)) * page_size;
    let end = (start + page_size).min(files.len() as u32);

    assert_eq!(start, 0);
    assert_eq!(end, 3);

    let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
        let start_idx = start as usize;
        let end_idx = end as usize;
        &files[start_idx..end_idx]
    } else {
        &[]
    };

    assert_eq!(page_slice.len(), 3);
}

#[test]
fn test_paging_calculation_out_of_bounds() {
    let files: Vec<ClDiffFile> = vec![ClDiffFile::New(
        PathBuf::from("file1.txt"),
        ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
    )];

    let page_size = 2u32;
    let page_id = 3u32; // Page that doesn't exist

    let start = (page_id.saturating_sub(1)) * page_size;
    let end = (start + page_size).min(files.len() as u32);

    assert_eq!(start, 4);
    assert_eq!(end, 1); // end is clamped to files.len()

    let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
        let start_idx = start as usize;
        let end_idx = end as usize;
        &files[start_idx..end_idx]
    } else {
        &[]
    };

    assert_eq!(page_slice.len(), 0);
}

#[test]
fn test_paging_calculation_edge_case_zero_page_size() {
    let files: Vec<ClDiffFile> = vec![ClDiffFile::New(
        PathBuf::from("file1.txt"),
        ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
    )];

    let page_size = 0u32;
    let page_id = 1u32;

    let start = (page_id.saturating_sub(1)) * page_size;
    let end = (start + page_size).min(files.len() as u32);

    assert_eq!(start, 0);
    assert_eq!(end, 0);

    let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
        let start_idx = start as usize;
        let end_idx = end as usize;
        &files[start_idx..end_idx]
    } else {
        &[]
    };

    assert_eq!(page_slice.len(), 0);
}

#[test]
fn test_paging_calculation_zero_page_id() {
    let files: Vec<ClDiffFile> = vec![
        ClDiffFile::New(
            PathBuf::from("file1.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        ),
        ClDiffFile::Modified(
            PathBuf::from("file2.txt"),
            ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
            ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
        ),
    ];

    let page_size = 2u32;
    let page_id = 0u32; // Should be treated as page 1 due to saturating_sub

    let start = (page_id.saturating_sub(1)) * page_size;
    let end = (start + page_size).min(files.len() as u32);

    assert_eq!(start, 0);
    assert_eq!(end, 2);

    let page_slice: &[ClDiffFile] = if (start as usize) < files.len() {
        let start_idx = start as usize;
        let end_idx = end as usize;
        &files[start_idx..end_idx]
    } else {
        &[]
    };

    assert_eq!(page_slice.len(), 2);
}

#[test]
fn test_paging_algorithm() {
    let total_files = 10usize;
    let current_page = 2u32;
    let page_size = 3u32;

    let total_pages = total_files.div_ceil(page_size as usize);
    let current_page = current_page as usize;
    let page_size = page_size as usize;

    assert_eq!(total_pages, 4);
    assert_eq!(current_page, 2);
    assert_eq!(page_size, 3);
}

#[test]
fn test_collect_page_blobs_new_files() {
    let files = vec![ClDiffFile::New(
        PathBuf::from("new_file.txt"),
        ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
    )];

    let mut old_blobs = Vec::new();
    let mut new_blobs = Vec::new();

    collect_page_blobs(&files, &mut old_blobs, &mut new_blobs);

    assert_eq!(old_blobs.len(), 0);
    assert_eq!(new_blobs.len(), 1);
    assert_eq!(new_blobs[0].0, PathBuf::from("new_file.txt"));
}

#[test]
fn test_collect_page_blobs_deleted_files() {
    let files = vec![ClDiffFile::Deleted(
        PathBuf::from("deleted_file.txt"),
        ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
    )];

    let mut old_blobs = Vec::new();
    let mut new_blobs = Vec::new();

    collect_page_blobs(&files, &mut old_blobs, &mut new_blobs);

    assert_eq!(old_blobs.len(), 1);
    assert_eq!(new_blobs.len(), 0);
    assert_eq!(old_blobs[0].0, PathBuf::from("deleted_file.txt"));
}

#[test]
fn test_file_lists_with_roots() {
    let all_files = vec![
        "src/main.rs".to_string(),
        "src/utils/math.rs".to_string(),
        "src/utils/io.rs".to_string(),
        "README.md".to_string(),
    ];

    let root: Option<&str> = None;
    let filtered_none: Vec<String> = all_files
        .iter()
        .filter(|file_path| {
            if let Some(prefix) = root {
                file_path.starts_with(prefix)
            } else {
                true
            }
        })
        .cloned()
        .collect();

    assert_eq!(filtered_none.len(), 4);
    assert_eq!(filtered_none, all_files);

    let filtered_some: Vec<String> = all_files
        .iter()
        .filter(|file_path| {
            if let Some(prefix) = Some("src/utils") {
                file_path.starts_with(prefix)
            } else {
                true
            }
        })
        .cloned()
        .collect();

    assert_eq!(filtered_some.len(), 2);
    assert_eq!(
        filtered_some,
        vec![
            "src/utils/math.rs".to_string(),
            "src/utils/io.rs".to_string()
        ]
    );
}

#[test]
fn test_collect_page_blobs_modified_files() {
    let files = vec![ClDiffFile::Modified(
        PathBuf::from("modified_file.txt"),
        ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap(),
        ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap(),
    )];

    let mut old_blobs = Vec::new();
    let mut new_blobs = Vec::new();

    collect_page_blobs(&files, &mut old_blobs, &mut new_blobs);

    assert_eq!(old_blobs.len(), 1);
    assert_eq!(new_blobs.len(), 1);
    assert_eq!(old_blobs[0].0, PathBuf::from("modified_file.txt"));
    assert_eq!(new_blobs[0].0, PathBuf::from("modified_file.txt"));
}

#[test]
fn test_collect_page_blobs_mixed_files() {
    let files = vec![
        ClDiffFile::New(
            PathBuf::from("new.txt"),
            ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap(),
        ),
        ClDiffFile::Deleted(
            PathBuf::from("deleted.txt"),
            ObjectHash::from_str("2222222222222222222222222222222222222222").unwrap(),
        ),
        ClDiffFile::Modified(
            PathBuf::from("modified.txt"),
            ObjectHash::from_str("3333333333333333333333333333333333333333").unwrap(),
            ObjectHash::from_str("4444444444444444444444444444444444444444").unwrap(),
        ),
    ];

    let mut old_blobs = Vec::new();
    let mut new_blobs = Vec::new();

    collect_page_blobs(&files, &mut old_blobs, &mut new_blobs);

    assert_eq!(old_blobs.len(), 2); // deleted + modified
    assert_eq!(new_blobs.len(), 2); // new + modified

    assert_eq!(old_blobs[0].0, PathBuf::from("deleted.txt"));
    assert_eq!(old_blobs[1].0, PathBuf::from("modified.txt"));
    assert_eq!(new_blobs[0].0, PathBuf::from("new.txt"));
    assert_eq!(new_blobs[1].0, PathBuf::from("modified.txt"));
}

#[test]
fn test_relocate_patch_body_rewrites_paths_and_keeps_hunk() {
    let raw_patch = "\
diff --git a/old/name.txt b/old/name.txt\n\
index 1111111..2222222 100644\n\
--- a/old/name.txt\n\
+++ b/old/name.txt\n\
@@ -1 +1 @@\n\
-old line\n\
+new line\n";

    let relocated = MonoApiService::relocate_patch_body(
        raw_patch,
        Path::new("old/name.txt"),
        Path::new("new/name.txt"),
    );

    assert!(!relocated.contains("diff --git"));
    assert!(relocated.contains("--- a/old/name.txt"));
    assert!(relocated.contains("+++ b/new/name.txt"));
    assert!(relocated.contains("@@ -1 +1 @@"));
    assert!(relocated.contains("-old line"));
    assert!(relocated.contains("+new line"));
    assert!(!relocated.contains("deleted file mode"));
}

#[test]
fn test_relocate_patch_body_preserves_hunk_backslashes() {
    let raw_patch = "\
diff --git a/old/name.txt b/old/name.txt\n\
index 1111111..2222222 100644\n\
--- a/old/name.txt\n\
+++ b/old/name.txt\n\
@@ -1 +1 @@\n\
-let path = \"C:\\\\temp\\\\old\";\n\
+let path = \"C:\\\\temp\\\\new\";\n";

    let relocated = MonoApiService::relocate_patch_body(
        raw_patch,
        Path::new("old/name.txt"),
        Path::new("new/name.txt"),
    );

    assert!(relocated.contains("--- a/old/name.txt"));
    assert!(relocated.contains("+++ b/new/name.txt"));
    assert!(relocated.contains("-let path = \"C:\\\\temp\\\\old\";"));
    assert!(relocated.contains("+let path = \"C:\\\\temp\\\\new\";"));
}

#[test]
fn test_normalize_diff_item_path_uses_forward_slashes() {
    let item = DiffItem {
        path: "dir\\nested\\file.txt".to_string(),
        data: "diff --git a/dir\\nested\\file.txt b/dir\\nested\\file.txt\n".to_string(),
    };

    let normalized = MonoApiService::normalize_diff_item(item);
    assert_eq!(normalized.path, "dir/nested/file.txt");
    assert!(
        normalized
            .data
            .contains("diff --git a/dir/nested/file.txt b/dir/nested/file.txt")
    );
}

#[test]
fn test_normalize_patch_header_paths_preserves_hunk_content() {
    let raw_patch = "\
diff --git a/dir\\nested\\file.txt b/dir\\nested\\file.txt\n\
--- a/dir\\nested\\file.txt\n\
+++ b/dir\\nested\\file.txt\n\
@@ -1 +1 @@\n\
-let path = \"C:\\\\temp\\\\old\";\n\
+let path = \"C:\\\\temp\\\\new\";\n";

    let normalized = MonoApiService::normalize_patch_header_paths(raw_patch);

    assert!(normalized.contains("diff --git a/dir/nested/file.txt b/dir/nested/file.txt"));
    assert!(normalized.contains("--- a/dir/nested/file.txt"));
    assert!(normalized.contains("+++ b/dir/nested/file.txt"));
    assert!(normalized.contains("-let path = \"C:\\\\temp\\\\old\";"));
    assert!(normalized.contains("+let path = \"C:\\\\temp\\\\new\";"));
}

#[tokio::test]
async fn test_content_diff_functionality() {
    use std::collections::HashMap;

    use git_internal::internal::object::blob::Blob;

    // Test basic diff generation with sample data
    let old_content = "Hello World\nLine 2\nLine 3";
    let new_content = "Hello Universe\nLine 2\nLine 3 modified";

    let old_blob = Blob::from_content(old_content);
    let new_blob = Blob::from_content(new_content);

    let old_blobs = vec![(PathBuf::from("test_file.txt"), old_blob.id)];
    let new_blobs = vec![(PathBuf::from("test_file.txt"), new_blob.id)];

    // Create a blob cache for the test
    let mut blob_cache: HashMap<ObjectHash, Vec<u8>> = HashMap::new();
    blob_cache.insert(old_blob.id, old_content.as_bytes().to_vec());
    blob_cache.insert(new_blob.id, new_content.as_bytes().to_vec());

    // Test the diff engine directly
    let read_content = |_file: &PathBuf, hash: &ObjectHash| -> Vec<u8> {
        blob_cache.get(hash).cloned().unwrap_or_default()
    };

    let diff_output: Vec<DiffItem> = GitDiff::diff(old_blobs, new_blobs, Vec::new(), read_content)
        .into_iter()
        .collect();

    // Verify diff output contains expected content
    assert!(!diff_output.is_empty(), "Diff output should not be empty");
    assert_eq!(diff_output.len(), 1, "Should have diff for one file");

    let diff_item = &diff_output[0];
    assert_eq!(diff_item.path, "test_file.txt");
    assert!(
        diff_item.data.contains("diff --git"),
        "Should contain git diff header"
    );
    assert!(
        diff_item.data.contains("-Hello World"),
        "Should show removed line"
    );
    assert!(
        diff_item.data.contains("+Hello Universe"),
        "Should show added line"
    );
    assert!(diff_item.data.contains("-Line 3"), "Should show old line 3");
    assert!(
        diff_item.data.contains("+Line 3 modified"),
        "Should show new line 3"
    );
}

#[tokio::test]
async fn test_get_diff_by_blobs_with_empty_content() {
    // Test diff generation with empty content (simulating missing blobs)
    let old_hash = ObjectHash::from_str("1234567890123456789012345678901234567890").unwrap();
    let new_hash = ObjectHash::from_str("abcdefabcdefabcdefabcdefabcdefabcdefabcd").unwrap();

    let old_blobs = vec![(PathBuf::from("empty_file.txt"), old_hash)];
    let new_blobs = vec![(PathBuf::from("empty_file.txt"), new_hash)];

    // Create empty blob cache to simulate missing blobs
    let blob_cache: HashMap<ObjectHash, Vec<u8>> = HashMap::new();

    let read_content = |_file: &PathBuf, hash: &ObjectHash| -> Vec<u8> {
        blob_cache.get(hash).cloned().unwrap_or_default()
    };

    // Test the diff engine with empty content
    let diff_output: Vec<DiffItem> = GitDiff::diff(old_blobs, new_blobs, Vec::new(), read_content)
        .into_iter()
        .collect();

    assert!(
        !diff_output.is_empty(),
        "Should generate diff even with empty blobs"
    );
    assert_eq!(diff_output[0].path, "empty_file.txt");
    assert!(
        diff_output[0].data.contains("diff --git"),
        "Should contain git diff header"
    );
}
