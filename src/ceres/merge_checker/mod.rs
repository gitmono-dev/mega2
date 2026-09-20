use std::{collections::HashMap, fmt, str::FromStr, sync::Arc};

use async_trait::async_trait;
use serde::Serialize;
use utoipa::ToSchema;

use crate::{
    callisto::{check_result, sea_orm_active_enums::CheckTypeEnum},
    ceres::merge_checker::{
        cl_sync_checker::ClSyncChecker, commit_message_checker::CommitMessageChecker,
        gpg_signature_checker::GpgSignatureChecker,
    },
    common::errors::MegaError,
    config::PushPolicy,
    jupiter::{model::cl_dto::ClInfoDto, storage::Storage},
};

pub mod cl_sync_checker;
mod commit_message_checker;
pub(crate) mod gpg_signature_checker;

/// Upper bound of a CL's cumulative `(from_hash → to_hash)` commit range —
/// the single meaning defined by ADR-MC-07. The receive side enforces the
/// same bound on push (MC-03); the GPG signature checker keeps it as defense
/// in depth while walking the parent chain.
pub const MAX_CL_CHAIN_COMMITS: usize = 250;

#[async_trait]
pub trait Checker: Send + Sync {
    async fn run(&self, params: &serde_json::Value) -> CheckResult;

    async fn build_params(&self, cl_info: &ClInfoDto) -> Result<serde_json::Value, MegaError>;
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, ToSchema)]
pub enum CheckType {
    GpgSignature,
    BranchProtection,
    CommitMessage,
    ClSync,
    MergeConflict,
    CiStatus,
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub enum ConditionResult {
    FAILED,
    PASSED,
}

impl fmt::Display for ConditionResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ConditionResult::FAILED => "FAILED",
            ConditionResult::PASSED => "PASSED",
        };
        write!(f, "{}", s)
    }
}

impl FromStr for ConditionResult {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "PASSED" => Ok(ConditionResult::PASSED),
            "FAILED" => Ok(ConditionResult::FAILED),
            _ => Err(()),
        }
    }
}

impl CheckType {
    pub fn display_name(&self) -> &'static str {
        match self {
            CheckType::GpgSignature => "Gpg signature",
            CheckType::BranchProtection => "Branch protection",
            CheckType::CommitMessage => "Commit message",
            CheckType::ClSync => "Cl sync",
            CheckType::MergeConflict => "Merge conflict",
            CheckType::CiStatus => "Ci status",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            CheckType::GpgSignature => {
                "Verify whether the commit has a valid GPG signature and the key is trusted"
            }
            CheckType::BranchProtection => {
                "Ensure the merge target complies with branch protection policies, such as no direct merges to main and requiring squash or rebase"
            }
            CheckType::CommitMessage => {
                "Verify whether the commit message follows Conventional Commits or the internal agreed-upon format"
            }
            CheckType::ClSync => {
                "Ensure the CL is based on the latest commit of the target branch and determine whether a rebase is required"
            }
            CheckType::MergeConflict => {
                "The pull request must not have any unresolved merge conflicts"
            }
            CheckType::CiStatus => {
                "Verify that all required continuous integration pipelines have passed"
            }
        }
    }
}

impl TryFrom<CheckTypeEnum> for CheckType {
    type Error = ();

    fn try_from(value: CheckTypeEnum) -> Result<Self, Self::Error> {
        match value {
            CheckTypeEnum::GpgSignature => Ok(CheckType::GpgSignature),
            CheckTypeEnum::BranchProtection => Ok(CheckType::BranchProtection),
            CheckTypeEnum::CommitMessage => Ok(CheckType::CommitMessage),
            CheckTypeEnum::ClSync => Ok(CheckType::ClSync),
            CheckTypeEnum::MergeConflict => Ok(CheckType::MergeConflict),
            CheckTypeEnum::CiStatus => Ok(CheckType::CiStatus),
            CheckTypeEnum::CodeReview => Err(()),
            CheckTypeEnum::ClaSign => Err(()),
        }
    }
}

impl From<CheckType> for CheckTypeEnum {
    fn from(value: CheckType) -> Self {
        match value {
            CheckType::GpgSignature => CheckTypeEnum::GpgSignature,
            CheckType::BranchProtection => CheckTypeEnum::BranchProtection,
            CheckType::CommitMessage => CheckTypeEnum::CommitMessage,
            CheckType::ClSync => CheckTypeEnum::ClSync,
            CheckType::MergeConflict => CheckTypeEnum::MergeConflict,
            CheckType::CiStatus => CheckTypeEnum::CiStatus,
        }
    }
}

#[derive(Debug)]
pub struct CheckResult {
    pub check_type_code: CheckType,
    pub status: ConditionResult,
    pub message: String,
}

pub struct CheckerRegistry {
    checkers: HashMap<CheckType, Box<dyn Checker>>,
    storage: Arc<Storage>,
    #[allow(dead_code)]
    username: String,
}

impl CheckerRegistry {
    pub fn new(storage: Arc<Storage>, username: String) -> Self {
        let mut r = CheckerRegistry {
            checkers: HashMap::new(),
            storage: storage.clone(),
            username,
        };
        r.register(
            CheckType::ClSync,
            Box::new(ClSyncChecker {
                storage: storage.clone(),
            }),
        );
        r.register(
            CheckType::GpgSignature,
            Box::new(GpgSignatureChecker {
                storage: storage.clone(),
            }),
        );
        r.register(CheckType::CommitMessage, Box::new(CommitMessageChecker));

        r
    }

    pub fn register(&mut self, check_type: CheckType, checker: Box<dyn Checker>) {
        self.checkers.insert(check_type, checker);
    }

    pub async fn run_checks(&self, cl_info: ClInfoDto) -> Result<(), MegaError> {
        if self.storage.config().monorepo.push_policy == PushPolicy::Trunk {
            return Err(MegaError::Other(
                "push_policy=trunk: CheckerRegistry is closed; CL checkers do not run".into(),
            ));
        }
        let check_configs = self
            .storage
            .cl_storage()
            .get_checks_config_by_path(&cl_info.path)
            .await?;
        let mut save_models = vec![];

        for c_config in check_configs {
            let Ok(check_type) = CheckType::try_from(c_config.check_type_code) else {
                continue;
            };
            if let Some(checker) = self.checkers.get(&check_type) {
                let params = checker.build_params(&cl_info).await?;
                let res = checker.run(&params).await;
                let model = check_result::Model::new(
                    &cl_info.path,
                    &cl_info.link,
                    &cl_info.to_hash,
                    res.check_type_code.into(),
                    &res.status.to_string(),
                    &res.message,
                );
                save_models.push(model);
            }
        }
        self.storage
            .cl_storage()
            .save_check_results(save_models)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        callisto::sea_orm_active_enums::MergeStatusEnum, config::PushPolicy,
        jupiter::tests::test_storage_with_config,
    };

    #[tokio::test]
    async fn run_checks_fails_closed_under_trunk() {
        let temp = tempfile::TempDir::new().expect("temp");
        let mut config = crate::config::testing::isolated_config(temp.path().join("config"));
        config.monorepo.push_policy = PushPolicy::Trunk;
        let storage = test_storage_with_config(temp.path(), config).await;
        let registry = CheckerRegistry::new(std::sync::Arc::new(storage), "tester".into());
        let now = chrono::Utc::now().naive_utc();
        let err = registry
            .run_checks(ClInfoDto {
                link: "C0000001".into(),
                title: "t".into(),
                merge_date: None,
                status: MergeStatusEnum::Open,
                path: "/foo".into(),
                from_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                to_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                created_at: now,
                updated_at: now,
                username: "tester".into(),
            })
            .await
            .expect_err("trunk must not run CL checkers");
        assert!(
            err.to_string().contains("CheckerRegistry is closed"),
            "{err}"
        );
    }
}
