use std::collections::HashMap;

use indexmap::IndexMap;

use crate::{
    callisto::{item_assignees, label, mega_conversation, sea_orm_active_enums::ConvTypeEnum},
    jupiter::{model::common::ItemDetails, storage::stg_common::item::ItemEntity},
};

pub mod item;
pub mod query_build;

/// Combine labels, assignees, and conversations into a unified list of `ItemDetails`.
///
/// This function merges multiple related datasets for a list of CL items:
/// * `item_labels` - A vector of tuples where each tuple contains a CL and its associated labels.
/// * `item_assignees` - A vector of tuples where each tuple contains a CL and its associated assignees.
/// * `conversations` - A vector of tuples where each tuple contains a CL and its associated conversations.
///
/// It aggregates the data into a single `ItemDetails` structure for each CL,
///
/// # Returns
///
/// A vector of `ItemDetails` combining the data from the above sources.
pub fn combine_item_list<T>(
    item_labels: Vec<(T::Model, Vec<label::Model>)>,
    item_assignees: Vec<(T::Model, Vec<item_assignees::Model>)>,
    conversations: Vec<(T::Model, Vec<mega_conversation::Model>)>,
) -> Vec<ItemDetails>
where
    T: ItemEntity,
    T::Model: Clone,
{
    let mut conv_map = HashMap::new();
    for (model, convs) in conversations {
        let id = T::get_id(&model);
        conv_map.insert(
            id,
            convs
                .into_iter()
                .filter(|m| m.conv_type == ConvTypeEnum::Comment)
                .collect::<Vec<_>>()
                .len(),
        );
    }

    let mut result: IndexMap<i64, ItemDetails> = IndexMap::new();
    for (model, labels) in item_labels {
        let id = T::get_id(&model);
        result.insert(
            id,
            ItemDetails {
                item: T::item_kind(model),
                labels,
                assignees: vec![],
                comment_num: *conv_map.get(&id).unwrap_or(&0),
            },
        );
    }

    for (model, assignees) in item_assignees {
        let id = T::get_id(&model);
        let assignees = assignees.iter().map(|m| m.assignnee_id.clone()).collect();
        if let Some(entry) = result.get_mut(&id) {
            entry.assignees = assignees;
        } else {
            result.insert(
                id,
                ItemDetails {
                    item: T::item_kind(model),
                    labels: vec![],
                    assignees,
                    comment_num: *conv_map.get(&id).unwrap_or(&0),
                },
            );
        }
    }

    result.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callisto::{
        item_assignees, label, mega_cl, mega_conversation,
        sea_orm_active_enums::{ConvTypeEnum, MergeStatusEnum},
    };

    #[test]
    fn test_combine_item_list() {
        let cl = mega_cl::Model {
            id: 1,
            link: String::from("CLAAAA"),
            title: String::from("sample CL"),
            status: MergeStatusEnum::Open,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            merge_date: None,
            username: String::from("alice"),
            from_hash: String::from("abc"),
            to_hash: String::from("def"),
            path: String::from("/"),
            base_branch: String::from("main"),
        };

        let label = label::Model {
            id: 1,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            name: String::from("bugs"),
            color: String::from("#000000"),
            description: String::from("des"),
        };

        let assignee = item_assignees::Model {
            item_id: 1,
            assignnee_id: "alice".to_string(),
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            item_type: String::from("change_list"),
        };

        let conv = mega_conversation::Model {
            id: 1,
            conv_type: ConvTypeEnum::Comment,
            link: String::from("CLAAAA"),
            comment: None,
            created_at: chrono::Utc::now().naive_utc(),
            updated_at: chrono::Utc::now().naive_utc(),
            username: String::from("alice"),
            resolved: None,
        };

        let item_labels = vec![(cl.clone(), vec![label])];
        let item_assignees = vec![(cl.clone(), vec![assignee])];
        let conversations = vec![(cl.clone(), vec![conv])];

        let results =
            combine_item_list::<mega_cl::Entity>(item_labels, item_assignees, conversations);

        assert_eq!(results.len(), 1);
        let details = &results[0];
        assert_eq!(details.comment_num, 1);
        assert_eq!(details.labels.len(), 1);
        assert_eq!(details.assignees, vec!["alice"]);
    }
}
