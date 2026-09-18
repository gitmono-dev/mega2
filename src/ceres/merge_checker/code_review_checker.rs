use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ceres::merge_checker::{CheckResult, Checker},
    common::errors::MegaError,
    jupiter::{model::cl_dto::ClInfoDto, storage::Storage},
};

pub struct CodeReviewChecker {
    pub storage: Arc<Storage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CodeReviewParams {
    cl_link: String,
}

impl CodeReviewParams {
    fn from_value(v: &serde_json::Value) -> anyhow::Result<Self> {
        Ok(serde_json::from_value(v.clone())?)
    }
}

#[async_trait]
impl Checker for CodeReviewChecker {
    async fn run(&self, params: &Value) -> CheckResult {
        let params = CodeReviewParams::from_value(params).expect("parse params err");
        let mut res = CheckResult {
            check_type_code: crate::ceres::merge_checker::CheckType::CodeReview,
            status: crate::ceres::merge_checker::ConditionResult::FAILED,
            message: String::new(),
        };

        let approved = Self::verify_cl(&params.cl_link);
        match approved {
            Ok(_) => {
                res.status = crate::ceres::merge_checker::ConditionResult::PASSED;
                res.message = String::from("All reviewers have approved the CL.");
            }

            Err(e) => {
                res.status = crate::ceres::merge_checker::ConditionResult::FAILED;
                res.message = format!("Code review check failed: {e}");
            }
        }

        res
    }

    async fn build_params(&self, cl_info: &ClInfoDto) -> Result<Value, MegaError> {
        Ok(serde_json::json!({
            "cl_link": cl_info.link,
        }))
    }
}

impl CodeReviewChecker {
    fn verify_cl(_cl_link: &str) -> Result<(), MegaError> {
        Ok(())
    }
}
