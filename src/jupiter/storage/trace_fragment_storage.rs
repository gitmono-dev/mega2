//! Persist logical transcript fragments. Identical content in one stream and
//! index version keeps a single row; membership is recorded separately.

use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};
use uuid::Uuid;

use crate::{
    callisto::trace_fragment::{self, membership},
    ceres::traces::fragments::{FRAGMENT_INDEX_VERSION, TextFragment},
    common::errors::MegaError,
};

pub async fn store_fragments(
    txn: &impl ConnectionTrait,
    stream_id: &str,
    checkpoint_id: &str,
    resource_id: &str,
    fragments: &[TextFragment],
) -> Result<(), MegaError> {
    for fragment in fragments {
        let existing = trace_fragment::Entity::find()
            .filter(trace_fragment::Column::StreamId.eq(stream_id))
            .filter(trace_fragment::Column::ContentDigest.eq(fragment.content_digest.clone()))
            .filter(trace_fragment::Column::IndexVersion.eq(FRAGMENT_INDEX_VERSION))
            .one(txn)
            .await?;
        let fragment_id = if let Some(existing) = existing {
            existing.id
        } else {
            let id = Uuid::new_v4().to_string();
            trace_fragment::ActiveModel {
                id: Set(id.clone()),
                stream_id: Set(stream_id.to_owned()),
                content_digest: Set(fragment.content_digest.clone()),
                index_version: Set(FRAGMENT_INDEX_VERSION),
                body: Set(fragment.body.clone()),
                ascii_fold: Set(fragment.ascii_fold.clone()),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(txn)
            .await?;
            id
        };
        let linked = membership::Entity::find()
            .filter(membership::Column::FragmentId.eq(fragment_id.clone()))
            .filter(membership::Column::CheckpointId.eq(checkpoint_id))
            .filter(membership::Column::ResourceId.eq(resource_id))
            .one(txn)
            .await?;
        if linked.is_none() {
            membership::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                fragment_id: Set(fragment_id),
                stream_id: Set(stream_id.to_owned()),
                checkpoint_id: Set(checkpoint_id.to_owned()),
                resource_id: Set(resource_id.to_owned()),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(txn)
            .await?;
        }
    }
    Ok(())
}
