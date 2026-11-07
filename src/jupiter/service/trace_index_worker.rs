//! Background indexer for the durable trace ref journal.
//!
//! The worker reads only `trace_ref_update`. It does not watch the Git
//! transport, and it does not move `trace_stream.tip_oid`.

use std::time::Duration;

use sea_orm::DatabaseConnection;
use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    jupiter::{
        service::trace_object_service::TraceObjectService,
        storage::trace_projection_storage::TraceProjectionStore,
    },
};

/// How often a running service looks for a new journal row.
pub const INDEX_INTERVAL: Duration = Duration::from_millis(200);

/// Process-wide indexer started with the service.
#[derive(Clone)]
pub struct TraceIndexWorker {
    db: DatabaseConnection,
    objects: TraceObjectService,
}

impl TraceIndexWorker {
    pub fn new(db: DatabaseConnection, objects: TraceObjectService) -> Self {
        Self { db, objects }
    }

    pub fn spawn_background(self, shutdown: CancellationToken) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(INDEX_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if shutdown.is_cancelled() {
                            break;
                        }
                        if let Err(error) = self.index_once().await {
                            tracing::error!(error = %error, "trace index worker cycle failed");
                        }
                    }
                }
            }
        });
    }

    pub async fn index_once(&self) -> Result<usize, MegaError> {
        TraceProjectionStore::new(&self.db, &self.objects)
            .index_available()
            .await
    }

    /// Rebuild fragment rows for one stream at `index_version` from raw objects.
    ///
    /// Already indexed journal rows are reread in `ref_sequence` order. A failure
    /// stops the replay, so a later sequence is not indexed ahead of an earlier
    /// one. Raw objects are not written.
    pub async fn rebuild_index_version(
        &self,
        stream_id: &str,
        index_version: i64,
    ) -> Result<(), MegaError> {
        TraceProjectionStore::new(&self.db, &self.objects)
            .rebuild_indexed(stream_id, index_version)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use git_internal::hash::HashKind;
    use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::*;
    use crate::{
        callisto::{
            trace_fragment, trace_object, trace_principal, trace_ref_update, trace_stream,
            trace_target,
        },
        ceres::traces::publication::tests::{Fixture, local_store},
        jupiter::{
            migration::apply_migrations, service::trace_object_service::TraceObjectService,
            tests::test_db_connection,
        },
    };

    /// A stream whose single supported checkpoint journal row is already indexed.
    struct IndexedStream {
        _directory: tempfile::TempDir,
        _store_dir: tempfile::TempDir,
        db: sea_orm::DatabaseConnection,
        objects: TraceObjectService,
        fixture: Fixture,
        stream_id: String,
        commit: String,
    }

    async fn indexed_stream() -> IndexedStream {
        let directory = tempfile::tempdir().unwrap();
        let db = test_db_connection(directory.path()).await;
        apply_migrations(&db, true).await.unwrap();
        let (store_dir, store) = local_store().await;
        let objects = TraceObjectService::new(store);
        let mut fixture = Fixture::new(HashKind::Sha1);
        let metadata = br#"{"schema_version":2,"checkpoint_id":"abcd","agent_kind":"claude_code","created_at":1700000000}"#;
        let events = b"{}\n";
        let transcript = b"hello-transcript";
        let report = b"{}";
        let mut covered = Sha256::new();
        covered.update(metadata);
        covered.update(events);
        covered.update(transcript);
        covered.update(report);
        let hash_file = format!("sha256:{}", hex::encode(covered.finalize()));
        let metadata_oid = fixture.object("blob", metadata);
        let events_oid = fixture.object("blob", events);
        let transcript_oid = fixture.object("blob", transcript);
        let report_oid = fixture.object("blob", report);
        let hash_oid = fixture.object("blob", hash_file.as_bytes());
        let manifest = serde_json::json!({
            "schema_version": 1,
            "checkpoint_id": "abcd",
            "content_hash": {
                "algorithm": "sha256",
                "path": "content_hash.txt",
                "coverage": ["metadata", "lifecycle_events", "transcript", "redaction_report"]
            },
            "entries": {
                "metadata": {"path": "metadata.json", "oid": metadata_oid, "byte_len": metadata.len()},
                "lifecycle_events": {"path": "events/lifecycle.jsonl", "oid": events_oid, "byte_len": events.len(), "schema_version": 2},
                "transcript": {"path": "transcript/t.jsonl", "oid": transcript_oid, "byte_len": transcript.len()},
                "redaction_report": {"path": "redaction_report.json", "oid": report_oid, "byte_len": report.len()},
                "content_hash": {"path": "content_hash.txt", "oid": hash_oid, "byte_len": hash_file.len()}
            }
        });
        let manifest_oid = fixture.object("blob", &serde_json::to_vec(&manifest).unwrap());
        let events_tree = fixture.tree(&[("100644", b"lifecycle.jsonl", &events_oid)]);
        let transcript_tree = fixture.tree(&[("100644", b"t.jsonl", &transcript_oid)]);
        let leaf = fixture.tree(&[
            ("100644", b"content_hash.txt", &hash_oid),
            ("40000", b"events", &events_tree),
            ("100644", b"manifest.json", &manifest_oid),
            ("100644", b"metadata.json", &metadata_oid),
            ("100644", b"redaction_report.json", &report_oid),
            ("40000", b"transcript", &transcript_tree),
        ]);
        let prefix = fixture.tree(&[("40000", b"cd", &leaf)]);
        let checkpoints = fixture.tree(&[("40000", b"ab", &prefix)]);
        let root = fixture.tree(&[("40000", b"checkpoint", &checkpoints)]);
        let commit = fixture.commit(&root, None);
        let now = Utc::now().fixed_offset();
        let principal = trace_principal::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            created_at: Set(now),
        }
        .insert(&db)
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
        .insert(&db)
        .await
        .unwrap();
        let stream_id = trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal.id),
            target_id: Set(target_id),
            source_repo_id: Set("rebuild-source".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set("sha1".to_owned()),
            tip_oid: Set(Some(commit.clone())),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap()
        .id;
        for (oid, _kind, bytes) in &fixture.objects {
            objects
                .put_raw(&stream_id, "sha1", oid, bytes)
                .await
                .unwrap();
            trace_object::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                stream_id: Set(stream_id.clone()),
                hash_kind: Set("sha1".to_owned()),
                oid: Set(oid.clone()),
                visible: Set(true),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(&db)
            .await
            .unwrap();
        }
        trace_ref_update::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            stream_id: Set(stream_id.clone()),
            ref_sequence: Set(1),
            idempotency_key: Set(Uuid::new_v4().to_string()),
            old_tip: Set(None),
            new_tip: Set(commit.clone()),
            state: Set("pending".to_owned()),
            claim_until: Set(None),
            attempt_count: Set(0),
            created_at: Set(now),
        }
        .insert(&db)
        .await
        .unwrap();
        let worker = TraceIndexWorker::new(db.clone(), objects.clone());
        assert_eq!(worker.index_once().await.unwrap(), 1);
        IndexedStream {
            _directory: directory,
            _store_dir: store_dir,
            db,
            objects,
            fixture,
            stream_id,
            commit,
        }
    }

    #[tokio::test]
    async fn fragment_index_version_rebuild_keeps_raw_bytes() {
        let IndexedStream {
            _directory,
            _store_dir,
            db,
            objects,
            fixture,
            stream_id,
            commit,
        } = indexed_stream().await;
        let worker = TraceIndexWorker::new(db.clone(), objects.clone());
        let mut before = BTreeMap::new();
        for (oid, _kind, bytes) in &fixture.objects {
            let stored = objects.get_raw(&stream_id, "sha1", oid).await.unwrap();
            assert_eq!(stored.as_deref(), Some(bytes.as_slice()));
            before.insert(oid.clone(), stored);
        }
        worker.rebuild_index_version(&stream_id, 2).await.unwrap();
        for (oid, bytes) in &before {
            let stored = objects.get_raw(&stream_id, "sha1", oid).await.unwrap();
            assert_eq!(&stored, bytes, "rebuild must not rewrite raw object {oid}");
        }
        let version_rows = |version: i64| {
            let db = db.clone();
            let stream_id = stream_id.clone();
            async move {
                trace_fragment::Entity::find()
                    .filter(trace_fragment::Column::StreamId.eq(stream_id))
                    .filter(trace_fragment::Column::IndexVersion.eq(version))
                    .all(&db)
                    .await
                    .unwrap()
                    .len()
            }
        };
        assert!(version_rows(1).await >= 1);
        assert_eq!(version_rows(1).await, version_rows(2).await);
        let tip = trace_stream::Entity::find_by_id(stream_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tip.tip_oid.as_deref(), Some(commit.as_str()));
    }

    #[tokio::test]
    async fn gin_independent_watermark_does_not_mark_search_ready() {
        use crate::jupiter::{
            service::trace_search_index_service::{IndexState, TraceSearchIndexService},
            storage::trace_search_index_storage::CatalogFailure,
        };

        let IndexedStream {
            _directory,
            _store_dir,
            db,
            stream_id,
            ..
        } = indexed_stream().await;
        // The journal watermark has caught up and fragments exist.
        let pending = trace_ref_update::Entity::find()
            .filter(trace_ref_update::Column::StreamId.eq(stream_id.clone()))
            .all(&db)
            .await
            .unwrap();
        assert!(!pending.is_empty());
        assert!(pending.iter().all(|row| row.state == "indexed"));
        let fragments = trace_fragment::Entity::find()
            .filter(trace_fragment::Column::StreamId.eq(stream_id))
            .all(&db)
            .await
            .unwrap();
        assert!(!fragments.is_empty());
        // This cloned test database has no pg_trgm, so search stays unavailable.
        let service = TraceSearchIndexService::new(db);
        assert_eq!(
            service.ensure_once().await.unwrap(),
            IndexState::Unavailable(CatalogFailure::ExtensionMissing)
        );
        assert_ne!(service.state(), IndexState::Ready);
    }
}
