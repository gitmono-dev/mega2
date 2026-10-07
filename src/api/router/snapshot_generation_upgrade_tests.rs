use sea_orm_migration::MigratorTrait;

use super::*;
use crate::jupiter::migration::Migrator;

async fn scalar(db: &sea_orm::DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(sea_orm::Statement::from_string(
        sea_orm::DbBackend::Postgres,
        sql,
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get_by_index(0)
    .unwrap()
}

#[tokio::test]
async fn mst2_generation_additive_upgrade_preserves_legacy_v3_sid_lease_and_new_resolve() {
    let fixture = Fixture::new_with_pg_config(true).await;
    let mono = fixture.state.storage.mono_storage();
    let db = mono.get_connection();
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        0
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE storage_seal IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NOT NULL"
        )
        .await,
        0
    );
    let pages = scalar(db, "SELECT count(*) FROM mst2_metadata_payload").await;
    let original = success_json(fixture.send("GET", "descriptor", Body::empty()).await).await;
    // The unchanged v3 installer created these durable rows. Remove only the
    // empty additive schema in this isolated fixture to reproduce the previous
    // deployed schema; the SID, lease, CAS bytes and all original guards survive.
    db.execute_unprepared(
        "DROP TRIGGER mst2_metadata_generation_mapping_guard ON mst2_metadata_prepare_page;
         DROP TRIGGER mst2_metadata_generation_seal_guard ON mst2_metadata_prepare;
         DROP FUNCTION mst2_metadata_generation_mapping_guard();
         DROP FUNCTION mst2_metadata_generation_seal_guard();
         ALTER TABLE mst2_metadata_prepare DROP COLUMN canonical_bindings,DROP COLUMN bindings_digest,
           DROP COLUMN primary_scope,DROP COLUMN storage_seal;
         ALTER TABLE mst2_metadata_prepare_page DROP COLUMN generation;
         ALTER TABLE mst2_metadata_payload DROP COLUMN generation;
         DROP TABLE mst2_metadata_lifetime;
         DROP FUNCTION mst2_metadata_lifetime_guard();
         DELETE FROM seaql_migrations WHERE version='m20261007_000200_add_mst2_metadata_generations'"
    ).await.unwrap();
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_snapshot_lease WHERE state='ACTIVE'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_snapshot_context").await,
        1
    );
    assert_eq!(
        success_json(fixture.send("GET", "descriptor", Body::empty()).await).await,
        original
    );
    Migrator::up(db, None).await.unwrap();
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_metadata_payload WHERE generation IS NULL"
        )
        .await,
        pages
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE storage_seal IS NULL"
        )
        .await,
        1
    );
    assert_eq!(
        success_json(fixture.send("GET", "descriptor", Body::empty()).await).await,
        original
    );

    let config = fixture.state.storage.config();
    let connection = crate::jupiter::storage::init::postgres_connection(&config.database)
        .await
        .unwrap();
    let storage = crate::jupiter::storage::Storage::new_with_connection(
        config,
        Arc::new(connection),
        fixture.state.storage.git_service.obj_storage.clone(),
    )
    .await
    .unwrap();
    let state = MonoApiServiceState {
        storage,
        ..fixture.state.clone()
    };
    let app = Router::new().nest("/api/v2", routers(state.clone()).with_state(state));
    let restored = success_json(
        app.clone()
            .oneshot(fixture.request("GET", "descriptor", Body::empty()))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(restored, original);
    let next = success_json(
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/snapshots/resolve")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"target":{"kind":"latest"},"scope":"/project"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(next["descriptor"]["snapshot_id"], fixture.snapshot);
    assert_ne!(next["lease_id"], fixture.lease);
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_snapshot_lease WHERE state='ACTIVE'"
        )
        .await,
        2
    );
    let content = app
        .oneshot(fixture.request("GET", "blob?path=/file", Body::empty()))
        .await
        .unwrap();
    assert_eq!(content.status(), 200);
    assert_eq!(
        to_bytes(content.into_body(), usize::MAX)
            .await
            .unwrap()
            .as_ref(),
        fixture.raw.as_slice()
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_metadata_lifetime").await,
        0
    );
    assert_eq!(
        scalar(db, "SELECT count(*) FROM mst2_metadata_payload").await,
        pages
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM mst2_metadata_prepare WHERE storage_seal IS NULL"
        )
        .await,
        1
    );
}
