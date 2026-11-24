use std::path::PathBuf;

use git_internal::{DiffItem, hash::ObjectHash};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    callisto::{check_result, mega_cl_commits, sea_orm_active_enums::MergeStatusEnum},
    ceres::{
        merge_checker::{CheckType, ConditionResult},
        model::conversation::ConversationItem,
    },
    contract::api::common::CommonPage,
    jupiter::model::{cl_dto::CLDetails, common::ListParams},
};

#[derive(Deserialize, ToSchema)]
pub struct ListPayload {
    pub status: String,
    pub author: Option<String>,
    pub sort_by: Option<String>,
    pub asc: bool,
}

impl From<ListPayload> for ListParams {
    fn from(value: ListPayload) -> Self {
        Self {
            status: value.status,
            author: value.author,
            sort_by: value.sort_by,
            asc: value.asc,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct CLDetailRes {
    pub id: i64,
    pub link: String,
    pub title: String,
    pub status: MergeStatus,
    pub open_timestamp: i64,
    pub merge_timestamp: Option<i64>,
    pub conversations: Vec<ConversationItem>,
    pub path: String,
    pub from_hash: String,
    pub to_hash: String,
    pub head_revision: i64,
    pub base_branch: String,
    pub author: String,
}

impl From<CLDetails> for CLDetailRes {
    fn from(value: CLDetails) -> Self {
        Self {
            id: value.cl.id,
            link: value.cl.link,
            title: value.cl.title,
            status: value.cl.status.into(),
            open_timestamp: value.cl.created_at.and_utc().timestamp(),
            merge_timestamp: value.cl.merge_date.map(|dt| dt.and_utc().timestamp()),
            conversations: value
                .conversations
                .into_iter()
                .map(|x| ConversationItem::from_model(x.conversation))
                .collect(),
            path: value.cl.path,
            from_hash: value.cl.from_hash,
            to_hash: value.cl.to_hash,
            head_revision: value.cl.head_revision,
            base_branch: value.cl.base_branch,
            author: value.cl.username,
        }
    }
}

/// One commit in a CL's commit listing — the DEP-01 frozen wire contract
/// (`sha`/`message`/`author_name`/`author_email`, snake_case on the wire,
/// chain order decided by the storage read). Commit metadata only, never blob
/// content. plan-20260827 MC-05.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ClCommitRes {
    pub sha: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}

impl From<mega_cl_commits::Model> for ClCommitRes {
    fn from(value: mega_cl_commits::Model) -> Self {
        Self {
            sha: value.commit_sha,
            message: value.message,
            author_name: value.author_name,
            author_email: value.author_email,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct FilesChangedPage {
    pub page: CommonPage<ClFilesChangedItemSchema>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
pub struct ClFilesChangedItemSchema {
    pub path: String,
    pub data: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema, Clone)]
pub struct DiffItemSchema {
    pub path: String,
    pub data: String,
}

impl From<DiffItem> for DiffItemSchema {
    fn from(item: DiffItem) -> Self {
        Self {
            path: item.path,
            data: item.data,
        }
    }
}

impl ClFilesChangedItemSchema {
    pub fn new(item: DiffItem, old_path: Option<String>) -> Self {
        Self {
            path: item.path,
            data: item.data,
            old_path,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct UpdateBranchStatusRes {
    pub base_commit: String,
    pub target_head: String,
    pub outdated: bool,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct MuiTreeNode {
    id: String,
    pub label: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub children: Option<Vec<MuiTreeNode>>,
}

impl MuiTreeNode {
    pub fn new(label: &str, path: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            label: label.to_string(),
            path: path.to_string(),
            children: None,
        }
    }

    pub fn insert_path(&mut self, parts: &[&str], buf: &mut String) {
        if parts.is_empty() {
            return;
        }

        if self.children.is_none() {
            self.children = Some(Vec::new());
        }

        let children = self.children.as_mut().unwrap();

        if let Some(existing) = children.iter_mut().find(|c| c.label == parts[0]) {
            let mut buf = existing.path.clone();
            existing.insert_path(&parts[1..], &mut buf);
        } else {
            buf.push('/');
            buf.push_str(parts[0]);
            let mut new_node = MuiTreeNode::new(parts[0], buf);
            new_node.insert_path(&parts[1..], buf);
            children.push(new_node);
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct ClFilesRes {
    pub path: String,
    pub sha: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<u8>,
}

impl From<ClDiffFile> for ClFilesRes {
    fn from(value: ClDiffFile) -> Self {
        match value {
            ClDiffFile::New(path, sha) => Self {
                path: path.to_string_lossy().replace('\\', "/"),
                sha: sha.to_string(),
                action: "new".to_owned(),
                old_path: None,
                similarity: None,
            },
            ClDiffFile::Deleted(path, sha) => Self {
                path: path.to_string_lossy().replace('\\', "/"),
                sha: sha.to_string(),
                action: "deleted".to_owned(),
                old_path: None,
                similarity: None,
            },
            ClDiffFile::Modified(path, _, new) => Self {
                path: path.to_string_lossy().replace('\\', "/"),
                sha: new.to_string(),
                action: "modified".to_owned(),
                old_path: None,
                similarity: None,
            },
            ClDiffFile::Renamed(old_path, new_path, _, new_hash, similarity)
            | ClDiffFile::Moved(old_path, new_path, _, new_hash, similarity) => Self {
                path: new_path.to_string_lossy().replace('\\', "/"),
                sha: new_hash.to_string(),
                action: "renamed".to_owned(),
                old_path: Some(old_path.to_string_lossy().replace('\\', "/")),
                similarity: Some(similarity),
            },
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub enum MergeStatus {
    Open,
    Merged,
    Closed,
    Draft,
}

impl From<MergeStatusEnum> for MergeStatus {
    fn from(value: MergeStatusEnum) -> Self {
        match value {
            MergeStatusEnum::Open => MergeStatus::Open,
            MergeStatusEnum::Merged => MergeStatus::Merged,
            MergeStatusEnum::Closed => MergeStatus::Closed,
            MergeStatusEnum::Draft => MergeStatus::Draft,
        }
    }
}

impl From<MergeStatus> for MergeStatusEnum {
    fn from(value: MergeStatus) -> Self {
        match value {
            MergeStatus::Open => MergeStatusEnum::Open,
            MergeStatus::Merged => MergeStatusEnum::Merged,
            MergeStatus::Closed => MergeStatusEnum::Closed,
            MergeStatus::Draft => MergeStatusEnum::Draft,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct MergeBoxRes {
    pub merge_requirements: Option<MergeRequirements>,
}

impl MergeBoxRes {
    pub fn from_condition(conditions: Vec<Condition>) -> Self {
        let mut state = RequirementsState::MERGEABLE;
        for cond in &conditions {
            if cond.result != ConditionResult::PASSED {
                state = RequirementsState::UNMERGEABLE;
            }
        }
        MergeBoxRes {
            merge_requirements: Some(MergeRequirements { conditions, state }),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct MergeRequirements {
    pub conditions: Vec<Condition>,
    pub state: RequirementsState,
}

#[derive(Serialize, ToSchema)]
pub struct Condition {
    #[serde(rename = "type")]
    pub condition_type: CheckType,
    pub display_name: String,
    pub description: String,
    pub message: String,
    pub result: ConditionResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_attempt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_generation: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_status: Option<String>,
}

impl TryFrom<check_result::Model> for Condition {
    type Error = ();

    fn try_from(value: check_result::Model) -> Result<Self, Self::Error> {
        let check_type = CheckType::try_from(value.check_type_code)?;
        let result = ConditionResult::from_status(&value.status);
        Ok(Self {
            condition_type: check_type.clone(),
            display_name: check_type.display_name().to_string(),
            description: check_type.description().to_string(),
            message: value.message,
            result,
            commit_id: (!value.commit_id.is_empty()).then_some(value.commit_id),
            head_revision: (value.head_revision > 0).then_some(value.head_revision),
            ci_task_id: value.ci_task_id,
            ci_attempt_id: value.ci_attempt_id,
            ci_generation: (value.ci_generation > 0).then_some(value.ci_generation),
            details_url: value.details_url,
            raw_status: Some(value.status),
        })
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub enum RequirementsState {
    UNMERGEABLE,
    MERGEABLE,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct UpdateClStatusPayload {
    pub status: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ReviewerPayload {
    pub reviewer_usernames: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ReviewersResponse {
    pub result: Vec<ReviewerInfo>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ChangeReviewerStatePayload {
    pub approved: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ChangeReviewStatePayload {
    pub conversation_id: i64,
    pub resolved: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct SetSystemReviewersPayload {
    pub target_reviewer_usernames: Vec<String>,
    pub is_system_required: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ReviewerInfo {
    pub username: String,
    pub approved: bool,
    pub system_required: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct CloneRepoPayload {
    pub owner: String,
    pub repo: String,
    pub path: String,
}

#[derive(PartialEq, Eq, Debug, Clone, Serialize, Deserialize)]
pub enum ClDiffFile {
    New(PathBuf, ObjectHash),
    Deleted(PathBuf, ObjectHash),
    // path, old_hash, new_hash
    Modified(PathBuf, ObjectHash, ObjectHash),
    // old_path, new_path, old_hash, new_hash, similarity
    Renamed(PathBuf, PathBuf, ObjectHash, ObjectHash, u8),
    // old_path, new_path, old_hash, new_hash, similarity
    Moved(PathBuf, PathBuf, ObjectHash, ObjectHash, u8),
}

impl ClDiffFile {
    pub fn path(&self) -> &PathBuf {
        match self {
            ClDiffFile::New(path, _) => path,
            ClDiffFile::Deleted(path, _) => path,
            ClDiffFile::Modified(path, _, _) => path,
            ClDiffFile::Renamed(_, new_path, _, _, _) => new_path,
            ClDiffFile::Moved(_, new_path, _, _, _) => new_path,
        }
    }

    pub fn kind_weight(&self) -> u8 {
        match self {
            ClDiffFile::New(_, _) => 0,
            ClDiffFile::Deleted(_, _) => 1,
            ClDiffFile::Renamed(_, _, _, _, _) => 2,
            ClDiffFile::Moved(_, _, _, _, _) => 3,
            ClDiffFile::Modified(_, _, _) => 4,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, str::FromStr};

    use git_internal::{DiffItem, hash::ObjectHash};
    use serde_json::Value;

    use super::{ClDiffFile, ClFilesChangedItemSchema, ClFilesRes};

    #[test]
    fn relocated_files_serialize_as_renamed() {
        let old_hash = ObjectHash::from_str("1111111111111111111111111111111111111111").unwrap();
        let new_hash = ObjectHash::from_str("2222222222222222222222222222222222222222").unwrap();

        let renamed = ClFilesRes::from(ClDiffFile::Renamed(
            PathBuf::from("old_dir/file.rs"),
            PathBuf::from("new_dir/file.rs"),
            old_hash,
            new_hash,
            91,
        ));
        let moved = ClFilesRes::from(ClDiffFile::Moved(
            PathBuf::from("old_dir/file.rs"),
            PathBuf::from("other_dir/file.rs"),
            old_hash,
            new_hash,
            88,
        ));

        let renamed_json: Value = serde_json::to_value(renamed).unwrap();
        let moved_json: Value = serde_json::to_value(moved).unwrap();

        assert_eq!(renamed_json["action"], "renamed");
        assert_eq!(renamed_json["old_path"], "old_dir/file.rs");
        assert_eq!(renamed_json["similarity"], 91);
        assert!(renamed_json.get("display_action").is_none());

        assert_eq!(moved_json["action"], "renamed");
        assert_eq!(moved_json["old_path"], "old_dir/file.rs");
        assert_eq!(moved_json["similarity"], 88);
        assert!(moved_json.get("display_action").is_none());
    }

    #[test]
    fn cl_files_changed_item_schema_omits_old_path_for_non_relocated_items() {
        let regular = ClFilesChangedItemSchema::new(
            DiffItem {
                path: "dir/file.rs".to_string(),
                data: "diff --git a/dir/file.rs b/dir/file.rs\n".to_string(),
            },
            None,
        );
        let renamed = ClFilesChangedItemSchema::new(
            DiffItem {
                path: "new_dir/file.rs".to_string(),
                data: "diff --git a/old_dir/file.rs b/new_dir/file.rs\n".to_string(),
            },
            Some("old_dir/file.rs".to_string()),
        );

        let regular_json: Value = serde_json::to_value(regular).unwrap();
        let renamed_json: Value = serde_json::to_value(renamed).unwrap();

        assert!(regular_json.get("old_path").is_none());
        assert_eq!(renamed_json["old_path"], "old_dir/file.rs");
    }

    #[test]
    fn condition_from_model_maps_all_ci_statuses_without_panicking() {
        use crate::{
            callisto::{check_result, sea_orm_active_enums::CheckTypeEnum},
            ceres::merge_checker::ConditionResult,
        };

        let now = chrono::Utc::now().naive_utc();
        let cases = [
            ("PASSED", ConditionResult::PASSED),
            ("PENDING", ConditionResult::PENDING),
            ("RUNNING", ConditionResult::FAILED),
            ("FAILED", ConditionResult::FAILED),
            ("CANCELLED", ConditionResult::FAILED),
            ("INVALIDATED", ConditionResult::FAILED),
            ("UNEXPECTED_FUTURE_STATUS", ConditionResult::FAILED),
            ("", ConditionResult::FAILED),
        ];

        for (raw, expected) in cases {
            let model = check_result::Model {
                id: 1,
                created_at: now,
                updated_at: now,
                path: "/project/demo".to_owned(),
                cl_link: "CL123456".to_owned(),
                commit_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                check_type_code: CheckTypeEnum::CiStatus,
                status: raw.to_owned(),
                message: format!("status={raw}"),
                head_revision: 3,
                ci_generation: 9,
                ci_task_id: Some("tsk_1".to_owned()),
                ci_attempt_id: Some("att_1".to_owned()),
                details_url: Some("https://ci.example/tasks/tsk_1".to_owned()),
            };
            let cond = super::Condition::try_from(model).expect("supported CiStatus converts");
            assert_eq!(cond.result, expected, "raw status: {raw}");
            assert_eq!(cond.raw_status.as_deref(), Some(raw));
            assert_eq!(
                cond.commit_id.as_deref(),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            );
            assert_eq!(cond.head_revision, Some(3));
            assert_eq!(cond.ci_generation, Some(9));
            assert_eq!(cond.ci_task_id.as_deref(), Some("tsk_1"));
            assert_eq!(cond.ci_attempt_id.as_deref(), Some("att_1"));
            assert_eq!(
                cond.details_url.as_deref(),
                Some("https://ci.example/tasks/tsk_1")
            );
        }

        // Legacy non-CI check rows omit zero/empty optional CI metadata cleanly.
        let legacy = check_result::Model {
            id: 2,
            created_at: now,
            updated_at: now,
            path: "/project/demo".to_owned(),
            cl_link: "CL123456".to_owned(),
            commit_id: String::new(),
            check_type_code: CheckTypeEnum::CommitMessage,
            status: "PASSED".to_owned(),
            message: "ok".to_owned(),
            head_revision: 0,
            ci_generation: 0,
            ci_task_id: None,
            ci_attempt_id: None,
            details_url: None,
        };
        let legacy_json: Value = serde_json::to_value(
            super::Condition::try_from(legacy).expect("supported CommitMessage converts"),
        )
        .unwrap();
        assert_eq!(legacy_json["result"], "PASSED");
        assert_eq!(legacy_json["raw_status"], "PASSED");
        assert!(legacy_json.get("commit_id").is_none());
        assert!(legacy_json.get("head_revision").is_none());
        assert!(legacy_json.get("ci_task_id").is_none());
        assert!(legacy_json.get("ci_attempt_id").is_none());
        assert!(legacy_json.get("ci_generation").is_none());
        assert!(legacy_json.get("details_url").is_none());

        // Retired enum variants (CodeReview / ClaSign) must return Err without panicking.
        for retired in [CheckTypeEnum::CodeReview, CheckTypeEnum::ClaSign] {
            let retired_model = check_result::Model {
                id: 3,
                created_at: now,
                updated_at: now,
                path: "/project/demo".to_owned(),
                cl_link: "CL123456".to_owned(),
                commit_id: String::new(),
                check_type_code: retired,
                status: "PASSED".to_owned(),
                message: "retired".to_owned(),
                head_revision: 0,
                ci_generation: 0,
                ci_task_id: None,
                ci_attempt_id: None,
                details_url: None,
            };
            assert!(
                super::Condition::try_from(retired_model).is_err(),
                "retired check type must return Err instead of panicking"
            );
        }
    }

    #[test]
    fn merge_box_res_requires_all_conditions_passed() {
        use crate::ceres::merge_checker::{CheckType, ConditionResult};

        let make = |result: ConditionResult| super::Condition {
            condition_type: CheckType::CiStatus,
            display_name: "Ci status".to_owned(),
            description: "desc".to_owned(),
            message: "msg".to_owned(),
            result,
            commit_id: None,
            head_revision: None,
            ci_task_id: None,
            ci_attempt_id: None,
            ci_generation: None,
            details_url: None,
            raw_status: None,
        };

        let all_passed = super::MergeBoxRes::from_condition(vec![make(ConditionResult::PASSED)]);
        assert_eq!(
            all_passed.merge_requirements.unwrap().state,
            super::RequirementsState::MERGEABLE
        );

        let with_pending = super::MergeBoxRes::from_condition(vec![
            make(ConditionResult::PASSED),
            make(ConditionResult::PENDING),
        ]);
        assert_eq!(
            with_pending.merge_requirements.unwrap().state,
            super::RequirementsState::UNMERGEABLE
        );

        let with_failed = super::MergeBoxRes::from_condition(vec![
            make(ConditionResult::PASSED),
            make(ConditionResult::FAILED),
        ]);
        assert_eq!(
            with_failed.merge_requirements.unwrap().state,
            super::RequirementsState::UNMERGEABLE
        );
    }

    #[test]
    fn cl_detail_and_item_res_serialize_additive_metadata() {
        use crate::{
            callisto::{mega_cl, sea_orm_active_enums::MergeStatusEnum},
            ceres::model::issue::ItemRes,
            jupiter::model::{
                cl_dto::CLDetails,
                common::{ItemDetails, ItemKind},
            },
        };

        let now = chrono::Utc::now().naive_utc();
        let cl = mega_cl::Model {
            id: 42,
            link: "CLDTO001".to_owned(),
            title: "add ci pipeline".to_owned(),
            merge_date: None,
            status: MergeStatusEnum::Open,
            path: "/project/buck2_test".to_owned(),
            from_hash: "1111111111111111111111111111111111111111".to_owned(),
            to_hash: "2222222222222222222222222222222222222222".to_owned(),
            created_at: now,
            updated_at: now,
            username: "alice".to_owned(),
            base_branch: "main".to_owned(),
            revision: 4,
            head_revision: 5,
        };

        let detail = super::CLDetailRes::from(CLDetails {
            username: "viewer".to_owned(),
            cl: cl.clone(),
            conversations: vec![],
        });
        let detail_json: Value = serde_json::to_value(detail).unwrap();
        assert_eq!(detail_json["link"], "CLDTO001");
        assert_eq!(detail_json["path"], "/project/buck2_test");
        assert_eq!(
            detail_json["from_hash"],
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(
            detail_json["to_hash"],
            "2222222222222222222222222222222222222222"
        );
        assert_eq!(detail_json["head_revision"], 5);
        assert_eq!(detail_json["base_branch"], "main");
        assert_eq!(detail_json["author"], "alice");

        let item = ItemRes::from(ItemDetails {
            item: ItemKind::Cl(cl),
            comment_num: 3,
        });
        let item_json: Value = serde_json::to_value(item).unwrap();
        assert_eq!(item_json["link"], "CLDTO001");
        assert_eq!(item_json["path"], "/project/buck2_test");
        assert_eq!(
            item_json["from_hash"],
            "1111111111111111111111111111111111111111"
        );
        assert_eq!(
            item_json["to_hash"],
            "2222222222222222222222222222222222222222"
        );
        assert_eq!(item_json["head_revision"], 5);
        assert_eq!(item_json["base_branch"], "main");
        assert_eq!(item_json["author"], "alice");
        assert_eq!(item_json["comment_num"], 3);
    }
}
