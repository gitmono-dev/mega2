use crate::{
    callisto::{mega_group, mega_group_member},
    ceres::api_service::mono_api_service::MonoApiService,
    common::errors::MegaError,
    contract::api::common::Pagination,
    jupiter::model::group_dto::{CreateGroupPayload, DeleteGroupStats, UpdateGroupPayload},
};

impl MonoApiService {
    pub async fn create_group(
        &self,
        payload: CreateGroupPayload,
    ) -> Result<mega_group::Model, MegaError> {
        self.storage.group_storage().create_group(payload).await
    }

    pub async fn list_groups(
        &self,
        page: Pagination,
    ) -> Result<(Vec<mega_group::Model>, u64), MegaError> {
        self.storage.group_storage().list_groups(page).await
    }

    pub async fn get_group_by_id(
        &self,
        group_id: i64,
    ) -> Result<Option<mega_group::Model>, MegaError> {
        self.storage.group_storage().get_group_by_id(group_id).await
    }

    pub async fn update_group(
        &self,
        group_id: i64,
        payload: UpdateGroupPayload,
    ) -> Result<mega_group::Model, MegaError> {
        self.storage
            .group_storage()
            .update_group(group_id, payload)
            .await
    }

    pub async fn delete_group(&self, group_id: i64) -> Result<DeleteGroupStats, MegaError> {
        let stats = self
            .storage
            .group_storage()
            .delete_group_with_relations(group_id)
            .await?;

        if stats.deleted_groups_count == 0 {
            return Err(MegaError::NotFound(format!(
                "Group not found: {}",
                group_id
            )));
        }

        Ok(stats)
    }

    pub async fn add_group_members(
        &self,
        group_id: i64,
        usernames: Vec<String>,
    ) -> Result<Vec<mega_group_member::Model>, MegaError> {
        let group_storage = self.storage.group_storage();
        group_storage.add_group_members(group_id, &usernames).await
    }

    pub async fn remove_group_member(
        &self,
        group_id: i64,
        username: &str,
    ) -> Result<bool, MegaError> {
        let group_storage = self.storage.group_storage();
        if group_storage.get_group_by_id(group_id).await?.is_none() {
            return Err(MegaError::NotFound(format!(
                "Group not found: {}",
                group_id
            )));
        }
        group_storage.remove_group_member(group_id, username).await
    }

    pub async fn list_group_members(
        &self,
        group_id: i64,
        page: Pagination,
    ) -> Result<(Vec<mega_group_member::Model>, u64), MegaError> {
        let group_storage = self.storage.group_storage();
        if group_storage.get_group_by_id(group_id).await?.is_none() {
            return Err(MegaError::NotFound(format!(
                "Group not found: {}",
                group_id
            )));
        }
        group_storage.list_group_members(group_id, page).await
    }

    pub async fn get_user_groups(
        &self,
        username: &str,
    ) -> Result<Vec<mega_group::Model>, MegaError> {
        self.storage
            .group_storage()
            .find_groups_by_username(username)
            .await
    }
}
