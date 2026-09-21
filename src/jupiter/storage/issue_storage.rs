use std::ops::Deref;

use sea_orm::{ActiveModelTrait, IntoActiveModel};

use crate::{
    callisto::{issue_cl_references, sea_orm_active_enums::ReferenceTypeEnum},
    common::errors::MegaError,
    jupiter::storage::base_storage::{BaseStorage, StorageConnector},
};

/// Shared CL reference persistence (CE-22: Issue HTTP retired).
#[derive(Clone)]
pub struct IssueStorage {
    pub base: BaseStorage,
}

impl Deref for IssueStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl IssueStorage {
    pub async fn add_reference(
        &self,
        source_id: &str,
        target_id: &str,
        reference_type: ReferenceTypeEnum,
    ) -> Result<issue_cl_references::Model, MegaError> {
        let issue_ref = issue_cl_references::Model {
            source_id: source_id.to_owned(),
            target_id: target_id.to_owned(),
            reference_type,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
        };

        let res = issue_ref
            .into_active_model()
            .insert(self.get_connection())
            .await?;

        Ok(res)
    }
}
