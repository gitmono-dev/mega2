use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let conn = manager.get_connection();

        // 1. custom_reactions: replace case-sensitive unique on `name` with
        //    case-insensitive unique on `lower(name)`.
        if manager.get_database_backend() == DbBackend::Postgres {
            conn.execute_unprepared(
                r#"ALTER TABLE custom_reactions DROP CONSTRAINT IF EXISTS custom_reactions_name_key;"#,
            )
            .await?;
        }
        conn.execute_unprepared(
            r#"CREATE UNIQUE INDEX IF NOT EXISTS "idx-custom-reactions-name-lower" ON custom_reactions (lower(name));"#,
        )
        .await?;

        // 2. reactions: replace the nullable-column unique index with a partial
        //    unique index so that only non-deleted reactions are deduplicated.
        manager
            .drop_index(
                Index::drop()
                    .name("idx-reactions-unique-v2")
                    .table(Reactions::Table)
                    .to_owned(),
            )
            .await?;
        conn.execute_unprepared(
            r#"CREATE UNIQUE INDEX IF NOT EXISTS "idx-reactions-unique-active"
               ON reactions (subject_type, subject_id, username, content, custom_reaction_id)
               WHERE discarded_at IS NULL;"#,
        )
        .await?;

        // 3. channels: add missing indexes.
        manager
            .create_index(
                Index::create()
                    .name("idx-channels-latest-message-id")
                    .table(Channels::Table)
                    .col(Channels::LatestMessageId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-channels-discarded-at")
                    .table(Channels::Table)
                    .col(Channels::DiscardedAt)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-channels-owner-username")
                    .table(Channels::Table)
                    .col(Channels::OwnerUsername)
                    .to_owned(),
            )
            .await?;

        // 4. channel_memberships: add missing indexes.
        manager
            .create_index(
                Index::create()
                    .name("idx-channel-memberships-username")
                    .table(ChannelMemberships::Table)
                    .col(ChannelMemberships::Username)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-channel-memberships-last-read-at")
                    .table(ChannelMemberships::Table)
                    .col(ChannelMemberships::LastReadAt)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-channel-memberships-manually-marked-unread-at")
                    .table(ChannelMemberships::Table)
                    .col(ChannelMemberships::ManuallyMarkedUnreadAt)
                    .to_owned(),
            )
            .await?;

        // 5. channel_membership_updates: add missing indexes.
        manager
            .create_index(
                Index::create()
                    .name("idx-channel-membership-updates-channel-id")
                    .table(ChannelMembershipUpdates::Table)
                    .col(ChannelMembershipUpdates::ChannelId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-channel-membership-updates-actor-username")
                    .table(ChannelMembershipUpdates::Table)
                    .col(ChannelMembershipUpdates::ActorUsername)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-channel-membership-updates-discarded-at")
                    .table(ChannelMembershipUpdates::Table)
                    .col(ChannelMembershipUpdates::DiscardedAt)
                    .to_owned(),
            )
            .await?;

        // 6. messages: add missing indexes.
        manager
            .create_index(
                Index::create()
                    .name("idx-messages-channel-id-id")
                    .table(Messages::Table)
                    .col(Messages::ChannelId)
                    .col(Messages::Id)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-messages-sender-username")
                    .table(Messages::Table)
                    .col(Messages::SenderUsername)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-messages-reply-to-id")
                    .table(Messages::Table)
                    .col(Messages::ReplyToId)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-messages-discarded-at")
                    .table(Messages::Table)
                    .col(Messages::DiscardedAt)
                    .to_owned(),
            )
            .await?;

        // 7. message_notifications: unique constraint + index.
        manager
            .create_index(
                Index::create()
                    .name("idx-message-notifications-unique")
                    .table(MessageNotifications::Table)
                    .col(MessageNotifications::ChannelMembershipId)
                    .col(MessageNotifications::MessageId)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx-message-notifications-message-id")
                    .table(MessageNotifications::Table)
                    .col(MessageNotifications::MessageId)
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
enum Reactions {
    Table,
}

#[derive(DeriveIden)]
enum Channels {
    Table,
    LatestMessageId,
    DiscardedAt,
    OwnerUsername,
}

#[derive(DeriveIden)]
enum ChannelMemberships {
    Table,
    Username,
    LastReadAt,
    ManuallyMarkedUnreadAt,
}

#[derive(DeriveIden)]
enum ChannelMembershipUpdates {
    Table,
    ChannelId,
    ActorUsername,
    DiscardedAt,
}

#[derive(DeriveIden)]
enum Messages {
    Table,
    ChannelId,
    Id,
    SenderUsername,
    ReplyToId,
    DiscardedAt,
}

#[derive(DeriveIden)]
enum MessageNotifications {
    Table,
    ChannelMembershipId,
    MessageId,
}
