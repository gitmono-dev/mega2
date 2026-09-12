pub mod chunker;
pub mod finalize;
pub mod protocol;
pub mod scope;
pub mod service;

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use super::{
        chunker, finalize,
        protocol::{ChunkEntry, CreatedBy, MediaManifest},
        scope::{MediaObjectKind, MediaScope},
        service::{MediaError, MediaService, PENDING_TTL_SECS},
    };
    use crate::{
        ceres::lfs::digest::LfsDigest,
        jupiter::{
            migration::apply_migrations,
            service::lfs_service::LfsService,
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                lfs_db_storage::LfsDbStorage,
                object_storage::build_object_storage,
            },
            tests::test_db_connection,
        },
        orbit_api::{
            factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig},
            object_storage::{ObjectKey, ObjectNamespace},
        },
    };

    struct Fixture {
        _dir: tempfile::TempDir,
        service: MediaService,
        scope: MediaScope,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: dir.path().to_string_lossy().into_owned(),
            },
            ..Default::default()
        };
        let store = build_object_storage(&cfg).await.unwrap();
        let service = LfsService {
            lfs_storage: LfsDbStorage {
                base: BaseStorage::mock(),
            },
            obj_storage: store,
        }
        .media();
        Fixture {
            _dir: dir,
            service,
            scope: MediaScope::from_server("user-1", "/acme/app").unwrap(),
        }
    }

    fn manifest_for(parts: &[&[u8]]) -> (MediaManifest, Vec<(String, Bytes)>) {
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut bodies = Vec::new();
        let mut all = BytesMut::new();
        for part in parts {
            all.extend_from_slice(part);
            let hash = LfsDigest::sha256_of(part).hex().to_owned();
            chunks.push(ChunkEntry {
                offset,
                length: part.len() as u64,
                chunk_hash: hash.clone(),
                encoded_length: part.len() as u64,
                compression: "none".to_string(),
                checksum: None,
            });
            bodies.push((hash, Bytes::copy_from_slice(part)));
            offset += part.len() as u64;
        }
        let media_oid = LfsDigest::sha256_of(&all).hex().to_owned();
        let manifest = MediaManifest {
            version: 1,
            algorithm: "fastcdc-v1".to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid,
            media_size: offset,
            chunks,
            created_by: CreatedBy {
                client: "test".to_string(),
                version: "0".to_string(),
                capabilities: vec!["fastcdc-v1".to_string()],
            },
            fallback_oid: None,
        };
        (manifest, bodies)
    }

    #[tokio::test]
    async fn prepare_and_upload() {
        let fx = fixture().await;
        let (manifest, bodies) = manifest_for(&[b"alpha-chunk", b"beta-chunk"]);
        assert!(manifest.fallback_oid.is_none());
        let prepared = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), 1_000)
            .await
            .unwrap();
        assert_eq!(prepared.manifest_id, manifest.id().unwrap());
        assert_eq!(
            prepared.missing_chunks,
            vec![bodies[0].0.clone(), bodies[1].0.clone()]
        );

        fx.service
            .upload_chunk_at(
                &fx.scope,
                &prepared.manifest_id,
                &bodies[0].0,
                bodies[0].1.clone(),
                1_000,
            )
            .await
            .unwrap();
        fx.service
            .upload_chunk_at(
                &fx.scope,
                &prepared.manifest_id,
                &bodies[1].0,
                bodies[1].1.clone(),
                1_000,
            )
            .await
            .unwrap();

        let again = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), 1_001)
            .await
            .unwrap();
        assert!(again.missing_chunks.is_empty());
        assert_eq!(again.manifest_id, prepared.manifest_id);

        let got = fx
            .service
            .get_chunk_at(&fx.scope, &prepared.manifest_id, &bodies[0].0, 1_001)
            .await
            .unwrap();
        assert_eq!(got, bodies[0].1);

        let other_hash = "a".repeat(64);
        assert!(matches!(
            fx.service
                .get_chunk_at(&fx.scope, &prepared.manifest_id, &other_hash, 1_001)
                .await,
            Err(MediaError::NotFound)
        ));
        assert!(matches!(
            fx.service
                .upload_chunk_at(
                    &fx.scope,
                    &prepared.manifest_id,
                    &bodies[0].0,
                    Bytes::from_static(b"tampered"),
                    1_001
                )
                .await,
            Err(MediaError::Invalid(_))
        ));

        let stranger = MediaScope::from_server("user-2", "/acme/app").unwrap();
        assert!(matches!(
            fx.service
                .get_chunk_at(&stranger, &prepared.manifest_id, &bodies[0].0, 1_001)
                .await,
            Err(MediaError::NotFound)
        ));

        fx.service
            .overwrite_chunk_for_test(&fx.scope, &bodies[0].0, Bytes::from_static(b"corrupt"))
            .await
            .unwrap();
        assert!(matches!(
            fx.service
                .upload_chunk_at(
                    &fx.scope,
                    &prepared.manifest_id,
                    &bodies[0].0,
                    bodies[0].1.clone(),
                    1_001
                )
                .await,
            Err(MediaError::Conflict(_))
        ));

        let oversized = vec![0u8; super::chunker::MAX_SIZE + 1];
        fx.service
            .overwrite_chunk_for_test(&fx.scope, &bodies[0].0, Bytes::from(oversized))
            .await
            .unwrap();
        assert!(matches!(
            fx.service
                .upload_chunk_at(
                    &fx.scope,
                    &prepared.manifest_id,
                    &bodies[0].0,
                    bodies[0].1.clone(),
                    1_001
                )
                .await,
            Err(MediaError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn resume_and_deduplicate() {
        let fx = fixture().await;
        let repeated = b"same-bytes";
        let (mut manifest, bodies) = manifest_for(&[repeated, b"other-bytes", repeated]);
        assert_eq!(bodies[0].0, bodies[2].0);
        manifest.fallback_oid = Some("b".repeat(64));
        assert!(manifest.validate().is_err());
        manifest.fallback_oid = None;

        let prepared = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), 50)
            .await
            .unwrap();
        assert_eq!(prepared.missing_chunks.len(), 2);
        assert_eq!(prepared.missing_chunks[0], bodies[0].0);
        assert_eq!(prepared.missing_chunks[1], bodies[1].0);

        fx.service
            .upload_chunk_at(
                &fx.scope,
                &prepared.manifest_id,
                &bodies[0].0,
                bodies[0].1.clone(),
                50,
            )
            .await
            .unwrap();
        fx.service
            .upload_chunk_at(
                &fx.scope,
                &prepared.manifest_id,
                &bodies[0].0,
                bodies[0].1.clone(),
                51,
            )
            .await
            .unwrap();

        let resumed = fx
            .service
            .prepare_at(&fx.scope, manifest, 52)
            .await
            .unwrap();
        assert_eq!(resumed.missing_chunks, vec![bodies[1].0.clone()]);

        assert!(matches!(
            fx.service
                .upload_chunk_at(
                    &fx.scope,
                    &prepared.manifest_id,
                    &bodies[1].0,
                    bodies[1].1.clone(),
                    50 + PENDING_TTL_SECS,
                )
                .await,
            Err(MediaError::Invalid(msg)) if msg.contains("expired")
        ));
    }

    struct DbFixture {
        _obj_dir: tempfile::TempDir,
        _db_dir: tempfile::TempDir,
        service: MediaService,
        lfs_db: LfsDbStorage,
        scope: MediaScope,
    }

    async fn db_fixture() -> DbFixture {
        let obj_dir = tempfile::tempdir().unwrap();
        let cfg = ObjectStorageConfig {
            storage_type: ObjectStorageBackend::Local,
            local: LocalConfig {
                root_dir: obj_dir.path().to_string_lossy().into_owned(),
            },
            ..Default::default()
        };
        let store = build_object_storage(&cfg).await.unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let db = test_db_connection(db_dir.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let lfs_db = LfsDbStorage {
            base: BaseStorage::new(std::sync::Arc::new(db)),
        };
        let service = LfsService {
            lfs_storage: lfs_db.clone(),
            obj_storage: store,
        }
        .media();
        DbFixture {
            _obj_dir: obj_dir,
            _db_dir: db_dir,
            service,
            lfs_db,
            scope: MediaScope::from_server("user-1", "/acme/app").unwrap(),
        }
    }

    fn manifest_from_bytes(data: &[u8]) -> (MediaManifest, Vec<(String, Bytes)>) {
        let chunks = chunker::chunk_bytes(data)
            .into_iter()
            .map(|c| ChunkEntry {
                offset: c.offset,
                length: c.length,
                chunk_hash: c.chunk_hash,
                encoded_length: c.length,
                compression: "none".to_string(),
                checksum: None,
            })
            .collect::<Vec<_>>();
        let bodies = chunks
            .iter()
            .map(|c| {
                let start = c.offset as usize;
                let end = start + c.length as usize;
                (
                    c.chunk_hash.clone(),
                    Bytes::copy_from_slice(&data[start..end]),
                )
            })
            .collect();
        let manifest = MediaManifest {
            version: 1,
            algorithm: "fastcdc-v1".to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid: LfsDigest::sha256_of(data).hex().to_owned(),
            media_size: data.len() as u64,
            chunks,
            created_by: CreatedBy {
                client: "test".to_string(),
                version: "0".to_string(),
                capabilities: vec!["fastcdc-v1".to_string()],
            },
            fallback_oid: None,
        };
        (manifest, bodies)
    }

    #[tokio::test]
    async fn finalize_round_trip() {
        let fx = db_fixture().await;
        let data = b"fastcdc-finalize-object";
        let (manifest, bodies) = manifest_from_bytes(data);
        let prepared = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), 10)
            .await
            .unwrap();
        for (hash, body) in &bodies {
            fx.service
                .upload_chunk_at(&fx.scope, &prepared.manifest_id, hash, body.clone(), 10)
                .await
                .unwrap();
        }
        let published = finalize::finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &prepared.manifest_id,
            11,
        )
        .await
        .unwrap();
        assert_eq!(published.manifest_id, prepared.manifest_id);
        assert_eq!(published.manifest.media_oid, manifest.media_oid);
        let lfs_key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: manifest.media_oid.clone(),
        };
        assert!(fx.service.exists(&lfs_key).await.unwrap());
        let row = fx
            .lfs_db
            .get_lfs_object(&manifest.media_oid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.size, data.len() as i64);
        assert!(row.exist);
        let again = finalize::finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &prepared.manifest_id,
            12,
        )
        .await
        .unwrap();
        assert_eq!(again.manifest_id, published.manifest_id);
        let finalized = fx
            .scope
            .object_key(MediaObjectKind::Finalized, &manifest.media_oid)
            .unwrap();
        assert!(fx.service.exists(&finalized).await.unwrap());
    }

    #[tokio::test]
    async fn rejects_corruption_without_publication() {
        let fx = db_fixture().await;
        let data = b"fastcdc-corrupt-object";
        let (manifest, bodies) = manifest_from_bytes(data);
        let prepared = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), 20)
            .await
            .unwrap();
        assert!(
            finalize::finalize_at(
                &fx.service,
                &fx.lfs_db,
                &fx.scope,
                &prepared.manifest_id,
                21
            )
            .await
            .is_err()
        );
        if let Some((hash, body)) = bodies.first() {
            fx.service
                .upload_chunk_at(&fx.scope, &prepared.manifest_id, hash, body.clone(), 22)
                .await
                .unwrap();
            fx.service
                .overwrite_chunk_for_test(&fx.scope, hash, Bytes::from_static(b"nope"))
                .await
                .unwrap();
        }
        assert!(
            finalize::finalize_at(
                &fx.service,
                &fx.lfs_db,
                &fx.scope,
                &prepared.manifest_id,
                23
            )
            .await
            .is_err()
        );
        let lfs_key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: manifest.media_oid.clone(),
        };
        assert!(!fx.service.exists(&lfs_key).await.unwrap());
        let finalized = fx
            .scope
            .object_key(MediaObjectKind::Finalized, &manifest.media_oid)
            .unwrap();
        assert!(!fx.service.exists(&finalized).await.unwrap());
        assert!(
            fx.lfs_db
                .get_lfs_object(&manifest.media_oid)
                .await
                .unwrap()
                .is_none()
        );
    }
}
