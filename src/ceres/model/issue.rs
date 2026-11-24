use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::jupiter::model::common::{ItemDetails, ItemKind};

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ItemRes {
    pub id: i64,
    pub link: String,
    pub title: String,
    pub status: String,
    pub author: String,
    pub open_timestamp: i64,
    pub closed_at: Option<i64>,
    pub merge_timestamp: Option<i64>,
    pub updated_at: i64,
    pub comment_num: usize,
    pub path: String,
    pub from_hash: String,
    pub to_hash: String,
    pub head_revision: i64,
    pub base_branch: String,
}

impl From<ItemDetails> for ItemRes {
    fn from(value: ItemDetails) -> Self {
        let ItemKind::Cl(model) = value.item;
        Self {
            id: model.id,
            link: model.link,
            title: model.title,
            status: format!("{:?}", model.status),
            author: model.username,
            open_timestamp: model.created_at.and_utc().timestamp(),
            merge_timestamp: model.merge_date.map(|dt| dt.and_utc().timestamp()),
            closed_at: None,
            updated_at: model.updated_at.and_utc().timestamp(),
            comment_num: value.comment_num,
            path: model.path,
            from_hash: model.from_hash,
            to_hash: model.to_hash,
            head_revision: model.head_revision,
            base_branch: model.base_branch,
        }
    }
}
