use std::collections::HashMap;

use crate::{
    callisto::{mega_conversation, sea_orm_active_enums::ConvTypeEnum},
    jupiter::{model::common::ItemDetails, storage::stg_common::item::ItemEntity},
};

pub mod item;
pub mod query_build;

/// Combine CL rows and conversations into a unified list of `ItemDetails`.
pub fn combine_item_list<T>(
    items: Vec<T::Model>,
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
                .count(),
        );
    }

    items
        .into_iter()
        .map(|model| {
            let id = T::get_id(&model);
            ItemDetails {
                item: T::item_kind(model),
                comment_num: *conv_map.get(&id).unwrap_or(&0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callisto::{
        mega_cl, mega_conversation,
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
            revision: 0,
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

        let conversations = vec![(cl.clone(), vec![conv])];

        let results = combine_item_list::<mega_cl::Entity>(vec![cl], conversations);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].comment_num, 1);
    }
}
