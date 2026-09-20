use std::ops::Deref;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseTransaction, EntityTrait, IntoActiveModel, QueryFilter,
    Set, prelude::Expr,
};

use crate::{
    callisto::{mega_conversation, sea_orm_active_enums::ConvTypeEnum},
    common::errors::MegaError,
    jupiter::{
        model::conv_dto::ConvWithReactions,
        storage::base_storage::{BaseStorage, StorageConnector},
    },
};

#[derive(Clone)]
pub struct ConversationStorage {
    pub base: BaseStorage,
}

impl Deref for ConversationStorage {
    type Target = BaseStorage;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl ConversationStorage {
    pub async fn add_conversation(
        &self,
        link: &str,
        username: &str,
        comment: Option<String>,
        conv_type: ConvTypeEnum,
    ) -> Result<i64, MegaError> {
        let conversation = mega_conversation::Model::new(link, conv_type, comment, username);
        let conversation = conversation.into_active_model();
        let res = conversation.insert(self.get_connection()).await.unwrap();
        Ok(res.id)
    }

    pub async fn add_conversation_in_txn(
        &self,
        link: &str,
        username: &str,
        comment: Option<String>,
        conv_type: ConvTypeEnum,
        txn: &DatabaseTransaction,
    ) -> Result<i64, MegaError> {
        let conversation = mega_conversation::Model::new(link, conv_type, comment, username);
        let conversation = conversation.into_active_model();
        let res = conversation.insert(txn).await?;
        Ok(res.id)
    }

    pub async fn update_comment(
        &self,
        comment_id: i64,
        comment: Option<String>,
    ) -> Result<(), MegaError> {
        mega_conversation::Entity::update_many()
            .col_expr(mega_conversation::Column::Comment, Expr::value(comment))
            .col_expr(
                mega_conversation::Column::UpdatedAt,
                Expr::value(chrono::Utc::now().naive_utc()),
            )
            .filter(mega_conversation::Column::Id.eq(comment_id))
            .filter(mega_conversation::Column::ConvType.eq(ConvTypeEnum::Comment))
            .exec(self.get_connection())
            .await?;
        Ok(())
    }

    pub async fn remove_conversation(&self, id: i64) -> Result<(), MegaError> {
        mega_conversation::Entity::delete_by_id(id)
            .exec(self.get_connection())
            .await
            .unwrap();
        Ok(())
    }

    pub async fn get_comments_with_reactions(
        &self,
        link: &str,
    ) -> Result<Vec<ConvWithReactions>, MegaError> {
        let conversations = mega_conversation::Entity::find()
            .filter(mega_conversation::Column::Link.eq(link))
            .all(self.get_connection())
            .await?;

        let results = conversations
            .into_iter()
            .map(|conversation| ConvWithReactions { conversation })
            .collect();
        Ok(results)
    }

    pub async fn change_review_state(
        &self,
        cl_link: &str,
        review_id: &i64,
        state: bool,
    ) -> Result<(), MegaError> {
        let mut conversation = mega_conversation::Entity::find()
            .filter(mega_conversation::Column::Id.eq(*review_id))
            .filter(mega_conversation::Column::ConvType.eq(ConvTypeEnum::Review))
            .filter(mega_conversation::Column::Link.eq(cl_link))
            .one(self.get_connection())
            .await
            .map_err(|e| {
                tracing::error!("Error finding conversation: {e}");
                e
            })?
            .ok_or_else(|| MegaError::Other("No conversation found".to_string()))?
            .into_active_model();

        conversation.resolved = Set(Some(state));
        conversation.updated_at = Set(chrono::Utc::now().naive_utc());

        conversation
            .update(self.get_connection())
            .await
            .map_err(|e| MegaError::Other(format!("Error updating conversation: {e}")))?;

        Ok(())
    }
}
