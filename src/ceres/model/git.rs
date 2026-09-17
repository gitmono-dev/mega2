use std::collections::HashMap;

use git_internal::internal::object::{
    commit::Commit,
    tree::{TreeItem, TreeItemMode},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::contract::api::git::commit::LatestCommitInfo;

#[derive(PartialEq, Eq, Debug, Clone, Deserialize, ToSchema)]
pub struct CreateEntryInfo {
    /// can be a file or directory
    pub is_directory: bool,
    pub name: String,
    /// leave empty if it's under root
    pub path: String,
    // pub import_dir: bool,
    pub content: Option<String>,
    /// web user email for commit binding
    pub author_email: Option<String>,
    /// web username for commit binding (optional)
    pub author_username: Option<String>,
    /// if true, skip build
    #[serde(default)]
    pub skip_build: bool,
    /// Controls how CL is created or reused for this change.
    #[serde(default = "default_create_mode")]
    pub mode: EditCLMode,
}

impl CreateEntryInfo {
    pub fn commit_msg(&self) -> String {
        if self.is_directory {
            format!("create new directory {}", self.name)
        } else {
            format!("create new file {}", self.name)
        }
    }
}

fn default_create_mode() -> EditCLMode {
    EditCLMode::TryReuse(None)
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct CodePreviewQuery {
    #[serde(default)]
    pub refs: String,
    #[serde(default = "default_path")]
    pub path: String,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct TreeQuery {
    pub oid: Option<String>,
    #[serde(default = "default_path")]
    pub path: String,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct BlobContentQuery {
    #[serde(default)]
    pub refs: String,
    #[serde(default = "default_path")]
    pub path: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct CommitBindingInfo {
    pub matched_username: Option<String>,
    pub is_anonymous: bool,
}

pub struct LatestCommitInfoWrapper(pub LatestCommitInfo);

impl From<Commit> for LatestCommitInfoWrapper {
    fn from(commit: Commit) -> Self {
        let message = commit.format_message();
        Self(LatestCommitInfo {
            oid: commit.id.to_string(),
            date: commit.committer.timestamp.to_string(),
            short_message: message,
            author: commit.author.name,
            committer: commit.committer.name,
            status: "success".to_string(),
        })
    }
}

// UserInfo removed: author/committer are now plain strings

#[derive(Serialize, Deserialize, ToSchema)]
pub struct TreeCommitItem {
    pub commit_id: String,
    pub name: String,
    pub content_type: String,
    pub commit_message: String,
    pub date: String,
}

impl From<(TreeItem, Option<Commit>)> for TreeCommitItem {
    fn from((item, commit): (TreeItem, Option<Commit>)) -> Self {
        TreeCommitItem {
            name: item.name.clone(),
            content_type: if item.mode == TreeItemMode::Tree {
                "directory".to_owned()
            } else {
                "file".to_owned()
            },
            commit_id: commit
                .as_ref()
                .map(|x| x.id.to_string())
                .unwrap_or_default(),
            commit_message: commit
                .as_ref()
                .map(|x| x.format_message())
                .unwrap_or_default(),
            date: commit
                .as_ref()
                .map(|x| x.committer.timestamp.to_string())
                .unwrap_or_default(),
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct TreeHashItem {
    pub name: String,
    pub content_type: String,
    pub oid: String,
}

impl From<TreeItem> for TreeHashItem {
    fn from(value: TreeItem) -> Self {
        Self {
            oid: value.id.to_string(),
            name: value.name,
            content_type: if value.mode == TreeItemMode::Tree {
                "directory".to_owned()
            } else {
                "file".to_owned()
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TreeBriefItem {
    pub name: String,
    pub path: String,
    pub content_type: String,
}

impl From<TreeItem> for TreeBriefItem {
    fn from(value: TreeItem) -> Self {
        TreeBriefItem {
            name: value.name,
            path: String::new(),
            content_type: if value.mode == TreeItemMode::Tree {
                "directory".to_owned()
            } else {
                "file".to_owned()
            },
        }
    }
}

fn default_path() -> String {
    "/".to_string()
}

/// Request body for previewing diff of a single file before saving.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DiffPreviewPayload {
    /// Full file path like "/project/dir/file.rs"
    pub path: String,
    /// New content to preview against current HEAD
    pub content: String,
    /// Optional refs (commit SHA or tag); empty/default means current HEAD
    #[serde(default)]
    pub refs: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditCLMode {
    /// force create new cl
    ForceCreate,
    /// try to reuse old cl, if none, will search existing open cl, and create new cl if not found
    TryReuse(Option<String>),
}
/// Request body for saving an edited file with conflict detection.
#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
pub struct EditFilePayload {
    /// Full file path like "/project/dir/file.rs"
    pub path: String,
    /// New file content to save
    pub content: String,
    /// Commit message to use when creating the commit
    pub commit_message: String,
    /// author email to bind this commit to a user
    #[serde(default)]
    pub author_email: Option<String>,
    /// platform username (used to verify and bind commit to user)
    #[serde(default)]
    pub author_username: Option<String>,
    /// if true, skip build
    #[serde(default)]
    pub skip_build: bool,
    #[serde(default = "default_edit_mode")]
    pub mode: EditCLMode,
}

fn default_edit_mode() -> EditCLMode {
    EditCLMode::TryReuse(None)
}

/// Response body after saving an edited file
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct EditFileResult {
    /// New commit id created by this save
    pub commit_id: String,
    /// New blob oid of the saved file
    pub new_oid: String,
    /// Saved file path
    pub path: String,
    pub cl_link: Option<String>,
}

/// Response body after creating a file or directory
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateEntryResult {
    /// New commit id created by this operation
    pub commit_id: String,
    /// New blob oid for the created entry
    pub new_oid: String,
    /// Created entry path
    pub path: String,
    pub cl_link: Option<String>,
}

/// Request body for `POST /delete-entry` (plan-20260917 ADR-LB-03): the
/// parent `path` plus the directory `name`, same shape as create-entry minus
/// `is_directory` / `content` / `mode`.
#[derive(PartialEq, Eq, Debug, Clone, Deserialize, ToSchema)]
pub struct DeleteEntryInfo {
    /// parent directory, rooted; `/` (or empty) means the root
    pub path: String,
    /// name of the directory to delete
    pub name: String,
    /// web username for commit binding / CL ownership (optional)
    pub author_username: Option<String>,
    /// if true, skip build
    #[serde(default)]
    pub skip_build: bool,
}

impl DeleteEntryInfo {
    pub fn commit_msg(&self) -> String {
        format!("delete directory {}", self.name)
    }
}

/// Response body after deleting a directory
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DeleteEntryResult {
    /// New commit id created by this operation
    pub commit_id: String,
    /// Deleted entry path (receipt only)
    pub path: String,
    pub cl_link: Option<String>,
}

/// Validate the parent `path` / entry `name` pair a directory change names
/// (ADR-LB-03): the parent is rooted (`/`-prefixed; empty means the root)
/// with no empty, `.` or `..` component and no control characters; the name
/// is one non-empty component without separators, `.`, `..`, NUL or control
/// characters. The error is a plain diagnostic; callers add the `[code:400]`
/// prefix.
pub fn validate_entry_target(path: &str, name: &str) -> Result<(), String> {
    if !path.is_empty() && !path.starts_with('/') {
        return Err(format!("path must be rooted: {path:?}"));
    }
    if path.chars().any(char::is_control) {
        return Err("path must not contain NUL or control characters".to_string());
    }
    // `/` and `` both mean the root; exactly one trailing `/` is tolerated
    // (like create-entry's `build_entry_path`); a second one is an empty
    // component and is rejected below.
    let body = path.strip_suffix('/').unwrap_or(path);
    if !body.is_empty() {
        for component in body[1..].split('/') {
            if component.is_empty() || component == "." || component == ".." {
                return Err(format!("path contains an invalid component: {path:?}"));
            }
        }
    }
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("name must be a directory name, got {name:?}"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(format!("name must not contain a path separator: {name:?}"));
    }
    if name.chars().any(char::is_control) {
        return Err("name must not contain NUL or control characters".to_string());
    }
    Ok(())
}

/// LB-02 AC-2: the wire shape of `DeleteEntryInfo` (required `path`/`name`,
/// optional `author_username`, `skip_build` defaulting to `false`) and the
/// rooted-parent / single-component-name rules ADR-LB-03 specifies for
/// directory-change writes (create-entry itself does not enforce them;
/// ADR-LB-01 forbids changing it).
#[cfg(test)]
#[test]
fn delete_entry_body_matches_create_entry_path_name_rules() {
    let info: DeleteEntryInfo =
        serde_json::from_str(r#"{"path":"/project","name":"old-dir"}"#).expect("minimal body");
    assert_eq!(
        info,
        DeleteEntryInfo {
            path: "/project".to_string(),
            name: "old-dir".to_string(),
            author_username: None,
            skip_build: false,
        }
    );
    let info: DeleteEntryInfo = serde_json::from_str(
        r#"{"path":"/project","name":"old-dir","author_username":null,"skip_build":true}"#,
    )
    .expect("full body");
    assert!(info.skip_build && info.author_username.is_none());
    assert_eq!(info.commit_msg(), "delete directory old-dir");
    assert!(
        serde_json::from_str::<DeleteEntryInfo>(r#"{"name":"x"}"#).is_err(),
        "path is required"
    );
    assert!(
        serde_json::from_str::<DeleteEntryInfo>(r#"{"path":"/project"}"#).is_err(),
        "name is required"
    );

    for (path, name) in [
        ("/project", "old-dir"),
        ("", "dir"),
        ("/", "dir"),
        ("/a/b", "c.d"),
        ("/project/", "x"),
    ] {
        assert!(
            validate_entry_target(path, name).is_ok(),
            "{path:?} {name:?}"
        );
    }
    for (path, name) in [
        ("project", "x"),
        ("/project/../etc", "x"),
        ("/project//x", "x"),
        ("/./x", "x"),
        ("/project", ""),
        ("/project", "."),
        ("/project", ".."),
        ("/project", "a/b"),
        ("/project", "a\\b"),
        ("/project", "a\0b"),
        ("/project", "a\nb"),
        ("/pro\0ject", "x"),
        ("/project//", "x"),
        ("/project///", "x"),
        ("//", "x"),
    ] {
        assert!(
            validate_entry_target(path, name).is_err(),
            "{path:?} {name:?}"
        );
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TreeResponse {
    pub file_tree: HashMap<String, FileTreeItem>,
    pub tree_items: Vec<TreeBriefItem>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct FileTreeItem {
    pub tree_items: Vec<TreeBriefItem>,
    pub total_count: usize,
}
