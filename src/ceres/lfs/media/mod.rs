pub mod chunker;
pub mod protocol;
pub mod scope;
pub mod service;

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use super::{
        protocol::{ChunkEntry, CreatedBy, MediaManifest},
        scope::MediaScope,
        service::{MediaError, MediaService, PENDING_TTL_SECS},
    };
    use crate::{
        ceres::lfs::digest::LfsDigest,
        jupiter::{
            service::lfs_service::LfsService,
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                lfs_db_storage::LfsDbStorage,
                object_storage::build_object_storage,
            },
        },
        orbit_api::factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig},
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
}
