use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    ceres::model::label::LabelItem,
    jupiter::model::{
        common::{ItemDetails, ItemKind},
    },
};

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
    pub labels: Vec<LabelItem>,
    pub assignees: Vec<String>,
    pub comment_num: usize,
    /// Aggregated Orion build status for this CL (latest task), if any.
    /// Issues and CLs without build data leave this `null`.
    /// Ported from mega@fae6823 `ceres/src/model/issue.rs` (#2163).
    #[serde(default)]
    pub build_status: Option<String>,
}

impl ItemRes {
    /// Fill `build_status` from a `link -> status` map (worst-wins aggregate
    /// from `ClStorage::latest_build_status_by_cl_links`); links absent from
    /// the map stay `None`.
    pub fn apply_build_statuses(
        items: &mut [Self],
        statuses: &std::collections::HashMap<String, String>,
    ) {
        for item in items {
            item.build_status = statuses.get(&item.link).cloned();
        }
    }
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
            labels: value.labels.into_iter().map(|m| m.into()).collect(),
            assignees: value.assignees,
            comment_num: value.comment_num,
            build_status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(link: &str) -> ItemRes {
        ItemRes {
            id: 1,
            link: link.to_string(),
            title: "t".to_string(),
            status: "open".to_string(),
            author: "a".to_string(),
            open_timestamp: 0,
            closed_at: None,
            merge_timestamp: None,
            updated_at: 0,
            labels: vec![],
            assignees: vec![],
            comment_num: 0,
            build_status: None,
        }
    }

    /// SYNC-05 (mega@2398a92, #2163): statuses are backfilled by `link`;
    /// items absent from the map stay `None` (additive compatibility).
    #[test]
    fn apply_build_statuses_backfills_by_link() {
        let mut items = vec![item("CLAAAA"), item("CLBBBB")];
        let statuses =
            std::collections::HashMap::from([("CLAAAA".to_string(), "Failed".to_string())]);

        ItemRes::apply_build_statuses(&mut items, &statuses);

        assert_eq!(items[0].build_status.as_deref(), Some("Failed"));
        assert_eq!(items[1].build_status, None);
    }

    /// SYNC-05: the field serializes (as `null` when unset) so list consumers
    /// see a stable additive schema.
    #[test]
    fn build_status_serializes_as_null_when_unset() {
        let json = serde_json::to_value(item("CLAAAA")).expect("serialize");
        assert_eq!(json["build_status"], serde_json::Value::Null);
    }
}
