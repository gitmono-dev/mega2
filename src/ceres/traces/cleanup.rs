//! Crash recovery for traces staging. Published membership stays.

use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter};

use crate::{
    common::errors::{MegaError, TracesError},
    jupiter::storage::{object_storage::MegaObjectStorageWrapper, trace_object_storage},
};

/// Delete unpublished quarantine objects and leave visible membership in place.
pub async fn retain_published_membership<C: ConnectionTrait>(
    conn: &C,
    store: &MegaObjectStorageWrapper,
    stream_id: &str,
    ingest_id: &str,
    hash_kind: &str,
    staged_oids: &[String],
) -> Result<Vec<String>, MegaError> {
    let before = visible_memberships(conn, stream_id).await?;
    let deleted = trace_object_storage::recover_quarantine_objects(
        conn,
        store,
        stream_id,
        ingest_id,
        hash_kind,
        staged_oids,
    )
    .await?;
    let after = visible_memberships(conn, stream_id).await?;
    if before != after {
        return Err(MegaError::Traces(TracesError::ObjectIntegrity));
    }
    Ok(deleted)
}

async fn visible_memberships<C: ConnectionTrait>(
    conn: &C,
    stream_id: &str,
) -> Result<u64, MegaError> {
    crate::callisto::trace_object::Entity::find()
        .filter(crate::callisto::trace_object::Column::StreamId.eq(stream_id))
        .filter(crate::callisto::trace_object::Column::Visible.eq(true))
        .count(conn)
        .await
        .map_err(MegaError::from)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, ColumnTrait, QueryFilter, Set};
    use uuid::Uuid;

    use super::*;
    use crate::{
        callisto::{trace_object, trace_principal, trace_stream, trace_target},
        jupiter::{
            migration::apply_migrations,
            storage::{
                object_storage::mock_object_storage,
                trace_object_storage::{get_raw, get_staging, put_raw, put_staging},
            },
            tests::test_db_connection,
        },
    };

    const INGEST: &str = "33333333-3333-4333-8333-333333333333";
    const OTHER_INGEST: &str = "44444444-4444-4444-8444-444444444444";

    async fn open_db() -> (tempfile::TempDir, sea_orm::DatabaseConnection) {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let db = test_db_connection(dir.path()).await;
        apply_migrations(&db, true).await.expect("migrations");
        (dir, db)
    }

    async fn stream(db: &sea_orm::DatabaseConnection) -> String {
        let principal_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let now = Utc::now().fixed_offset();
        trace_principal::ActiveModel {
            id: Set(principal_id.clone()),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("principal");
        trace_target::ActiveModel {
            id: Set(target_id.clone()),
            target_kind: Set("native".to_owned()),
            project_path: Set(format!("/team/{target_id}")),
            backing_import_repo_id: Set(None),
            active: Set(true),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("target");
        trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal_id),
            target_id: Set(target_id),
            source_repo_id: Set("source-opaque".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .expect("stream")
        .id
    }

    #[tokio::test]
    async fn published_membership_retention_keeps_visible_bytes() {
        let (_dir, db) = open_db().await;
        let store = mock_object_storage();
        let stream_id = stream(&db).await;
        let published = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let unpublished = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        put_raw(&store, &stream_id, "sha1", published, b"published-bytes")
            .await
            .expect("durable");
        put_staging(&store, INGEST, "sha1", published, b"published-bytes")
            .await
            .expect("published staging");
        put_staging(&store, INGEST, "sha1", unpublished, b"staged-only")
            .await
            .expect("unpublished staging");
        put_staging(&store, OTHER_INGEST, "sha1", unpublished, b"other-ingest")
            .await
            .expect("other ingest");
        trace_object::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            hash_kind: Set("sha1".to_owned()),
            oid: Set(published.to_owned()),
            visible: Set(true),
            created_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&db)
        .await
        .expect("membership");

        let deleted = retain_published_membership(
            &db,
            &store,
            &stream_id,
            INGEST,
            "sha1",
            &[published.to_owned(), unpublished.to_owned()],
        )
        .await
        .expect("recover");
        assert_eq!(deleted, vec![unpublished.to_owned()]);
        assert_eq!(
            get_staging(&store, INGEST, "sha1", published)
                .await
                .expect("published staging remains"),
            Some(b"published-bytes".to_vec())
        );
        assert_eq!(
            get_staging(&store, INGEST, "sha1", unpublished)
                .await
                .expect("unpublished staging"),
            None
        );
        assert_eq!(
            get_staging(&store, OTHER_INGEST, "sha1", unpublished)
                .await
                .expect("other ingest remains"),
            Some(b"other-ingest".to_vec())
        );
        assert_eq!(
            get_raw(&store, &stream_id, "sha1", published)
                .await
                .expect("durable remains"),
            Some(b"published-bytes".to_vec())
        );
        let row = trace_object::Entity::find()
            .filter(trace_object::Column::StreamId.eq(&stream_id))
            .filter(trace_object::Column::Visible.eq(true))
            .one(&db)
            .await
            .expect("membership query")
            .expect("visible row");
        assert_eq!(row.oid, published);
    }
}
