use std::ops::Deref;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, EntityTrait, IntoActiveModel, PaginatorTrait,
    QueryFilter, TransactionTrait,
};

use crate::{
    callisto::{issue_cl_references, item_assignees, item_labels, label, sea_orm_active_enums::ReferenceTypeEnum},
    common::errors::MegaError,
    contract::api::common::Pagination,
    jupiter::{
        model::common::LabelAssigneeParams,
        storage::{
            base_storage::{BaseStorage, StorageConnector},
        },
    },
};

/// Shared label/assignee/reference persistence for CL items (CE-22: Issue HTTP retired).
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
    pub async fn new_label(
        &self,
        name: &str,
        color: &str,
        description: &str,
    ) -> Result<label::Model, MegaError> {
        let model = label::Model::new(name, color, description);
        let res = model
            .into_active_model()
            .insert(self.get_connection())
            .await?;
        Ok(res)
    }

    pub async fn get_label_by_id(&self, id: i64) -> Result<Option<label::Model>, MegaError> {
        let model = label::Entity::find_by_id(id)
            .one(self.get_connection())
            .await?;
        Ok(model)
    }

    pub async fn list_labels_by_page(
        &self,
        page: Pagination,
        name: &str,
    ) -> Result<(Vec<label::Model>, u64), MegaError> {
        let mut condition = Condition::all();
        if !name.is_empty() {
            let name = format!("%{name}%");
            condition = condition.add(label::Column::Name.like(name));
        }
        let paginator = label::Entity::find()
            .filter(condition)
            .paginate(self.get_connection(), page.per_page);
        let num_pages = paginator.num_items().await?;
        Ok(paginator
            .fetch_page(page.page - 1)
            .await
            .map(|m| (m, num_pages))?)
    }

    pub async fn find_item_exist_labels(
        &self,
        item_id: i64,
    ) -> Result<Vec<item_labels::Model>, MegaError> {
        let item_labels = item_labels::Entity::find()
            .filter(item_labels::Column::ItemId.eq(item_id))
            .all(self.get_connection())
            .await?;
        Ok(item_labels)
    }

    pub async fn find_item_exist_assignees(
        &self,
        item_id: i64,
    ) -> Result<Vec<item_assignees::Model>, MegaError> {
        let item_assignees = item_assignees::Entity::find()
            .filter(item_assignees::Column::ItemId.eq(item_id))
            .all(self.get_connection())
            .await?;
        Ok(item_assignees)
    }

    pub async fn modify_labels(
        &self,
        to_add: Vec<i64>,
        to_remove: Vec<i64>,
        params: LabelAssigneeParams,
    ) -> Result<(), MegaError> {
        let txn = self.get_connection().begin().await?;

        let LabelAssigneeParams { item_id, item_type } = params;

        if !to_remove.is_empty() {
            item_labels::Entity::delete_many()
                .filter(item_labels::Column::ItemId.eq(item_id))
                .filter(item_labels::Column::LabelId.is_in(to_remove.clone()))
                .exec(&txn)
                .await?;
        }

        if !to_add.is_empty() {
            let mut new_item_labels = Vec::new();
            for label_id in to_add.clone() {
                new_item_labels.push(
                    item_labels::Model {
                        created_at: chrono::Utc::now().naive_utc(),
                        updated_at: chrono::Utc::now().naive_utc(),
                        item_id,
                        label_id,
                        item_type: item_type.clone(),
                    }
                    .into_active_model(),
                );
            }

            item_labels::Entity::insert_many(new_item_labels)
                .exec(&txn)
                .await?;
        }

        txn.commit().await?;

        Ok(())
    }

    pub async fn modify_assignees(
        &self,
        to_add: Vec<String>,
        to_remove: Vec<String>,
        params: LabelAssigneeParams,
    ) -> Result<(), MegaError> {
        let txn = self.get_connection().begin().await?;

        let LabelAssigneeParams { item_id, item_type } = params;

        if !to_remove.is_empty() {
            item_assignees::Entity::delete_many()
                .filter(item_assignees::Column::ItemId.eq(item_id))
                .filter(item_assignees::Column::AssignneeId.is_in(to_remove.clone()))
                .exec(&txn)
                .await?;
        }

        if !to_add.is_empty() {
            let mut new_item = Vec::new();
            for assignnee_id in to_add.clone() {
                new_item.push(
                    item_assignees::Model {
                        created_at: chrono::Utc::now().naive_utc(),
                        updated_at: chrono::Utc::now().naive_utc(),
                        item_id,
                        assignnee_id,
                        item_type: item_type.clone(),
                    }
                    .into_active_model(),
                );
            }

            item_assignees::Entity::insert_many(new_item)
                .exec(&txn)
                .await?;
        }

        txn.commit().await?;

        Ok(())
    }

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
