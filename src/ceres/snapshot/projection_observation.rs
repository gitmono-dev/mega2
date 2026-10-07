//! Operation-local resolve diagnostics, not publication or retention receipts.

use std::time::Duration;

use git_internal::hash::ObjectHash;
use mst2_codec::descriptor::{
    ACCESS_PROJECTION_EXACT_FULL, FS_SEMANTICS_LINUX_CODE_V1, MATERIALIZATION_POLICY_GIT_RAW_V1,
    METADATA_CODEC, SCHEMA_VERSION, ServingDescriptor,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    ceres::snapshot::error::{SnapshotError, SnapshotErrorCode},
    jupiter::storage::mono_storage::MST2_VERIFICATION_VERSION,
};

pub(crate) const NATIVE_PROJECTION_REVISION: u16 = 1;

/// Work in one directory projection. Page counters cover returned directory
/// roots, excluding codec-internal radix encoding and later route traversal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ProjectionWork {
    pub directories_rebuilt: u64,
    /// Cache-hit boundary visits, not all descendants or unique source OIDs.
    pub reused_subtree_roots: u64,
    pub directory_root_pages_built: u64,
    pub directory_root_page_bytes_built: u64,
    pub directory_root_pages_reused: u64,
    pub directory_root_page_bytes_reused: u64,
    pub directory_entries_scanned: u64,
    pub scope_path_entries_examined: u64,
    /// Backend tree requests, excluding the caller-supplied fixed root tree.
    pub tree_fetches: u64,
    /// Verification-record outcomes per file entry, not unique body OIDs.
    pub verified_blob_hits: u64,
    pub verified_blob_misses: u64,
    /// Returned raw-body lengths, not physical disk/network traffic.
    pub raw_bytes_fetched: u64,
    /// Bytes passed to this builder's explicit raw-file SHA-256.
    pub raw_bytes_hashed: u64,
}

/// Captured only from the validated native head, before resolve projects it.
pub(crate) struct NativeResolveSource {
    instance: Uuid,
    commit: ObjectHash,
    tree: ObjectHash,
    certificate_receipt_id: u64,
    writer_epoch: u64,
    publication_sequence: u64,
}

pub(crate) struct ResolvedProjection<'a> {
    pub descriptor: &'a ServingDescriptor,
    pub snapshot_id: &'a str,
    pub metadata_root: &'a str,
    pub context_commit: &'a str,
    pub context_root_tree: &'a str,
    pub fixed_root_tree: ObjectHash,
    pub requested_scope: &'a str,
    pub request_id: &'a str,
}

/// Immutable successful resolve observation. This grants no authorization,
/// retention, durable publication or proof of codec-internal construction work.
pub(crate) struct NativeProjectionObservation {
    source: NativeResolveSource,
    scope: String,
    namespace_view_id: String,
    snapshot_id: String,
    metadata_root: String,
    request_id: String,
    projection_elapsed_micros: u64,
    work: ProjectionWork,
}

/// Closed wire fields, borrowed only from the already validated observation.
#[derive(Serialize)]
pub(crate) struct ProjectionWireRecord<'a> {
    observation_revision: u16,
    phase: &'static str,
    source_domain: &'static str,
    request_id: &'a str,
    instance_id: String,
    #[serde(serialize_with = "tagged_oid")]
    root_commit_oid: ObjectHash,
    #[serde(serialize_with = "tagged_oid")]
    root_tree_oid: ObjectHash,
    native_certificate_receipt_id: u64,
    native_writer_epoch: u64,
    native_publication_sequence: u64,
    scope: &'a str,
    schema_version: u16,
    metadata_codec: u16,
    materialization_policy: u16,
    fs_semantics: u16,
    access_projection: u16,
    verification_revision: i32,
    projection_revision: u16,
    namespace_view_id: &'a str,
    snapshot_id: &'a str,
    metadata_root: &'a str,
    projection_elapsed_micros: u64,
    page_counter_scope: &'static str,
    codec_radix_work: &'static str,
    #[serde(flatten)]
    work: &'a ProjectionWork,
    message: &'static str,
}

fn tagged_oid<S: serde::Serializer>(oid: &ObjectHash, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&oid.to_tagged_string())
}

fn invalid() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::IntegrityError,
        "native resolve observation does not match its fixed source and context",
    )
}

impl NativeResolveSource {
    pub(crate) fn capture(
        instance_id: &str,
        commit: ObjectHash,
        tree: ObjectHash,
        certificate_receipt_id: Option<i64>,
        writer_epoch: i64,
        publication_sequence: i64,
    ) -> Result<Self, SnapshotError> {
        let instance = Uuid::parse_str(instance_id).map_err(|_| invalid())?;
        let positive = |value: i64| {
            u64::try_from(value)
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(invalid)
        };
        if instance.to_string() != instance_id || commit.kind() != tree.kind() {
            return Err(invalid());
        }
        Ok(Self {
            instance,
            commit,
            tree,
            certificate_receipt_id: positive(certificate_receipt_id.ok_or_else(invalid)?)?,
            writer_epoch: positive(writer_epoch)?,
            publication_sequence: positive(publication_sequence)?,
        })
    }

    pub(crate) fn observe(
        self,
        resolved: ResolvedProjection<'_>,
        work: ProjectionWork,
        projection_elapsed: Duration,
    ) -> Result<NativeProjectionObservation, SnapshotError> {
        let mut view_hash = Sha256::new();
        view_hash.update(b"mega.mst2.namespaceview\0");
        view_hash.update(self.commit.to_string().as_bytes());
        let expected_view: [u8; 32] = view_hash.finalize().into();
        let descriptor = resolved.descriptor;
        let digest_text = |bytes: &[u8]| format!("sha256:{}", hex::encode(bytes));
        let snapshot = descriptor.snapshot_id().map_err(|_| invalid())?;
        let bounded_request_id = !resolved.request_id.is_empty()
            && resolved.request_id.len() <= 128
            && resolved.request_id.bytes().all(|byte| {
                matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b':' | b'/')
            });
        if resolved.context_commit != self.commit.to_string()
            || resolved.context_root_tree != self.tree.to_string()
            || resolved.fixed_root_tree != self.tree
            || descriptor.instance_uuid != *self.instance.as_bytes()
            || descriptor.namespace_view_id != expected_view
            || descriptor.scope != resolved.requested_scope
            || resolved.snapshot_id != digest_text(&snapshot)
            || resolved.metadata_root != digest_text(&descriptor.metadata_root)
            || !bounded_request_id
        {
            return Err(invalid());
        }
        Ok(NativeProjectionObservation {
            source: self,
            scope: descriptor.scope.clone(),
            namespace_view_id: digest_text(&descriptor.namespace_view_id),
            snapshot_id: resolved.snapshot_id.to_owned(),
            metadata_root: resolved.metadata_root.to_owned(),
            request_id: resolved.request_id.to_owned(),
            projection_elapsed_micros: u64::try_from(projection_elapsed.as_micros())
                .map_err(|_| invalid())?,
            work,
        })
    }
}

impl NativeProjectionObservation {
    pub(crate) fn wire_record(&self) -> ProjectionWireRecord<'_> {
        ProjectionWireRecord {
            observation_revision: 1,
            phase: "resolve_directory_projection",
            source_domain: "native-git",
            request_id: &self.request_id,
            instance_id: self.source.instance.to_string(),
            root_commit_oid: self.source.commit,
            root_tree_oid: self.source.tree,
            native_certificate_receipt_id: self.source.certificate_receipt_id,
            native_writer_epoch: self.source.writer_epoch,
            native_publication_sequence: self.source.publication_sequence,
            scope: &self.scope,
            schema_version: SCHEMA_VERSION,
            metadata_codec: METADATA_CODEC,
            materialization_policy: MATERIALIZATION_POLICY_GIT_RAW_V1,
            fs_semantics: FS_SEMANTICS_LINUX_CODE_V1,
            access_projection: ACCESS_PROJECTION_EXACT_FULL,
            verification_revision: MST2_VERIFICATION_VERSION,
            projection_revision: NATIVE_PROJECTION_REVISION,
            namespace_view_id: &self.namespace_view_id,
            snapshot_id: &self.snapshot_id,
            metadata_root: &self.metadata_root,
            projection_elapsed_micros: self.projection_elapsed_micros,
            page_counter_scope: "returned-directory-root-pages",
            codec_radix_work: "NOT_EXPOSED",
            work: &self.work,
            message: "native resolve directory projection succeeded",
        }
    }

    pub(crate) fn emit(self) {
        tracing::debug!(
            target: "mst2::native_projection_observation",
            observation_revision = 1u16,
            phase = "resolve_directory_projection",
            source_domain = "native-git",
            request_id = %self.request_id,
            instance_id = %self.source.instance,
            root_commit_oid = %self.source.commit.to_tagged_string(),
            root_tree_oid = %self.source.tree.to_tagged_string(),
            native_certificate_receipt_id = self.source.certificate_receipt_id,
            native_writer_epoch = self.source.writer_epoch,
            native_publication_sequence = self.source.publication_sequence,
            scope = ?self.scope,
            schema_version = SCHEMA_VERSION,
            metadata_codec = METADATA_CODEC,
            materialization_policy = MATERIALIZATION_POLICY_GIT_RAW_V1,
            fs_semantics = FS_SEMANTICS_LINUX_CODE_V1,
            access_projection = ACCESS_PROJECTION_EXACT_FULL,
            verification_revision = MST2_VERIFICATION_VERSION,
            projection_revision = NATIVE_PROJECTION_REVISION,
            namespace_view_id = %self.namespace_view_id,
            snapshot_id = %self.snapshot_id,
            metadata_root = %self.metadata_root,
            projection_elapsed_micros = self.projection_elapsed_micros,
            page_counter_scope = "returned-directory-root-pages",
            codec_radix_work = "NOT_EXPOSED",
            directories_rebuilt = self.work.directories_rebuilt,
            reused_subtree_roots = self.work.reused_subtree_roots,
            directory_root_pages_built = self.work.directory_root_pages_built,
            directory_root_page_bytes_built = self.work.directory_root_page_bytes_built,
            directory_root_pages_reused = self.work.directory_root_pages_reused,
            directory_root_page_bytes_reused = self.work.directory_root_page_bytes_reused,
            directory_entries_scanned = self.work.directory_entries_scanned,
            scope_path_entries_examined = self.work.scope_path_entries_examined,
            tree_fetches = self.work.tree_fetches,
            verified_blob_hits = self.work.verified_blob_hits,
            verified_blob_misses = self.work.verified_blob_misses,
            raw_bytes_fetched = self.work.raw_bytes_fetched,
            raw_bytes_hashed = self.work.raw_bytes_hashed,
            "native resolve directory projection succeeded"
        );
        #[cfg(test)]
        let _ = OBSERVATIONS.try_with(|observations| observations.lock().unwrap().push(self));
    }
}

#[cfg(test)]
tokio::task_local! {
    static OBSERVATIONS: std::sync::Arc<std::sync::Mutex<Vec<NativeProjectionObservation>>>;
}

#[cfg(test)]
pub(crate) async fn with_observations<F: std::future::Future>(
    future: F,
) -> (F::Output, Vec<NativeProjectionObservation>) {
    let observations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let result = OBSERVATIONS.scope(observations.clone(), future).await;
    let records = std::mem::take(&mut *observations.lock().unwrap());
    (result, records)
}

#[cfg(test)]
impl NativeProjectionObservation {
    pub(crate) fn test_identity(&self) -> serde_json::Value {
        serde_json::json!({
            "instance_id": self.source.instance.to_string(),
            "root_commit_oid": self.source.commit.to_tagged_string(),
            "root_tree_oid": self.source.tree.to_tagged_string(),
            "native_certificate_receipt_id": self.source.certificate_receipt_id,
            "native_writer_epoch": self.source.writer_epoch.to_string(),
            "native_publication_sequence": self.source.publication_sequence.to_string(),
            "scope": self.scope,
            "namespace_view_id": self.namespace_view_id,
            "snapshot_id": self.snapshot_id,
            "metadata_root": self.metadata_root,
            "request_id": self.request_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use git_internal::hash::HashKind;
    use tracing::{
        Event, Metadata, Subscriber,
        field::{Field, Visit},
        span::{Attributes, Id, Record},
    };

    use super::*;

    const INSTANCE: &str = "11111111-2222-4333-8444-555555555559";

    struct Fixture {
        commit: ObjectHash,
        tree: ObjectHash,
        descriptor: ServingDescriptor,
        snapshot_id: String,
        metadata_root: String,
    }

    impl Fixture {
        fn new() -> Self {
            let commit = ObjectHash::from_hex_for_kind(HashKind::Sha256, &"a".repeat(64)).unwrap();
            let tree = ObjectHash::from_hex_for_kind(HashKind::Sha256, &"b".repeat(64)).unwrap();
            let mut view_hash = Sha256::new();
            view_hash.update(b"mega.mst2.namespaceview\0");
            view_hash.update(commit.to_string().as_bytes());
            let descriptor = ServingDescriptor {
                instance_uuid: *Uuid::parse_str(INSTANCE).unwrap().as_bytes(),
                namespace_view_id: view_hash.finalize().into(),
                scope: "/project".to_owned(),
                metadata_root: [0xc; 32],
            };
            Self {
                commit,
                tree,
                snapshot_id: format!("sha256:{}", hex::encode(descriptor.snapshot_id().unwrap())),
                metadata_root: format!("sha256:{}", hex::encode(descriptor.metadata_root)),
                descriptor,
            }
        }

        fn source(&self) -> NativeResolveSource {
            NativeResolveSource::capture(INSTANCE, self.commit, self.tree, Some(97), 3, 7).unwrap()
        }

        fn resolved<'a>(&'a self, commit: &'a str, tree: &'a str) -> ResolvedProjection<'a> {
            ResolvedProjection {
                descriptor: &self.descriptor,
                snapshot_id: &self.snapshot_id,
                metadata_root: &self.metadata_root,
                context_commit: commit,
                context_root_tree: tree,
                fixed_root_tree: self.tree,
                requested_scope: "/project",
                request_id: "resolve:request-1",
            }
        }
    }

    #[test]
    fn observation_binds_fixed_source_context_descriptor_and_operation_work() {
        let fixture = Fixture::new();
        let commit = fixture.commit.to_string();
        let tree = fixture.tree.to_string();
        let work = ProjectionWork {
            directories_rebuilt: 3,
            reused_subtree_roots: 70,
            directory_entries_scanned: 104,
            verified_blob_misses: 1,
            ..ProjectionWork::default()
        };
        let observation = fixture
            .source()
            .observe(
                fixture.resolved(&commit, &tree),
                work.clone(),
                Duration::from_micros(123),
            )
            .unwrap();
        assert_eq!(observation.source.certificate_receipt_id, 97);
        assert_eq!(observation.source.publication_sequence, 7);
        assert_eq!(observation.source.writer_epoch, 3);
        assert_eq!(
            observation.source.tree.to_tagged_string(),
            format!("sha256:{tree}")
        );
        assert_eq!(observation.projection_elapsed_micros, 123);
        assert_eq!(observation.work, work);
        assert_eq!(observation.scope, "/project");
        assert_eq!(observation.snapshot_id, fixture.snapshot_id);
    }

    #[test]
    fn observation_rejects_each_changed_binding_or_unbounded_trace_token() {
        let fixture = Fixture::new();
        let commit = fixture.commit.to_string();
        let tree = fixture.tree.to_string();
        for mode in 0..8 {
            let mut descriptor = fixture.descriptor.clone();
            let mut resolved = fixture.resolved(&commit, &tree);
            match mode {
                0 => resolved.context_commit = &tree,
                1 => resolved.context_root_tree = &commit,
                2 => resolved.fixed_root_tree = fixture.commit,
                3 => descriptor.instance_uuid[0] ^= 1,
                4 => descriptor.namespace_view_id[0] ^= 1,
                5 => resolved.requested_scope = "/different",
                6 => resolved.snapshot_id = &fixture.metadata_root,
                7 => resolved.metadata_root = &fixture.snapshot_id,
                _ => unreachable!(),
            }
            resolved.descriptor = &descriptor;
            assert!(
                fixture
                    .source()
                    .observe(resolved, ProjectionWork::default(), Duration::ZERO)
                    .is_err(),
                "mode {mode}"
            );
        }
        for request_id in [
            "".to_owned(),
            "x".repeat(129),
            "bad\ntrace".to_owned(),
            "actor secret".to_owned(),
        ] {
            let mut resolved = fixture.resolved(&commit, &tree);
            resolved.request_id = &request_id;
            assert!(
                fixture
                    .source()
                    .observe(resolved, ProjectionWork::default(), Duration::ZERO)
                    .is_err()
            );
        }
    }

    #[test]
    fn capture_requires_positive_native_certificate_and_preserves_hash_kind() {
        let fixture = Fixture::new();
        for (certificate, epoch, sequence) in [
            (None, 1, 1),
            (Some(0), 1, 1),
            (Some(-1), 1, 1),
            (Some(1), 0, 1),
            (Some(1), 1, 0),
        ] {
            assert!(
                NativeResolveSource::capture(
                    INSTANCE,
                    fixture.commit,
                    fixture.tree,
                    certificate,
                    epoch,
                    sequence
                )
                .is_err()
            );
        }
        let blake_tree =
            ObjectHash::from_hex_for_kind(HashKind::Blake3, &fixture.tree.to_string()).unwrap();
        assert!(
            NativeResolveSource::capture(INSTANCE, fixture.commit, blake_tree, Some(1), 1, 1)
                .is_err()
        );
    }

    struct TraceCapture(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

    impl Subscriber for TraceCapture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            if event.metadata().target() != "mst2::native_projection_observation" {
                return;
            }
            struct Fields(BTreeMap<String, String>);
            impl Visit for Fields {
                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    self.0.insert(field.name().to_owned(), format!("{value:?}"));
                }
                fn record_u64(&mut self, field: &Field, value: u64) {
                    self.0.insert(field.name().to_owned(), value.to_string());
                }
                fn record_str(&mut self, field: &Field, value: &str) {
                    self.0.insert(field.name().to_owned(), value.to_owned());
                }
            }
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }

    #[test]
    fn actual_trace_has_closed_identity_profile_counter_fields_and_no_body_or_credentials() {
        let fixture = Fixture::new();
        let commit = fixture.commit.to_string();
        let tree = fixture.tree.to_string();
        let observation = fixture
            .source()
            .observe(
                fixture.resolved(&commit, &tree),
                ProjectionWork::default(),
                Duration::from_micros(4),
            )
            .unwrap();
        let wire = serde_json::to_value(observation.wire_record()).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::with_default(TraceCapture(events.clone()), || observation.emit());
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let fields = &events[0];
        let expected = [
            "observation_revision",
            "phase",
            "source_domain",
            "request_id",
            "instance_id",
            "root_commit_oid",
            "root_tree_oid",
            "native_certificate_receipt_id",
            "native_writer_epoch",
            "native_publication_sequence",
            "scope",
            "schema_version",
            "metadata_codec",
            "materialization_policy",
            "fs_semantics",
            "access_projection",
            "verification_revision",
            "projection_revision",
            "namespace_view_id",
            "snapshot_id",
            "metadata_root",
            "projection_elapsed_micros",
            "page_counter_scope",
            "codec_radix_work",
            "directories_rebuilt",
            "reused_subtree_roots",
            "directory_root_pages_built",
            "directory_root_page_bytes_built",
            "directory_root_pages_reused",
            "directory_root_page_bytes_reused",
            "directory_entries_scanned",
            "scope_path_entries_examined",
            "tree_fetches",
            "verified_blob_hits",
            "verified_blob_misses",
            "raw_bytes_fetched",
            "raw_bytes_hashed",
            "message",
        ];
        assert_eq!(fields.len(), expected.len());
        assert!(expected.iter().all(|field| fields.contains_key(*field)));
        let wire = wire.as_object().unwrap();
        assert_eq!(wire.len(), expected.len());
        for key in expected {
            let typed = &wire[key];
            let expected = if key == "scope" {
                typed.to_string()
            } else if let Some(text) = typed.as_str() {
                text.into()
            } else {
                typed.to_string()
            };
            assert_eq!(fields[key], expected, "typed observation diverged at {key}");
        }
        assert_eq!(fields["phase"], "resolve_directory_projection");
        assert_eq!(
            fields["page_counter_scope"],
            "returned-directory-root-pages"
        );
        assert_eq!(fields["codec_radix_work"], "NOT_EXPOSED");
        assert_eq!(fields["projection_elapsed_micros"], "4");
        assert_eq!(fields["native_certificate_receipt_id"], "97");
        assert_eq!(fields["scope"], "\"/project\"");
        assert_eq!(fields["root_tree_oid"], fixture.tree.to_tagged_string());
    }

    #[tokio::test]
    async fn emitted_observations_are_operation_local_with_no_publication_aggregation() {
        let collect = |request: &'static str, sequence: i64| async move {
            let fixture = Fixture::new();
            let commit = fixture.commit.to_string();
            let tree = fixture.tree.to_string();
            let mut resolved = fixture.resolved(&commit, &tree);
            resolved.request_id = request;
            let source = NativeResolveSource::capture(
                INSTANCE,
                fixture.commit,
                fixture.tree,
                Some(97),
                3,
                sequence,
            )
            .unwrap();
            source
                .observe(resolved, ProjectionWork::default(), Duration::ZERO)
                .unwrap()
                .emit();
        };
        let (left, right) = tokio::join!(
            with_observations(collect("request-left", 7)),
            with_observations(collect("request-right", 8))
        );
        assert_eq!(left.1.len(), 1);
        assert_eq!(right.1.len(), 1);
        assert_eq!(left.1[0].request_id, "request-left");
        assert_eq!(right.1[0].request_id, "request-right");
        assert_eq!(left.1[0].source.publication_sequence, 7);
        assert_eq!(right.1[0].source.publication_sequence, 8);
    }
}
