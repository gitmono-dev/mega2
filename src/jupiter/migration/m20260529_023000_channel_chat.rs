use sea_orm_migration::{prelude::*, schema::*};

use crate::jupiter::migration::pk_bigint;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. channels
        manager
            .create_table(
                table_auto(Channels::Table)
                    .col(pk_bigint(Channels::Id))
                    .col(string_len(Channels::PublicId, 12).unique_key())
                    .col(string_null(Channels::Title))
                    .col(date_time(Channels::LastMessageAt))
                    .col(big_integer_null(Channels::LatestMessageId))
                    .col(integer(Channels::MembersCount).default(0))
                    .col(string_null(Channels::ImagePath))
                    .col(boolean(Channels::Group).default(false))
                    .col(date_time_null(Channels::NotificationForcedAt))
                    .col(string(Channels::OwnerUsername))
                    .col(date_time_null(Channels::DiscardedAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx-channels-last-message-at")
                    .table(Channels::Table)
                    .col(Channels::LastMessageAt)
                    .to_owned(),
            )
            .await?;

        // 2. channel_memberships
        manager
            .create_table(
                table_auto(ChannelMemberships::Table)
                    .col(pk_bigint(ChannelMemberships::Id))
                    .col(big_integer(ChannelMemberships::ChannelId))
                    .col(string(ChannelMemberships::Username))
                    .col(date_time(ChannelMemberships::LastReadAt))
                    .col(date_time_null(ChannelMemberships::ManuallyMarkedUnreadAt))
                    .col(integer(ChannelMemberships::NotificationLevel).default(0))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx-channel-memberships-unique")
                    .table(ChannelMemberships::Table)
                    .col(ChannelMemberships::ChannelId)
                    .col(ChannelMemberships::Username)
                    .unique()
                    .to_owned(),
            )
            .await?;

        // 3. channel_membership_updates
        manager
            .create_table(
                table_auto(ChannelMembershipUpdates::Table)
                    .col(pk_bigint(ChannelMembershipUpdates::Id))
                    .col(big_integer(ChannelMembershipUpdates::ChannelId))
                    .col(string(ChannelMembershipUpdates::ActorUsername))
                    .col(json(ChannelMembershipUpdates::AddedUsernames))
                    .col(json(ChannelMembershipUpdates::RemovedUsernames))
                    .col(date_time_null(ChannelMembershipUpdates::DiscardedAt))
                    .to_owned(),
            )
            .await?;

        // 4. messages
        manager
            .create_table(
                table_auto(Messages::Table)
                    .col(pk_bigint(Messages::Id))
                    .col(big_integer(Messages::ChannelId))
                    .col(string_null(Messages::SenderUsername))
                    .col(text(Messages::Content))
                    .col(string_len(Messages::PublicId, 12).unique_key())
                    .col(big_integer_null(Messages::ReplyToId))
                    .col(string_null(Messages::UnfurledLink))
                    .col(date_time_null(Messages::DiscardedAt))
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx-messages-channel-id")
                    .table(Messages::Table)
                    .col(Messages::ChannelId)
                    .to_owned(),
            )
            .await?;

        // 5. message_notifications
        manager
            .create_table(
                table_auto(MessageNotifications::Table)
                    .col(pk_bigint(MessageNotifications::Id))
                    .col(big_integer(MessageNotifications::ChannelMembershipId))
                    .col(big_integer(MessageNotifications::MessageId))
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Channels {
    Table,
    Id,
    PublicId,
    Title,
    LastMessageAt,
    LatestMessageId,
    MembersCount,
    ImagePath,
    Group,
    NotificationForcedAt,
    OwnerUsername,
    DiscardedAt,
}

#[derive(DeriveIden)]
enum ChannelMemberships {
    Table,
    Id,
    ChannelId,
    Username,
    LastReadAt,
    ManuallyMarkedUnreadAt,
    NotificationLevel,
}

#[derive(DeriveIden)]
enum ChannelMembershipUpdates {
    Table,
    Id,
    ChannelId,
    ActorUsername,
    AddedUsernames,
    RemovedUsernames,
    DiscardedAt,
}

#[derive(DeriveIden)]
enum Messages {
    Table,
    Id,
    ChannelId,
    SenderUsername,
    Content,
    PublicId,
    ReplyToId,
    UnfurledLink,
    DiscardedAt,
}

#[derive(DeriveIden)]
enum MessageNotifications {
    Table,
    Id,
    ChannelMembershipId,
    MessageId,
}
