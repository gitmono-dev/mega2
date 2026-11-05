//! Checkpoint projection read from one published traces commit (MTR-14).
//!
//! Every object is read through [`TraceObjectService::read_verified_raw`]
//! after its visible `trace_object` membership in the same stream is
//! checked, so an OID published in another stream cannot be borrowed. The
//! parser in [`crate::ceres::traces::parser`] decides layout and role
//! validity; this service only loads the bytes it asks for. Nothing is
//! written: an invalid checkpoint is reported and the archive is unchanged.

use std::time::{Duration, Instant};

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::{
    callisto::{trace_object, trace_stream},
    ceres::traces::{
        ingest::{RawTreeMode, verified_commit_links},
        parser::{self, CheckpointLocation, CheckpointParse, InvalidCheckpoint, MAX_CHECKPOINTS},
    },
    common::errors::{MegaError, TracesError},
    jupiter::service::trace_object_service::{RawObjectType, RawReadControl, TraceObjectService},
};

/// Deadline control for projection reads. Background projection work has no
/// request cancellation, only this wall-clock budget.
pub(crate) struct ProjectionControl {
    deadline: Instant,
}

impl ProjectionControl {
    pub(crate) fn new(budget: Duration) -> Self {
        Self {
            deadline: Instant::now() + budget,
        }
    }
}

impl RawReadControl for ProjectionControl {
    fn check(&self) -> Result<(), MegaError> {
        if Instant::now() >= self.deadline {
            Err(TracesError::QueryTimeout.into())
        } else {
            Ok(())
        }
    }

    async fn checkpoint(&self) -> Result<(), MegaError> {
        self.check()?;
        tokio::task::yield_now().await;
        self.check()
    }

    async fn run<T: Send>(
        &self,
        future: impl std::future::Future<Output = Result<T, MegaError>> + Send,
    ) -> Result<T, MegaError> {
        self.check()?;
        match tokio::time::timeout_at(self.deadline.into(), future).await {
            Ok(value) => {
                self.check()?;
                value
            }
            Err(_) => Err(TracesError::QueryTimeout.into()),
        }
    }
}

/// Checkpoints located in one commit, in raw tree order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitCheckpoints {
    pub(crate) commit_oid: String,
    pub(crate) root_tree: String,
    pub(crate) parents: Vec<String>,
    pub(crate) checkpoints: Vec<CheckpointParse>,
}

#[derive(Clone)]
pub(crate) struct TraceProjectionService {
    objects: TraceObjectService,
}

impl TraceProjectionService {
    pub(crate) fn new(objects: TraceObjectService) -> Self {
        Self { objects }
    }

    /// Parse every checkpoint in the tree of `commit_oid` in `stream_id`.
    ///
    /// Missing or foreign objects are an `object_integrity` error, not an
    /// invalid checkpoint: the published closure guarantees them.
    pub(crate) async fn checkpoints_at_commit(
        &self,
        db: &DatabaseConnection,
        stream_id: &str,
        commit_oid: &str,
        control: &impl RawReadControl,
    ) -> Result<CommitCheckpoints, MegaError> {
        let stream = trace_stream::Entity::find_by_id(stream_id.to_owned())
            .one(db)
            .await?
            .ok_or(TracesError::UnknownScope)?;
        let hash_bytes = match stream.hash_kind.as_str() {
            "sha1" => 20,
            "sha256" => 32,
            _ => return Err(TracesError::ObjectIntegrity.into()),
        };
        let reader = StreamReader {
            objects: &self.objects,
            db,
            stream_id,
            hash_kind: &stream.hash_kind,
            control,
        };
        let commit_oid = commit_oid.to_ascii_lowercase();
        let commit = reader.read(&commit_oid, RawObjectType::Commit).await?;
        let links = verified_commit_links(&commit, hash_bytes * 2, control)
            .map_err(|_| MegaError::from(TracesError::ObjectIntegrity))?;
        let root = reader.read(&links.tree, RawObjectType::Tree).await?;
        let mut checkpoints = Vec::new();
        if let Some(checkpoint_root) = parser::checkpoint_root(&root, hash_bytes, control)? {
            let prefixes = reader.read(&checkpoint_root, RawObjectType::Tree).await?;
            for prefix in parser::checkpoint_children(&prefixes, hash_bytes, control)? {
                control.checkpoint().await?;
                if checkpoints.len() >= MAX_CHECKPOINTS {
                    return Err(TracesError::ResponseBudget.into());
                }
                if prefix.mode != RawTreeMode::Tree {
                    checkpoints.push(CheckpointParse::Invalid {
                        key: parser::raw_key(prefix.name, b""),
                        tree_oid: prefix.oid,
                        reason: InvalidCheckpoint::LayoutMode,
                    });
                    continue;
                }
                let leaves = reader.read(&prefix.oid, RawObjectType::Tree).await?;
                for leaf in parser::checkpoint_children(&leaves, hash_bytes, control)? {
                    control.checkpoint().await?;
                    if checkpoints.len() >= MAX_CHECKPOINTS {
                        return Err(TracesError::ResponseBudget.into());
                    }
                    let checkpoint =
                        match (leaf.mode, parser::checkpoint_id(prefix.name, leaf.name)) {
                            (RawTreeMode::Tree, Ok(checkpoint_id)) => {
                                reader
                                    .checkpoint(CheckpointLocation {
                                        checkpoint_id,
                                        tree_oid: leaf.oid,
                                    })
                                    .await?
                            }
                            (mode, id) => CheckpointParse::Invalid {
                                key: parser::raw_key(prefix.name, leaf.name),
                                tree_oid: leaf.oid,
                                reason: if mode == RawTreeMode::Tree {
                                    id.err().unwrap_or(InvalidCheckpoint::LayoutKey)
                                } else {
                                    InvalidCheckpoint::LayoutMode
                                },
                            },
                        };
                    checkpoints.push(checkpoint);
                }
            }
        }
        control.check()?;
        Ok(CommitCheckpoints {
            commit_oid,
            root_tree: links.tree,
            parents: links.parents,
            checkpoints,
        })
    }
}

struct StreamReader<'a, C> {
    objects: &'a TraceObjectService,
    db: &'a DatabaseConnection,
    stream_id: &'a str,
    hash_kind: &'a str,
    control: &'a C,
}

impl<C: RawReadControl> StreamReader<'_, C> {
    /// Read one visible member of this stream with the expected Git type.
    async fn read(&self, oid: &str, expected: RawObjectType) -> Result<Vec<u8>, MegaError> {
        let member = self
            .control
            .run(async {
                Ok(trace_object::Entity::find()
                    .filter(trace_object::Column::StreamId.eq(self.stream_id))
                    .filter(trace_object::Column::HashKind.eq(self.hash_kind))
                    .filter(trace_object::Column::Oid.eq(oid))
                    .filter(trace_object::Column::Visible.eq(true))
                    .one(self.db)
                    .await?)
            })
            .await?;
        if member.is_none() {
            return Err(TracesError::ObjectIntegrity.into());
        }
        let object = self
            .objects
            .read_verified_raw(self.stream_id, self.hash_kind, oid, self.control)
            .await?
            .ok_or(TracesError::ObjectIntegrity)?;
        if object.object_type() != expected {
            return Err(TracesError::ObjectIntegrity.into());
        }
        Ok(object.into_bytes())
    }

    async fn checkpoint(&self, location: CheckpointLocation) -> Result<CheckpointParse, MegaError> {
        let hash_bytes = if self.hash_kind == "sha256" { 32 } else { 20 };
        let tree = self.read(&location.tree_oid, RawObjectType::Tree).await?;
        let roles = match parser::checkpoint_roles(&tree, hash_bytes, self.control)? {
            Ok(roles) => roles,
            Err(reason) => {
                return Ok(CheckpointParse::Invalid {
                    key: location.checkpoint_id,
                    tree_oid: location.tree_oid,
                    reason,
                });
            }
        };
        let metadata = match &roles.metadata {
            Some(oid) => self.read(oid, RawObjectType::Blob).await?,
            None => Vec::new(),
        };
        let manifest = match &roles.manifest {
            Some(oid) => self.read(oid, RawObjectType::Blob).await?,
            None => Vec::new(),
        };
        Ok(parser::parse_checkpoint(
            location, &roles, &metadata, &manifest,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use git_internal::{hash::HashKind, internal::object::types::ObjectType};
    use sea_orm::{ActiveModelTrait, PaginatorTrait, Set};
    use uuid::Uuid;

    use super::*;
    use crate::{
        callisto::{trace_principal, trace_target},
        ceres::traces::{
            admission::IngestAdmission,
            ingest::decode_wire_pack,
            publication::tests::{Fixture, local_store},
        },
        jupiter::{
            migration::apply_migrations, service::trace_object_service::raw_object_oid,
            tests::test_db_connection,
        },
    };

    /// Real Libra traces pack fetched from the H1 TraceRepo after an
    /// `agent push --mega2` of three seeded checkpoints (two temporary, one
    /// committed). See `tests/data/traces/README.md`.
    const LIBRA_H2_PACK: &[u8] =
        include_bytes!("../../../tests/data/traces/libra-h2-three-checkpoints.pack");
    const LIBRA_H2_TIP: &str = "d9bf5910abfdcd70f6ba5ec0ec1c69ba453075f7";

    fn budget() -> ProjectionControl {
        ProjectionControl::new(Duration::from_secs(30))
    }

    async fn database() -> (tempfile::TempDir, DatabaseConnection) {
        let directory = tempfile::tempdir().unwrap();
        let db = test_db_connection(directory.path()).await;
        apply_migrations(&db, true).await.unwrap();
        (directory, db)
    }

    async fn stream(db: &DatabaseConnection, hash_kind: &str) -> String {
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
        trace_stream::ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            principal_id: Set(principal.id),
            target_id: Set(target_id),
            source_repo_id: Set("libra-h2-source".to_owned()),
            epoch_id: Set(Uuid::new_v4().to_string()),
            hash_kind: Set(hash_kind.to_owned()),
            tip_oid: Set(None),
            created_at: Set(now),
        }
        .insert(db)
        .await
        .unwrap()
        .id
    }

    /// Store raw objects and their visible membership for `stream_id`.
    async fn archive(
        db: &DatabaseConnection,
        objects: &TraceObjectService,
        stream_id: &str,
        hash_kind: &str,
        entries: &[(String, Vec<u8>)],
    ) {
        for (oid, bytes) in entries {
            objects
                .put_raw(stream_id, hash_kind, oid, bytes)
                .await
                .unwrap();
            trace_object::ActiveModel {
                id: Set(Uuid::new_v4().to_string()),
                stream_id: Set(stream_id.to_owned()),
                hash_kind: Set(hash_kind.to_owned()),
                oid: Set(oid.clone()),
                visible: Set(true),
                created_at: Set(Utc::now().fixed_offset()),
            }
            .insert(db)
            .await
            .unwrap();
        }
    }

    async fn libra_objects() -> Vec<(String, Vec<u8>)> {
        let decoded =
            decode_wire_pack(&mut IngestAdmission::start(), HashKind::Sha1, LIBRA_H2_PACK)
                .await
                .unwrap();
        let mut out = Vec::new();
        for (kind, payload) in decoded.entries() {
            let raw_type = match kind {
                ObjectType::Commit => RawObjectType::Commit,
                ObjectType::Tree => RawObjectType::Tree,
                ObjectType::Blob => RawObjectType::Blob,
                ObjectType::Tag => RawObjectType::Tag,
                other => panic!("unexpected {other:?}"),
            };
            let oid = raw_object_oid("sha1", raw_type, payload, &budget())
                .await
                .unwrap();
            out.push((oid, payload.to_vec()));
        }
        out
    }

    #[tokio::test]
    async fn checkpoint_parse_projection_real_libra_history() {
        let (_db_dir, db) = database().await;
        let (_store_dir, store) = local_store().await;
        let objects = TraceObjectService::new(store);
        let stream_id = stream(&db, "sha1").await;
        let entries = libra_objects().await;
        assert_eq!(entries.len(), 33, "the fixture pack holds the full closure");
        archive(&db, &objects, &stream_id, "sha1", &entries).await;
        let service = TraceProjectionService::new(objects);

        let tip = service
            .checkpoints_at_commit(&db, &stream_id, LIBRA_H2_TIP, &budget())
            .await
            .unwrap();
        assert_eq!(tip.root_tree, "1282df59d3908488a1dfc2e9d9621b4539b6e4b6");
        assert_eq!(tip.parents, ["ad49fd98a85bcbb30d754b4d2ed63ca4061753a8"]);
        let parsed: BTreeMap<_, _> = tip
            .checkpoints
            .iter()
            .map(|checkpoint| match checkpoint {
                CheckpointParse::Parsed(parsed) => (
                    parsed.location.checkpoint_id.clone(),
                    (
                        parsed.location.tree_oid.clone(),
                        parsed.metadata_oid.clone(),
                        parsed.manifest_oid.clone(),
                    ),
                ),
                other => panic!("real Libra checkpoint did not parse: {other:?}"),
            })
            .collect();
        assert_eq!(
            parsed.keys().cloned().collect::<Vec<_>>(),
            [
                "1c6cc65e-f8bd-409a-8f0e-69fda086a5d0",
                "b09030ec-0419-4fd9-861e-0252c2ae7a00",
                "d7cf0133-e2f6-425b-81e7-d44dc0540e02",
            ]
        );
        let committed = &parsed["1c6cc65e-f8bd-409a-8f0e-69fda086a5d0"];
        assert_eq!(committed.1, "00f0558c1153658d99dd0068a7c3146d7caeb871");
        assert_eq!(committed.2, "99e59a9a157f71fc5ed9ef46a0836da18df113fb");
        // The checkpoint tree OID is the raw `checkpoint/1c/<rest>` entry.
        let name = b"40000 6cc65e-f8bd-409a-8f0e-69fda086a5d0\0";
        let prefix_tree = entries
            .iter()
            .find(|(_, bytes)| bytes.windows(name.len()).any(|window| window == name))
            .expect("prefix tree");
        let raw_entry_oid = {
            let at = prefix_tree
                .1
                .windows(name.len())
                .position(|window| window == name)
                .unwrap()
                + name.len();
            hex::encode(&prefix_tree.1[at..at + 20])
        };
        assert_eq!(committed.0, raw_entry_oid);

        let parent = service
            .checkpoints_at_commit(&db, &stream_id, &tip.parents[0], &budget())
            .await
            .unwrap();
        assert_eq!(
            parent.checkpoints.len(),
            2,
            "each Libra commit carries all checkpoints so far"
        );
        assert!(
            parent
                .checkpoints
                .iter()
                .all(|checkpoint| matches!(checkpoint, CheckpointParse::Parsed(_)))
        );
    }

    #[tokio::test]
    async fn checkpoint_parse_projection_invalid_keeps_archive() {
        let (_db_dir, db) = database().await;
        let (_store_dir, store) = local_store().await;
        let objects = TraceObjectService::new(store);
        for kind in [HashKind::Sha1, HashKind::Sha256] {
            let mut fixture = Fixture::new(kind);
            let hash_kind = fixture.hash_kind();
            let good_metadata = fixture.object("blob", br#"{"checkpoint_id":"abcd"}"#);
            let good_manifest = fixture.object("blob", br#"{"checkpoint_id":"abcd"}"#);
            let good = fixture.tree(&[
                ("100644", b"manifest.json", &good_manifest),
                ("100644", b"metadata.json", &good_metadata),
            ]);
            let not_json = fixture.object("blob", b"{not json");
            let broken = fixture.tree(&[
                ("100644", b"manifest.json", &good_manifest),
                ("100644", b"metadata.json", &not_json),
            ]);
            let other = fixture.object("blob", br#"{"checkpoint_id":"zzzz"}"#);
            let mismatch = fixture.tree(&[
                ("100644", b"manifest.json", &other),
                ("100644", b"metadata.json", &good_metadata),
            ]);
            let no_manifest = fixture.tree(&[("100644", b"metadata.json", &good_metadata)]);
            let stray = fixture.object("blob", b"stray");
            let prefix_ab = fixture.tree(&[
                ("40000", b"cd", &good),
                ("40000", b"ef", &broken),
                ("40000", b"gh", &mismatch),
                ("40000", b"ij", &no_manifest),
                ("100644", b"kl", &stray),
            ]);
            let prefix_bad = fixture.tree(&[("40000", b"x", &good)]);
            let checkpoints = fixture.tree(&[
                ("40000", b"ab", &prefix_ab),
                ("40000", b"abc", &prefix_bad),
                ("100644", b"zz", &stray),
            ]);
            let root = fixture.tree(&[("40000", b"checkpoint", &checkpoints)]);
            let commit = fixture.commit(&root, None);
            let stream_id = stream(&db, hash_kind).await;
            let entries: Vec<_> = fixture
                .objects
                .iter()
                .map(|(oid, _, bytes)| (oid.clone(), bytes.clone()))
                .collect();
            archive(&db, &objects, &stream_id, hash_kind, &entries).await;
            let service = TraceProjectionService::new(objects.clone());
            let result = service
                .checkpoints_at_commit(&db, &stream_id, &commit, &budget())
                .await
                .unwrap();
            let summary: Vec<_> = result
                .checkpoints
                .iter()
                .map(|checkpoint| match checkpoint {
                    CheckpointParse::Parsed(parsed) => {
                        (parsed.location.checkpoint_id.clone(), "parsed")
                    }
                    CheckpointParse::Invalid { key, reason, .. } => (key.clone(), reason.as_str()),
                })
                .collect();
            assert_eq!(
                summary,
                [
                    ("abcd".to_owned(), "parsed"),
                    ("abef".to_owned(), "metadata_not_object"),
                    ("abgh".to_owned(), "id_mismatch"),
                    ("abij".to_owned(), "missing_manifest"),
                    ("ab/kl".to_owned(), "layout_mode"),
                    ("abc/x".to_owned(), "layout_key"),
                    ("zz/".to_owned(), "layout_mode"),
                ],
                "{hash_kind}"
            );
            // Invalid captures leave every archived object readable.
            for (oid, bytes) in &entries {
                assert_eq!(
                    objects
                        .get_raw(&stream_id, hash_kind, oid)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(bytes.as_slice())
                );
            }
            assert_eq!(
                trace_object::Entity::find()
                    .filter(trace_object::Column::StreamId.eq(&stream_id))
                    .filter(trace_object::Column::Visible.eq(true))
                    .count(&db)
                    .await
                    .unwrap(),
                entries.len() as u64
            );

            // A commit outside the stream membership is an integrity error.
            let foreign = stream(&db, hash_kind).await;
            assert!(matches!(
                service
                    .checkpoints_at_commit(&db, &foreign, &commit, &budget())
                    .await,
                Err(MegaError::Traces(TracesError::ObjectIntegrity))
            ));
            // A tree OID passed as a commit is rejected by type.
            assert!(matches!(
                service
                    .checkpoints_at_commit(&db, &stream_id, &root, &budget())
                    .await,
                Err(MegaError::Traces(TracesError::ObjectIntegrity))
            ));
        }
        assert!(matches!(
            TraceProjectionService::new(objects)
                .checkpoints_at_commit(&db, &Uuid::new_v4().to_string(), LIBRA_H2_TIP, &budget())
                .await,
            Err(MegaError::Traces(TracesError::UnknownScope))
        ));
    }

    #[tokio::test]
    async fn checkpoint_parse_projection_without_checkpoints_and_deadline() {
        let (_db_dir, db) = database().await;
        let (_store_dir, store) = local_store().await;
        let objects = TraceObjectService::new(store);
        let mut fixture = Fixture::new(HashKind::Sha1);
        let blob = fixture.object("blob", b"plain");
        let root = fixture.tree(&[("100644", b"checkpoint", &blob)]);
        let commit = fixture.commit(&root, None);
        let stream_id = stream(&db, "sha1").await;
        let entries: Vec<_> = fixture
            .objects
            .iter()
            .map(|(oid, _, bytes)| (oid.clone(), bytes.clone()))
            .collect();
        archive(&db, &objects, &stream_id, "sha1", &entries).await;
        let service = TraceProjectionService::new(objects);
        let result = service
            .checkpoints_at_commit(&db, &stream_id, &commit, &budget())
            .await
            .unwrap();
        assert!(
            result.checkpoints.is_empty(),
            "a blob named checkpoint is not a root"
        );
        assert!(matches!(
            service
                .checkpoints_at_commit(
                    &db,
                    &stream_id,
                    &commit,
                    &ProjectionControl::new(Duration::ZERO)
                )
                .await,
            Err(MegaError::Traces(TracesError::QueryTimeout))
        ));
    }
}
