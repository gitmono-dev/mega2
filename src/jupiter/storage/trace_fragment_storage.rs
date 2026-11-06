//! Persist logical transcript fragments. Identical content in one stream and
//! index version keeps a single row; membership is recorded separately.

use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};
use uuid::Uuid;

use crate::{
    callisto::trace_fragment::{self, membership},
    ceres::traces::fragments::TextFragment,
    common::errors::MegaError,
};

pub async fn store_fragments(
    txn: &impl ConnectionTrait,
    stream_id: &str,
    checkpoint_id: &str,
    resource_id: &str,
    index_version: i64,
    fragments: &[TextFragment],
) -> Result<(), MegaError> {
    if index_version < 1 {
        return Err(MegaError::Other(
            "trace fragment index version must be positive".into(),
        ));
    }
    for fragment in fragments {
        let existing = trace_fragment::Entity::find()
            .filter(trace_fragment::Column::StreamId.eq(stream_id))
            .filter(trace_fragment::Column::ContentDigest.eq(fragment.content_digest.clone()))
            .filter(trace_fragment::Column::IndexVersion.eq(index_version))
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
                index_version: Set(index_version),
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};
    use uuid::Uuid;

    use super::*;
    use crate::{
        callisto::{
            trace_checkpoint, trace_fragment::membership, trace_principal, trace_resource,
            trace_stream, trace_target,
        },
        ceres::traces::fragments::TextFragment,
        jupiter::{migration::apply_migrations, tests::test_db_connection},
    };

    async fn resource(db: &sea_orm::DatabaseConnection) -> (String, String, String) {
        let now = Utc::now().fixed_offset();
        let principal = trace_principal::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
        let target_id = Uuid::new_v4().to_string();
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/project/{target_id}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
        let stream_id = trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal.id),
            target_id: Set(target_id),
            source_repo_id: Set("fragment-source".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap()
        .id;
        let checkpoint_id = Uuid::new_v4().to_string();
        trace_checkpoint::ActiveModel {
            id: Set(checkpoint_id.clone()),
            stream_id: Set(stream_id.clone()),
            checkpoint_key: Set(Uuid::new_v4().to_string()),
            original_tree: Set("a".repeat(40)),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
        let resource_id = Uuid::new_v4().to_string();
        trace_resource::ActiveModel {
            id: Set(resource_id.clone()),
            stream_id: Set(stream_id.clone()),
            checkpoint_id: Set(checkpoint_id.clone()),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(1),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap();
        (stream_id, checkpoint_id, resource_id)
    }

    fn piece(body: &str) -> TextFragment {
        crate::ceres::traces::fragments::split_utf8(body.as_bytes())
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    async fn database() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let directory = tempfile::tempdir().unwrap();
        let db = test_db_connection(directory.path()).await;
        apply_migrations(&db, true).await.unwrap();
        (directory, db)
    }

    #[tokio::test]
    async fn fragment_membership_dedup_shares_one_digest_row() {
        let (_dir, db) = database().await;
        let (stream, checkpoint_a, resource_a) = resource(&db).await;
        let now = Utc::now().fixed_offset();
        let checkpoint_b = Uuid::new_v4().to_string();
        trace_checkpoint::ActiveModel {
            id: Set(checkpoint_b.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_key: Set("dedup-b".to_owned()),
            original_tree: Set("b".repeat(40)),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        let resource_b = Uuid::new_v4().to_string();
        trace_resource::ActiveModel {
            id: Set(resource_b.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_id: Set(checkpoint_b.clone()),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(1),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        let fragment = piece("same-bytes");
        let txn = db.begin().await.unwrap();
        store_fragments(
            &txn,
            &stream,
            &checkpoint_a,
            &resource_a,
            1,
            std::slice::from_ref(&fragment),
        )
        .await
        .unwrap();
        store_fragments(&txn, &stream, &checkpoint_b, &resource_b, 1, &[fragment])
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let rows = trace_fragment::Entity::find()
            .filter(trace_fragment::Column::StreamId.eq(stream))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "one stream and version keeps one digest row");
    }

    #[tokio::test]
    async fn fragment_membership_keeps_every_checkpoint_and_resource() {
        let (_dir, db) = database().await;
        let (stream, checkpoint_a, resource_a) = resource(&db).await;
        let checkpoint_b = Uuid::new_v4().to_string();
        let now = Utc::now().fixed_offset();
        trace_checkpoint::ActiveModel {
            id: Set(checkpoint_b.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_key: Set("second".to_owned()),
            original_tree: Set("b".repeat(40)),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        let resource_b = Uuid::new_v4().to_string();
        trace_resource::ActiveModel {
            id: Set(resource_b.clone()),
            stream_id: Set(stream.clone()),
            checkpoint_id: Set(checkpoint_b.clone()),
            ordinal: Set(0),
            byte_start: Set(0),
            byte_end: Set(1),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        let fragment = piece("same-bytes");
        let txn = db.begin().await.unwrap();
        store_fragments(
            &txn,
            &stream,
            &checkpoint_a,
            &resource_a,
            1,
            std::slice::from_ref(&fragment),
        )
        .await
        .unwrap();
        store_fragments(&txn, &stream, &checkpoint_b, &resource_b, 1, &[fragment])
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let links = membership::Entity::find()
            .filter(membership::Column::StreamId.eq(stream))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(links.len(), 2);
        assert!(
            links
                .iter()
                .any(|link| link.checkpoint_id == checkpoint_a && link.resource_id == resource_a)
        );
        assert!(
            links
                .iter()
                .any(|link| link.checkpoint_id == checkpoint_b && link.resource_id == resource_b)
        );
    }
}
