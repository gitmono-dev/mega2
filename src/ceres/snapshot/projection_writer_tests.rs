use git_internal::hash::{HashKind, ObjectHash};
use mst2_codec::descriptor::ServingDescriptor;

use super::*;
use crate::ceres::snapshot::projection_observation::{
    NativeResolveSource, ProjectionWork, ResolvedProjection,
};

fn observation(scope: &str) -> NativeProjectionObservation {
    let commit = ObjectHash::from_hex_for_kind(HashKind::Sha256, &"a".repeat(64)).unwrap();
    let tree = ObjectHash::from_hex_for_kind(HashKind::Sha256, &"b".repeat(64)).unwrap();
    let instance = Uuid::new_v4();
    let mut namespace = Sha256::new();
    namespace.update(b"mega.mst2.namespaceview\0");
    namespace.update(commit.to_string().as_bytes());
    let descriptor = ServingDescriptor {
        instance_uuid: *instance.as_bytes(),
        namespace_view_id: namespace.finalize().into(),
        scope: scope.into(),
        metadata_root: [3; 32],
    };
    let snapshot = format!("sha256:{}", hex::encode(descriptor.snapshot_id().unwrap()));
    let root = format!("sha256:{}", hex::encode(descriptor.metadata_root));
    NativeResolveSource::capture(&instance.to_string(), commit, tree, Some(1), 2, 3)
        .unwrap()
        .observe(
            ResolvedProjection {
                descriptor: &descriptor,
                snapshot_id: &snapshot,
                metadata_root: &root,
                context_commit: &commit.to_string(),
                context_root_tree: &tree.to_string(),
                fixed_root_tree: tree,
                requested_scope: scope,
                request_id: "writer:test:a1",
            },
            ProjectionWork::default(),
            Duration::from_micros(4),
        )
        .unwrap()
}

fn idle_sink() -> (ProjectionObservationSink, Receiver<QueuedRecord>) {
    let (sender, receiver) = mpsc::sync_channel(RECORD_LIMIT);
    let sink = ProjectionObservationSink {
        id: Uuid::new_v4(),
        producer: Mutex::new(Producer {
            sender: Some(sender),
        }),
        pool: Arc::new(Mutex::new(
            (0..RECORD_LIMIT)
                .map(|_| RecordBuffer {
                    bytes: Box::new([0; RECORD_BYTES]),
                    len: 0,
                })
                .collect(),
        )),
        health: Arc::new(Health {
            first_error: AtomicU8::new(0),
            accepted: AtomicU64::new(0),
        }),
        worker: Mutex::new(None),
    };
    (sink, receiver)
}

#[test]
fn bounded_slots_precede_serialization_and_saturation_is_sticky_with_no_extra_allocation() {
    let (sink, receiver) = idle_sink();
    let observation = observation("/project");
    for _ in 0..RECORD_LIMIT {
        sink.enqueue(&observation).unwrap();
    }
    assert_eq!(sink.pool.lock().unwrap().len(), 0);
    assert_eq!(
        sink.health.accepted.load(Ordering::SeqCst),
        RECORD_LIMIT as u64
    );
    assert_eq!(
        sink.enqueue(&observation),
        Err(WriterFailure::RecordLimitExceeded)
    );
    assert_eq!(
        sink.health.first_error.load(Ordering::SeqCst),
        WriterFailure::RecordLimitExceeded as u8
    );
    let record = receiver.try_recv().unwrap();
    sink.return_slot(record.buffer);
    assert_eq!(
        sink.enqueue(&observation),
        Err(WriterFailure::WriterUnavailable)
    );
    assert_eq!(sink.pool.lock().unwrap().len(), 1);
}

#[test]
fn pool_saturation_and_disconnection_have_typed_failure_and_return_the_actual_slot() {
    let (sink, receiver) = idle_sink();
    let observation = observation("/project");
    let held = sink.pool.lock().unwrap().drain(..).collect::<Vec<_>>();
    assert_eq!(
        sink.enqueue(&observation),
        Err(WriterFailure::QueueSaturated)
    );
    assert_eq!(held.len(), RECORD_LIMIT);
    assert_eq!(sink.health.accepted.load(Ordering::SeqCst), 0);
    let (sink, receiver2) = idle_sink();
    drop(receiver2);
    assert_eq!(
        sink.enqueue(&observation),
        Err(WriterFailure::WriterUnavailable)
    );
    assert_eq!(sink.pool.lock().unwrap().len(), RECORD_LIMIT);
    assert_eq!(sink.health.accepted.load(Ordering::SeqCst), 0);
    drop(receiver);
}

#[test]
fn typed_wire_contract_has_exact_38_fields_and_rejects_oversize_without_growing_buffer() {
    let (sink, receiver) = idle_sink();
    let captured = observation("/project\"quoted");
    sink.enqueue(&captured).unwrap();
    let record = receiver.try_recv().unwrap();
    assert_eq!(record.buffer.bytes.len(), RECORD_BYTES);
    let envelope: serde_json::Value =
        serde_json::from_slice(&record.buffer.bytes[..record.buffer.len]).unwrap();
    let payload = &envelope["payload"];
    assert_eq!(payload.as_object().unwrap().len(), 38);
    assert_eq!(payload["scope"], "/project\"quoted");
    assert_eq!(payload["request_id"], "writer:test:a1");
    assert_eq!(payload["projection_elapsed_micros"], 4);
    assert_eq!(payload["codec_radix_work"], "NOT_EXPOSED");
    let mut small = RecordBuffer::<64> {
        bytes: Box::new([0; 64]),
        len: 0,
    };
    // Serialize a genuine validated observation into an insufficient reserved
    // slot, rather than bypassing the descriptor's component length checks.
    assert!(serde_json::to_writer(&mut small, &captured.wire_record()).is_err());
    assert!(small.len <= 64);
    assert_eq!(small.bytes.len(), 64);
    let len = small.len;
    assert!(small.write_all(&[0; 65]).is_err());
    assert_eq!(small.len, len);
    let wire = &record.buffer.bytes[..record.buffer.len];
    let prefix = b"\"payload\":";
    let begin = wire
        .windows(prefix.len())
        .position(|part| part == prefix)
        .unwrap()
        + prefix.len();
    let suffix = b",\"payload_sha256\":";
    let end = wire
        .windows(suffix.len())
        .position(|part| part == suffix)
        .unwrap();
    assert_eq!(
        envelope["payload_sha256"],
        format!("sha256:{}", hex::encode(Sha256::digest(&wire[begin..end])))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn actual_writer_fsync_ack_tracks_exact_bytes_and_closed_drain() {
    let temp = tempfile::tempdir().unwrap();
    let sink = ProjectionObservationSink::start(temp.path()).unwrap();
    sink.enqueue(&observation("/project")).unwrap();
    sink.shutdown(Instant::now() + Duration::from_secs(5))
        .await
        .unwrap();
    let root = temp
        .path()
        .join("logs/mst2-native-projection")
        .join(sink.id.to_string());
    let wire = fs::read(root.join("records.jsonl")).unwrap();
    let status: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("status.json")).unwrap()).unwrap();
    assert_eq!(status["written_bytes"], wire.len());
    assert_eq!(status["written_sequence"], 1);
    assert_eq!(status["accepted_records"], 1);
    assert_eq!(status["first_error_code"], 0);
    assert_eq!(status["closed"], true);
    assert_eq!(
        status["rolling_sha256"],
        format!("sha256:{}", hex::encode(Sha256::digest(&wire)))
    );
    let record: serde_json::Value = serde_json::from_slice(&wire).unwrap();
    assert_eq!(record["payload"].as_object().unwrap().len(), 38);
    assert_eq!(sink.pool.lock().unwrap().len(), RECORD_LIMIT);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for path in [root.join("records.jsonl"), root.join("status.json")] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn expired_drain_deadline_cannot_report_a_successful_capture() {
    let (sink, _) = idle_sink();
    assert_eq!(
        sink.shutdown(Instant::now()).await,
        Err(WriterFailure::DrainTimeout)
    );
    assert_eq!(
        sink.health.first_error.load(Ordering::SeqCst),
        WriterFailure::DrainTimeout as u8
    );
    assert_eq!(
        sink.enqueue(&observation("/project")),
        Err(WriterFailure::WriterUnavailable)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn actual_status_write_failure_and_binding_rejection_cannot_drain_successfully() {
    let temp = tempfile::tempdir().unwrap();
    let sink = ProjectionObservationSink::start(temp.path()).unwrap();
    let root = temp
        .path()
        .join("logs/mst2-native-projection")
        .join(sink.id.to_string());
    fs::write(root.join("status.tmp"), b"blocked exclusive temporary").unwrap();
    sink.enqueue(&observation("/project")).unwrap();
    assert!(
        sink.shutdown(Instant::now() + Duration::from_secs(5))
            .await
            .is_err()
    );
    let status: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("status.json")).unwrap()).unwrap();
    assert_eq!(
        status["written_sequence"], 0,
        "no durable sidecar acknowledgement for this record"
    );
    assert_eq!(
        sink.health.first_error.load(Ordering::SeqCst),
        WriterFailure::IoFailure as u8
    );
    let temp = tempfile::tempdir().unwrap();
    let sink = ProjectionObservationSink::start(temp.path()).unwrap();
    sink.reject_binding();
    assert!(
        sink.shutdown(Instant::now() + Duration::from_secs(5))
            .await
            .is_err()
    );
    let root = temp
        .path()
        .join("logs/mst2-native-projection")
        .join(sink.id.to_string());
    let status: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("status.json")).unwrap()).unwrap();
    assert_eq!(
        status["first_error_code"],
        WriterFailure::ObservationBindingRejected as u8
    );
}

#[cfg(unix)]
#[test]
fn actual_writer_rejects_a_symlinked_output_directory_before_creating_records() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("logs")).unwrap();
    symlink(
        target.path(),
        temp.path().join("logs/mst2-native-projection"),
    )
    .unwrap();
    assert!(ProjectionObservationSink::start(temp.path()).is_err());
    assert_eq!(target.path().read_dir().unwrap().count(), 0);
}
