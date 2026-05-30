use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

use crate::{
    callisto::{attachment, reactions},
    common::errors::MegaError,
    jupiter::storage::{
        attachment_storage::AttachmentStorage, base_storage::StorageConnector,
        channel_membership_storage::ChannelMembershipStorage,
        custom_reaction_storage::CustomReactionStorage, message_storage::MessageStorage,
        open_graph_storage::OpenGraphStorage, reaction_storage::ReactionStorage,
    },
};

#[derive(Clone)]
pub struct SharedChatService {
    pub attachment_storage: AttachmentStorage,
    pub reaction_storage: ReactionStorage,
    pub custom_reaction_storage: CustomReactionStorage,
    pub open_graph_storage: OpenGraphStorage,
    pub message_storage: MessageStorage,
    pub membership_storage: ChannelMembershipStorage,
}

impl SharedChatService {
    pub fn new(
        attachment_storage: AttachmentStorage,
        reaction_storage: ReactionStorage,
        custom_reaction_storage: CustomReactionStorage,
        open_graph_storage: OpenGraphStorage,
        message_storage: MessageStorage,
        membership_storage: ChannelMembershipStorage,
    ) -> Self {
        Self {
            attachment_storage,
            reaction_storage,
            custom_reaction_storage,
            open_graph_storage,
            message_storage,
            membership_storage,
        }
    }

    pub fn from_storage(storage: &crate::jupiter::storage::Storage) -> Self {
        Self::new(
            storage.attachment_storage(),
            storage.reaction_storage(),
            storage.custom_reaction_storage(),
            storage.open_graph_storage(),
            storage.message_storage(),
            storage.channel_membership_storage(),
        )
    }

    pub async fn create_reaction(
        &self,
        message_public_id: &str,
        username: String,
        content: Option<String>,
        custom_reaction_public_id: Option<String>,
    ) -> Result<reactions::Model, MegaError> {
        let msg = self
            .message_storage
            .get_message_by_public_id(message_public_id)
            .await?
            .ok_or_else(|| {
                MegaError::NotFound(format!("Message {} not found", message_public_id))
            })?;

        // Verify membership of caller in channel
        let mem = self
            .membership_storage
            .get_membership(msg.channel_id, &username)
            .await?;
        if mem.is_none() {
            // Return not found to prevent channel enumeration
            return Err(MegaError::NotFound(format!(
                "Message {} not found",
                message_public_id
            )));
        }

        let custom_reaction_id = if let Some(ref cr_pub) = custom_reaction_public_id {
            let cr = self
                .custom_reaction_storage
                .get_custom_reaction_by_public_id(cr_pub)
                .await?
                .ok_or_else(|| {
                    MegaError::NotFound(format!("Custom reaction {} not found", cr_pub))
                })?;
            Some(cr.id)
        } else {
            None
        };

        // Check if reaction already exists to prevent duplicate reaction insertion
        let existing = self
            .reaction_storage
            .get_reactions_by_subject("Message", msg.id)
            .await?;
        let duplicate = existing.iter().any(|r| {
            r.username == username
                && r.content == content
                && r.custom_reaction_id == custom_reaction_id
        });
        if duplicate {
            return Err(MegaError::Other("Reaction already exists".into()));
        }

        let public_id = crate::callisto::entity_ext::generate_public_id();
        let reaction = self
            .reaction_storage
            .create_reaction(
                public_id,
                content,
                "Message".to_string(),
                msg.id,
                username,
                custom_reaction_id,
            )
            .await?;

        Ok(reaction)
    }

    pub async fn delete_reaction(
        &self,
        reaction_public_id: &str,
        username: &str,
    ) -> Result<(), MegaError> {
        let reaction = reactions::Entity::find()
            .filter(reactions::Column::PublicId.eq(reaction_public_id))
            .filter(reactions::Column::DiscardedAt.is_null())
            .one(self.reaction_storage.get_connection())
            .await?
            .ok_or_else(|| {
                MegaError::NotFound(format!("Reaction {} not found", reaction_public_id))
            })?;

        if reaction.username != username {
            return Err(MegaError::Other("Only reaction owner can delete".into()));
        }

        self.reaction_storage
            .soft_delete_reaction(reaction_public_id)
            .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_attachment(
        &self,
        message_public_id: &str,
        username: &str,
        file_path: String,
        file_type: String,
        name: String,
        size: i64,
        position: i32,
    ) -> Result<attachment::Model, MegaError> {
        let msg = self
            .message_storage
            .get_message_by_public_id(message_public_id)
            .await?
            .ok_or_else(|| {
                MegaError::NotFound(format!("Message {} not found", message_public_id))
            })?;

        // Verify membership of caller in channel
        let mem = self
            .membership_storage
            .get_membership(msg.channel_id, username)
            .await?;
        if mem.is_none() {
            return Err(MegaError::NotFound(format!(
                "Message {} not found",
                message_public_id
            )));
        }

        let public_id = crate::callisto::entity_ext::generate_public_id();
        let att = self
            .attachment_storage
            .create_attachment(
                public_id,
                file_path,
                file_type,
                "Message".to_string(),
                msg.id,
                name,
                size,
                position,
            )
            .await?;

        Ok(att)
    }
}
