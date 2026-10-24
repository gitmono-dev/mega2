//! Complexity and deadline admission for one traces unpack.
//!
//! Counts, depths, object size, and the deadline come from
//! [`crate::config::TracesProtocolLimits`]. A cancelled attempt drops its
//! counters and must not continue.

use std::time::{Duration, Instant};

use crate::{
    common::errors::{MegaError, TracesError},
    config::TracesProtocolLimits,
};

/// Running complexity for one ingest worker.
#[derive(Debug, Clone)]
pub struct IngestAdmission {
    started: Instant,
    continuation_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    worker_done: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(test)]
    worker_started: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(test)]
    input_dropped: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    compressed_bytes: u64,
    decoded_bytes: u64,
    elapsed_secs: u64,
    cancelled: bool,
    /// Time the caller attributes to each object already in this attempt.
    pub seconds_per_object: u64,
    pub objects: u64,
    pub new_commits: u64,
    pub delta_depth: u64,
    pub tree_depth: u64,
}

impl IngestAdmission {
    /// Start a worker at the real clock with empty counters.
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
            continuation_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            worker_done: None,
            #[cfg(test)]
            worker_started: None,
            #[cfg(test)]
            input_dropped: None,
            compressed_bytes: 0,
            decoded_bytes: 0,
            elapsed_secs: 0,
            cancelled: false,
            seconds_per_object: 0,
            objects: 0,
            new_commits: 0,
            delta_depth: 0,
            tree_depth: 0,
        }
    }

    /// Record time already spent on this attempt, in whole seconds.
    pub fn record_elapsed(&mut self, elapsed_secs: u64) {
        self.elapsed_secs = self.elapsed_secs.max(elapsed_secs);
    }

    /// Fold the real clock, and one caller's per-object step, into elapsed time.
    pub fn refresh_elapsed(&mut self) {
        self.elapsed_secs = self
            .elapsed_secs
            .saturating_add(self.seconds_per_object)
            .max(self.started.elapsed().as_secs());
    }

    /// Fold only the wall clock into the recorded elapsed time.
    pub fn record_wall_clock(&mut self) {
        self.record_elapsed(self.started.elapsed().as_secs());
    }

    /// Cancel this worker and drop the counters it was holding.
    pub fn cancel(&mut self) {
        self.cancelled = true;
        self.continuation_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        self.compressed_bytes = 0;
        self.decoded_bytes = 0;
        self.objects = 0;
        self.new_commits = 0;
        self.delta_depth = 0;
        self.tree_depth = 0;
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

/// Reject a sample that is outside the protocol budget.
pub fn admit_ingest(admission: &IngestAdmission, max_object_bytes: u64) -> Result<(), MegaError> {
    let elapsed = Duration::from_secs(admission.elapsed_secs).max(admission.started.elapsed());
    if admission.cancelled
        || admission
            .continuation_cancel
            .load(std::sync::atomic::Ordering::Acquire)
        || elapsed > Duration::from_secs(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS)
    {
        return Err(MegaError::Traces(TracesError::QueryTimeout));
    }
    if admission.objects > TracesProtocolLimits::MAX_OBJECTS
        || admission.new_commits > TracesProtocolLimits::MAX_NEW_COMMITS
        || admission.delta_depth > TracesProtocolLimits::MAX_DELTA_DEPTH
        || admission.tree_depth > TracesProtocolLimits::MAX_TREE_DEPTH
        || max_object_bytes > TracesProtocolLimits::MAX_OBJECT_BYTES
    {
        return Err(MegaError::Traces(TracesError::ResponseBudget));
    }
    Ok(())
}

/// Reject a checkpoint metadata or manifest blob above its raw-byte limit.
///
/// Any other name is not one of those two roles.
pub fn admit_raw_capture_role(role: &str, raw_bytes: u64) -> Result<(), MegaError> {
    let limit = match role {
        "metadata" => TracesProtocolLimits::MAX_METADATA_BYTES,
        "manifest" => TracesProtocolLimits::MAX_MANIFEST_BYTES,
        _ => return Ok(()),
    };
    if raw_bytes > limit {
        return Err(MegaError::Traces(TracesError::ResponseBudget));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    use super::*;

    async fn wait_for_signal(signal: &AtomicBool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !signal.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("worker lifecycle signal");
    }

    struct ReleasePool(Option<mpsc::Sender<()>>);
    impl Drop for ReleasePool {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    fn occupied_pool() -> (
        tokio::runtime::Runtime,
        ReleasePool,
        tokio::task::JoinHandle<()>,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("runtime");
        let (started, started_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let release = ReleasePool(Some(release));
        let blocker = runtime.spawn_blocking(move || {
            started.send(()).expect("slot occupied");
            release_rx.recv().expect("release slot");
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("blocking slot acquired");
        (runtime, release, blocker)
    }

    fn release_pool(
        runtime: tokio::runtime::Runtime,
        release: ReleasePool,
        blocker: tokio::task::JoinHandle<()>,
    ) {
        drop(release);
        runtime.block_on(blocker).expect("blocking slot released");
        runtime.shutdown_timeout(Duration::from_secs(1));
    }

    struct ObservedInput<'a> {
        bytes: &'a [u8],
        eof: Arc<AtomicBool>,
    }
    impl tokio::io::AsyncRead for ObservedInput<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            out: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let count = self.bytes.len().min(out.remaining());
            out.put_slice(&self.bytes[..count]);
            self.bytes = &self.bytes[count..];
            if count == 0 {
                self.eof.store(true, Ordering::Release);
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    struct CancelOnEof<R> {
        inner: R,
        cancel: Arc<AtomicBool>,
        eof: Arc<AtomicBool>,
    }
    impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CancelOnEof<R> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            out: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let before = out.filled().len();
            let remaining = out.remaining();
            let result =
                tokio::io::AsyncRead::poll_read(std::pin::Pin::new(&mut self.inner), cx, out);
            if matches!(&result, std::task::Poll::Ready(Ok(())))
                && remaining != 0
                && out.filled().len() == before
            {
                self.eof.store(true, Ordering::Release);
                self.cancel.store(true, Ordering::Release);
            }
            result
        }
    }

    fn at_limit() -> IngestAdmission {
        let mut admission = IngestAdmission::start();
        admission.objects = TracesProtocolLimits::MAX_OBJECTS;
        admission.new_commits = TracesProtocolLimits::MAX_NEW_COMMITS;
        admission.delta_depth = TracesProtocolLimits::MAX_DELTA_DEPTH;
        admission.tree_depth = TracesProtocolLimits::MAX_TREE_DEPTH;
        admission.record_elapsed(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS);
        admission
    }

    #[test]
    fn ingest_admission_budget_accepts_each_limit_and_rejects_one_past_it() {
        let admission = at_limit();
        admit_ingest(&admission, TracesProtocolLimits::MAX_OBJECT_BYTES)
            .expect("every limit is inclusive");

        let mut objects = at_limit();
        objects.objects = TracesProtocolLimits::MAX_OBJECTS + 1;
        assert!(admit_ingest(&objects, 1).is_err(), "object count");

        let mut commits = at_limit();
        commits.new_commits = TracesProtocolLimits::MAX_NEW_COMMITS + 1;
        assert!(admit_ingest(&commits, 1).is_err(), "new commits");

        let mut delta = at_limit();
        delta.delta_depth = TracesProtocolLimits::MAX_DELTA_DEPTH + 1;
        assert!(admit_ingest(&delta, 1).is_err(), "delta depth");

        let mut tree = at_limit();
        tree.tree_depth = TracesProtocolLimits::MAX_TREE_DEPTH + 1;
        assert!(admit_ingest(&tree, 1).is_err(), "tree depth");

        let mut deadline = at_limit();
        deadline.record_elapsed(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS + 1);
        assert!(admit_ingest(&deadline, 1).is_err(), "deadline");

        let mut fraction = IngestAdmission::start();
        fraction.started = Instant::now()
            .checked_sub(
                Duration::from_secs(TracesProtocolLimits::INGEST_TIMEOUT_SECONDS)
                    + Duration::from_millis(50),
            )
            .expect("deadline in the past");
        assert!(
            admit_ingest(&fraction, 1).is_err(),
            "a fractional second past 300s is over the deadline"
        );

        assert!(
            admit_ingest(&at_limit(), TracesProtocolLimits::MAX_OBJECT_BYTES + 1).is_err(),
            "object size"
        );

        let mut cancelled = at_limit();
        cancelled.cancel();
        assert!(cancelled.is_cancelled());
        assert_eq!(cancelled.objects, 0);
        assert_eq!(cancelled.new_commits, 0);
        assert_eq!(cancelled.delta_depth, 0);
        assert_eq!(cancelled.tree_depth, 0);
        assert!(admit_ingest(&cancelled, 1).is_err(), "cancelled worker");
    }

    #[test]
    fn raw_capture_document_size_accepts_each_role_limit_and_rejects_one_past_it() {
        admit_raw_capture_role("metadata", TracesProtocolLimits::MAX_METADATA_BYTES)
            .expect("metadata limit is inclusive");
        assert!(
            admit_raw_capture_role("metadata", TracesProtocolLimits::MAX_METADATA_BYTES + 1)
                .is_err(),
            "metadata one byte over"
        );
        admit_raw_capture_role("manifest", TracesProtocolLimits::MAX_MANIFEST_BYTES)
            .expect("manifest limit is inclusive");
        assert!(
            admit_raw_capture_role("manifest", TracesProtocolLimits::MAX_MANIFEST_BYTES + 1)
                .is_err(),
            "manifest one byte over"
        );
        assert!(
            admit_raw_capture_role("notes", TracesProtocolLimits::MAX_METADATA_BYTES + 1).is_ok(),
            "another file name is not a capture role"
        );
    }
    fn synthetic_pack() -> Vec<u8> {
        use git_internal::{hash::HashKind, utils::HashAlgorithm};
        let mut pack = b"PACK\x00\x00\x00\x02\x00\x00\x00\x01".to_vec();
        pack.push(0x31);
        pack.extend([0x78, 0x9c, 0x4b, 0x04, 0x00, 0x00, 0x62, 0x00, 0x62]);
        let mut hasher = HashAlgorithm::new_for_kind(HashKind::Sha1);
        hasher.update(&pack);
        pack.extend(hasher.finalize());
        pack
    }

    #[derive(Default)]
    struct CountingSink {
        chunks: usize,
        bytes: usize,
    }

    impl crate::ceres::traces::pack_decode::PackSink for CountingSink {
        fn object_header(
            &mut self,
            _object_type: git_internal::internal::object::types::ObjectType,
            _size: usize,
        ) -> Result<(), crate::ceres::traces::pack_decode::PackError> {
            Ok(())
        }

        fn object_chunk(
            &mut self,
            chunk: &[u8],
        ) -> Result<(), crate::ceres::traces::pack_decode::PackError> {
            self.chunks += 1;
            self.bytes += chunk.len();
            Ok(())
        }

        fn object_end(&mut self) -> Result<(), crate::ceres::traces::pack_decode::PackError> {
            Ok(())
        }
    }

    #[test]
    fn wire_ingest_lifetime_clock_and_cancellation_stop_the_worker() {
        use crate::ceres::traces::pack_decode::{PackError, decode_pack};

        // Fixed 300s boundary: at the limit is still admitted, one second over
        // is rejected before any object is processed.
        let limit = TracesProtocolLimits::INGEST_TIMEOUT_SECONDS;
        let mut admission = IngestAdmission::start();
        admission.record_elapsed(limit);
        assert!(admit_ingest(&admission, 1).is_ok());
        admission.record_elapsed(limit + 1);
        assert!(admit_ingest(&admission, 1).is_err());

        // A worker already over the deadline stops before inflating anything.
        let pack = synthetic_pack();
        let mut admission = IngestAdmission::start();
        admission.record_elapsed(limit + 1);
        let mut sink = CountingSink::default();
        let mut bounded = BoundedPackSink::new(&mut sink, &mut admission, PackBudget::fixed());
        let outcome = decode_pack(&pack[..], git_internal::hash::HashKind::Sha1, &mut bounded);
        assert!(matches!(outcome, Err(PackError::Budget("deadline"))));
        assert_eq!(sink.chunks, 0);
    }

    #[tokio::test]
    async fn wire_ingest_lifetime_stalled_input_cancels_and_joins() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (source, _writer) = tokio::io::duplex(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let mut admission = IngestAdmission::start();
        admission.worker_done = Some(done.clone());
        admission.worker_started = Some(started.clone());
        let signal = cancel.clone();
        let setter = tokio::spawn(async move {
            wait_for_signal(&started).await;
            signal.store(true, Ordering::Release);
        });
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            super::super::ingest::decode_wire_pack_with_control(
                &mut admission,
                git_internal::hash::HashKind::Sha1,
                source,
                cancel,
            ),
        )
        .await
        .expect("cancellation stops the stalled reader")
        .expect_err("cancelled");
        setter.await.expect("signal task");
        assert!(matches!(
            error,
            MegaError::Traces(TracesError::QueryTimeout)
        ));
        assert!(
            done.load(Ordering::Acquire),
            "worker has exited before response"
        );
    }

    #[tokio::test]
    async fn wire_ingest_lifetime_dropped_request_releases_worker() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (source, _writer) = tokio::io::duplex(64);
        let done = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let mut admission = IngestAdmission::start();
        admission.worker_done = Some(done.clone());
        admission.worker_started = Some(started.clone());
        {
            let future = super::super::ingest::decode_wire_pack(
                &mut admission,
                git_internal::hash::HashKind::Sha1,
                source,
            );
            tokio::pin!(future);
            tokio::select! {
                _ = &mut future => panic!("stalled input completed"),
                _ = wait_for_signal(&started) => {}
            }
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while !done.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("dropped request stops its worker");
        assert!(
            admission.is_cancelled(),
            "abandoned counters cannot be reused"
        );
    }

    #[tokio::test]
    async fn wire_ingest_lifetime_preserves_clock_and_checks_cpu_phase() {
        let pack = synthetic_pack();
        let mut admission = IngestAdmission::start();
        admission.started = Instant::now()
            .checked_sub(Duration::from_secs(301))
            .expect("past");
        let error = super::super::ingest::decode_wire_pack(
            &mut admission,
            git_internal::hash::HashKind::Sha1,
            &pack[..],
        )
        .await
        .expect_err("original deadline");
        assert!(matches!(
            error,
            MegaError::Traces(TracesError::QueryTimeout)
        ));
        let admission = IngestAdmission::start();
        let control = PackControl::new(&admission, PackBudget::fixed()).expect("control");
        control
            .stop
            .store(true, std::sync::atomic::Ordering::Release);
        let error = super::super::ingest::decoded_tree_depth(
            git_internal::hash::HashKind::Sha1,
            &[],
            64,
            &control,
        )
        .expect_err("CPU phase shares cancellation");
        assert!(matches!(
            error,
            MegaError::Traces(TracesError::QueryTimeout)
        ));
    }
    #[test]
    fn wire_ingest_lifetime_queued_worker_is_cancelled_without_starting() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        let (runtime, release, blocker) = occupied_pool();
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let eof = Arc::new(AtomicBool::new(false));
        let mut admission = IngestAdmission::start();
        admission.worker_done = Some(done.clone());
        admission.input_dropped = Some(dropped.clone());
        let pack = synthetic_pack();
        let result = runtime.block_on(async {
            let flag = cancel.clone();
            let setter = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                flag.store(true, Ordering::Release);
            });
            let result = tokio::time::timeout(
                Duration::from_millis(200),
                super::super::ingest::decode_wire_pack_with_control(
                    &mut admission,
                    git_internal::hash::HashKind::Sha1,
                    ObservedInput {
                        bytes: &pack,
                        eof: eof.clone(),
                    },
                    cancel,
                ),
            )
            .await;
            setter.await.expect("cancellation task");
            result
        });
        let reclaimed_before_release = dropped.load(Ordering::Acquire);
        release_pool(runtime, release, blocker);
        assert!(
            reclaimed_before_release,
            "queued payload freed before slot release"
        );
        assert!(
            eof.load(Ordering::Acquire),
            "input ended before cancellation"
        );
        let error = result
            .expect("queued decode must cancel before the occupied slot is released")
            .expect_err("cancelled decode");
        assert!(matches!(
            error,
            MegaError::Traces(TracesError::QueryTimeout)
        ));
        assert!(
            !done.load(Ordering::Acquire),
            "cancelled queued decoder never runs"
        );
        assert!(admission.is_cancelled());
    }

    #[test]
    fn wire_ingest_lifetime_dropped_queued_request_reclaims_payload() {
        let (runtime, release, blocker) = occupied_pool();
        let done = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let eof = Arc::new(AtomicBool::new(false));
        let mut admission = IngestAdmission::start();
        admission.worker_done = Some(done.clone());
        admission.input_dropped = Some(dropped.clone());
        let pack = synthetic_pack();
        runtime.block_on(async {
            let request = super::super::ingest::decode_wire_pack(
                &mut admission,
                git_internal::hash::HashKind::Sha1,
                ObservedInput {
                    bytes: &pack,
                    eof: eof.clone(),
                },
            );
            tokio::pin!(request);
            tokio::select! {
                _ = &mut request => panic!("queued worker cannot complete before slot release"),
                _ = wait_for_signal(&eof) => {}
            }
        });
        let reclaimed_before_release = dropped.load(Ordering::Acquire);
        release_pool(runtime, release, blocker);
        assert!(
            reclaimed_before_release,
            "dropping request frees queued input immediately"
        );
        assert!(!done.load(Ordering::Acquire), "queued decode never started");
        assert!(admission.is_cancelled());
    }

    #[tokio::test]
    async fn wire_ingest_lifetime_started_worker_cancelled_after_eof() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, source) = tokio::io::duplex(64);
        let started = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let eof = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut admission = IngestAdmission::start();
        admission.worker_started = Some(started.clone());
        admission.worker_done = Some(done.clone());
        let sender = tokio::spawn(async move {
            wait_for_signal(&started).await;
            writer
                .write_all(&synthetic_pack())
                .await
                .expect("pack bytes");
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            super::super::ingest::decode_wire_pack_with_control(
                &mut admission,
                git_internal::hash::HashKind::Sha1,
                CancelOnEof {
                    inner: source,
                    eof: eof.clone(),
                    cancel: cancel.clone(),
                },
                cancel,
            ),
        )
        .await
        .expect("post-EOF cancellation completes");
        sender.await.expect("writer released");
        assert!(eof.load(Ordering::Acquire));
        assert!(matches!(
            result,
            Err(MegaError::Traces(TracesError::QueryTimeout))
        ));
        assert!(
            done.load(Ordering::Acquire),
            "started worker completed before error response"
        );
        assert!(admission.is_cancelled());
    }

    #[tokio::test]
    async fn wire_ingest_lifetime_budget_error_cancels_partial_admission() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, source) = tokio::io::duplex(64);
        let started = Arc::new(AtomicBool::new(false));
        let mut admission = IngestAdmission::start();
        admission.objects = 7;
        admission.new_commits = 2;
        admission.worker_started = Some(started.clone());
        let sender = tokio::spawn(async move {
            wait_for_signal(&started).await;
            writer.write_all(b"P").await.expect("budget probe");
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            super::decode_wire_pack_with_budget(
                &mut admission,
                git_internal::hash::HashKind::Sha1,
                source,
                PackBudget::for_test(0, u64::MAX, u64::MAX, u64::MAX, u64::MAX, 300),
            ),
        )
        .await
        .expect("budget rejection completes");
        sender.await.expect("writer joined");
        assert!(matches!(
            result,
            Err(MegaError::Traces(TracesError::ResponseBudget))
        ));
        assert!(admission.is_cancelled());
        assert_eq!(
            (
                admission.objects,
                admission.new_commits,
                admission.compressed_bytes,
                admission.decoded_bytes
            ),
            (0, 0, 0, 0)
        );
    }

    #[tokio::test]
    async fn wire_ingest_lifetime_continuation_preserves_deadline_and_external_cancel() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        use crate::ceres::traces::publication::tests::Fixture;
        let mut fixture = Fixture::new(git_internal::hash::HashKind::Sha1);
        fixture.tree(&[]);
        let pack = fixture.pack(None);
        let mut admission = IngestAdmission::start();
        admission.started = Instant::now() - Duration::from_secs(1);
        let decoded = decode_wire_pack_with_budget(
            &mut admission,
            fixture.kind,
            pack.as_slice(),
            PackBudget::for_test(4096, 4096, 4096, 10, 10, 2),
        )
        .await
        .unwrap();
        assert!(decoded.continuation().deadline <= admission.started + Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(matches!(
            decoded.continuation().check(),
            Err(MegaError::Traces(TracesError::QueryTimeout))
        ));
        let flag = Arc::new(AtomicBool::new(false));
        let decoded = decode_wire_pack(
            &mut IngestAdmission::start(),
            fixture.kind,
            pack.as_slice(),
            Some(flag.clone()),
        )
        .await
        .unwrap();
        let cancel = async {
            tokio::task::yield_now().await;
            flag.store(true, Ordering::Release);
        };
        let (result, ()) = tokio::join!(
            decoded
                .continuation()
                .run(std::future::pending::<Result<(), MegaError>>()),
            cancel
        );
        assert!(matches!(
            result,
            Err(MegaError::Traces(TracesError::QueryTimeout))
        ));
        assert!(matches!(
            decoded.continuation().checkpoint().await,
            Err(MegaError::Traces(TracesError::QueryTimeout))
        ));
    }

    #[test]
    fn bounded_pack_debug_does_not_expose_payload() {
        let payload = b"not-for-logs".repeat(256);
        let rendered_payload = format!("{payload:?}");
        let decoded = BoundedDecodedPack {
            hash_kind: git_internal::hash::HashKind::Sha1,
            continuation: IngestContinuation {
                deadline: Instant::now() + Duration::from_secs(300),
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                external_cancel: None,
            },
            decoded_bytes: payload.len() as u64,
            objects: 1,
            entries: vec![(
                git_internal::internal::object::types::ObjectType::Blob,
                payload,
            )],
        };
        let printed = format!("{decoded:?}");
        assert!(
            printed.len() < 200,
            "Debug output remains bounded independently of payload"
        );
        assert!(!printed.contains(&rendered_payload));
        assert!(!printed.contains("not-for-logs"));
        assert!(printed.contains("decoded_bytes") && printed.contains("objects"));
    }

    #[tokio::test]
    async fn wire_ingest_lifetime_control_entry_maps_cancellation_to_timeout() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        use crate::ceres::traces::ingest::decode_wire_pack_with_control;

        let pack = synthetic_pack();
        // The worker is already cancelled before the first read: the control
        // entry must reject without decoding a single object.
        let cancel = Arc::new(AtomicBool::new(true));
        let reader = &pack[..];
        let mut admission = IngestAdmission::start();
        let outcome = decode_wire_pack_with_control(
            &mut admission,
            git_internal::hash::HashKind::Sha1,
            reader,
            cancel.clone(),
        )
        .await;
        assert!(cancel.load(Ordering::SeqCst));
        assert!(matches!(
            outcome,
            Err(crate::common::errors::MegaError::Traces(
                crate::common::errors::TracesError::QueryTimeout
            ))
        ));
    }
}

/// Limits are constructed inside the wire entry; test overrides stay internal.
#[derive(Debug, Clone, Copy)]
pub(super) struct PackBudget {
    pub(super) max_pack_bytes: u64,
    pub(super) max_decoded_bytes: u64,
    pub(super) max_object_bytes: u64,
    pub(super) max_objects: u64,
    pub(super) max_new_commits: u64,
    pub(super) max_tree_depth: u64,
    pub(super) timeout_seconds: u64,
}

impl PackBudget {
    pub(super) const fn fixed() -> Self {
        Self {
            max_pack_bytes: TracesProtocolLimits::MAX_PACK_BYTES,
            max_decoded_bytes: TracesProtocolLimits::MAX_DECODED_BYTES,
            max_object_bytes: TracesProtocolLimits::MAX_OBJECT_BYTES,
            max_objects: TracesProtocolLimits::MAX_OBJECTS,
            max_new_commits: TracesProtocolLimits::MAX_NEW_COMMITS,
            max_tree_depth: TracesProtocolLimits::MAX_TREE_DEPTH,
            timeout_seconds: TracesProtocolLimits::INGEST_TIMEOUT_SECONDS,
        }
    }

    #[cfg(test)]
    pub(super) const fn with_tree_depth(mut self, depth: u64) -> Self {
        self.max_tree_depth = depth;
        self
    }

    #[cfg(test)]
    pub(super) const fn for_test(
        max_pack_bytes: u64,
        max_decoded_bytes: u64,
        max_object_bytes: u64,
        max_objects: u64,
        max_new_commits: u64,
        timeout_seconds: u64,
    ) -> Self {
        Self {
            max_pack_bytes,
            max_decoded_bytes,
            max_object_bytes,
            max_objects,
            max_new_commits,
            timeout_seconds,
            ..Self::fixed()
        }
    }
}

#[derive(Clone)]
pub(super) struct PackControl {
    deadline: Instant,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    request_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl PackControl {
    fn new(admission: &IngestAdmission, budget: PackBudget) -> Result<Self, MegaError> {
        let now = Instant::now();
        let duration = Duration::from_secs(budget.timeout_seconds);
        let wall = admission.started.checked_add(duration);
        let recorded =
            now.checked_add(duration.saturating_sub(Duration::from_secs(admission.elapsed_secs)));
        let deadline = wall
            .zip(recorded)
            .map(|(a, b)| a.min(b))
            .ok_or_else(timeout)?;
        Ok(Self {
            deadline,
            stop: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cancel: None,
            request_cancel: admission.continuation_cancel.clone(),
        })
    }

    pub(super) fn check(&self) -> Result<(), MegaError> {
        use std::sync::atomic::Ordering;
        if self.stop.load(Ordering::Acquire)
            || self.request_cancel.load(Ordering::Acquire)
            || self
                .cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            || Instant::now() > self.deadline
        {
            return Err(timeout());
        }
        Ok(())
    }
}

fn timeout() -> MegaError {
    MegaError::Traces(TracesError::QueryTimeout)
}

struct PendingDecode {
    state: IngestAdmission,
    input: PackInput,
    kind: git_internal::hash::HashKind,
    budget: PackBudget,
}

type PendingWorker = std::sync::Arc<std::sync::Mutex<Option<PendingDecode>>>;

fn take_pending(pending: &PendingWorker) -> Option<PendingDecode> {
    // The lock only transfers ownership; no decoder runs while it is held.
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
}

struct StopWorker<'a> {
    control: PackControl,
    admission: &'a mut IngestAdmission,
    pending: PendingWorker,
    abort: Option<tokio::task::AbortHandle>,
    finished: bool,
}

impl StopWorker<'_> {
    fn reclaim_queued(&self) -> bool {
        if let Some(pending) = take_pending(&self.pending) {
            drop(pending);
            if let Some(abort) = &self.abort {
                abort.abort();
            }
            true
        } else {
            false
        }
    }
}

impl Drop for StopWorker<'_> {
    fn drop(&mut self) {
        self.control
            .stop
            .store(true, std::sync::atomic::Ordering::Release);
        self.reclaim_queued();
        if !self.finished {
            self.admission.cancel();
        }
    }
}

/// A result can only be constructed by the controlled decode implementation.
pub struct BoundedDecodedPack {
    entries: DecodedEntries,
    decoded_bytes: u64,
    objects: u64,
    hash_kind: git_internal::hash::HashKind,
    continuation: IngestContinuation,
}

pub(super) type DecodedEntries = Vec<(git_internal::internal::object::types::ObjectType, Vec<u8>)>;

/// Request lifetime after the decoder has joined. Worker shutdown is separate.
#[derive(Clone)]
pub(crate) struct IngestContinuation {
    deadline: Instant,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    external_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl IngestContinuation {
    pub(crate) fn check(&self) -> Result<(), MegaError> {
        use std::sync::atomic::Ordering;
        if Instant::now() > self.deadline
            || self.cancelled.load(Ordering::Acquire)
            || self
                .external_cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(timeout());
        }
        Ok(())
    }

    pub(crate) async fn checkpoint(&self) -> Result<(), MegaError> {
        self.check()?;
        tokio::task::yield_now().await;
        self.check()
    }

    pub(crate) async fn run<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, MegaError>>,
    ) -> Result<T, MegaError> {
        self.check()?;
        tokio::pin!(future);
        let mut timer = tokio::time::interval(Duration::from_millis(5));
        loop {
            tokio::select! {
                biased;
                _ = timer.tick() => self.check()?,
                result = &mut future => {
                    self.check()?;
                    return result;
                }
            }
        }
    }
}

impl std::fmt::Debug for BoundedDecodedPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundedDecodedPack")
            .field("decoded_bytes", &self.decoded_bytes)
            .field("objects", &self.objects)
            .finish_non_exhaustive()
    }
}

impl BoundedDecodedPack {
    pub fn entries(&self) -> &[(git_internal::internal::object::types::ObjectType, Vec<u8>)] {
        &self.entries
    }

    pub fn decoded_bytes(&self) -> u64 {
        self.decoded_bytes
    }

    pub fn object_count(&self) -> u64 {
        self.objects
    }

    pub(crate) fn continuation(&self) -> &IngestContinuation {
        &self.continuation
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        git_internal::hash::HashKind,
        DecodedEntries,
        IngestContinuation,
    ) {
        (self.hash_kind, self.entries, self.continuation)
    }
}

#[derive(Default)]
struct PackCollector {
    entries: Vec<(git_internal::internal::object::types::ObjectType, Vec<u8>)>,
    current: Option<(git_internal::internal::object::types::ObjectType, Vec<u8>)>,
}

impl super::pack_decode::PackSink for PackCollector {
    fn object_header(
        &mut self,
        object_type: git_internal::internal::object::types::ObjectType,
        size: usize,
    ) -> Result<(), super::pack_decode::PackError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| super::pack_decode::PackError::Budget("allocation"))?;
        self.current = Some((object_type, bytes));
        Ok(())
    }

    fn object_chunk(&mut self, chunk: &[u8]) -> Result<(), super::pack_decode::PackError> {
        if let Some((_, bytes)) = &mut self.current {
            bytes.extend_from_slice(chunk);
        }
        Ok(())
    }

    fn object_end(&mut self) -> Result<(), super::pack_decode::PackError> {
        if let Some(entry) = self.current.take() {
            self.entries
                .try_reserve(1)
                .map_err(|_| super::pack_decode::PackError::Budget("allocation"))?;
            self.entries.push(entry);
        }
        Ok(())
    }
}

pub(super) struct BoundedPackSink<'a, S> {
    inner: S,
    admission: &'a mut IngestAdmission,
    budget: PackBudget,
    control: PackControl,
    decoded_bytes: u64,
    objects: u64,
}

impl<'a, S> BoundedPackSink<'a, S> {
    #[cfg(test)]
    pub(super) fn new(inner: S, admission: &'a mut IngestAdmission, budget: PackBudget) -> Self {
        let control = PackControl::new(admission, budget).expect("test deadline fits Instant");
        Self {
            inner,
            admission,
            budget,
            control,
            decoded_bytes: 0,
            objects: 0,
        }
    }

    fn check(&self) -> Result<(), super::pack_decode::PackError> {
        if self.admission.is_cancelled() || self.control.check().is_err() {
            return Err(super::pack_decode::PackError::Budget("deadline"));
        }
        Ok(())
    }
}

impl<S: super::pack_decode::PackSink> super::pack_decode::PackSink for BoundedPackSink<'_, S> {
    fn object_header(
        &mut self,
        object_type: git_internal::internal::object::types::ObjectType,
        size: usize,
    ) -> Result<(), super::pack_decode::PackError> {
        use super::pack_decode::PackError;
        self.check()?;
        if size as u64 > self.budget.max_object_bytes {
            return Err(PackError::Budget("object bytes"));
        }
        if size as u64
            > self
                .budget
                .max_decoded_bytes
                .saturating_sub(self.admission.decoded_bytes)
        {
            return Err(PackError::Budget("decoded bytes"));
        }
        if self.admission.objects >= self.budget.max_objects {
            return Err(PackError::Budget("objects"));
        }
        if object_type == git_internal::internal::object::types::ObjectType::Commit {
            if self.admission.new_commits >= self.budget.max_new_commits {
                return Err(PackError::Budget("new commits"));
            }
            self.admission.new_commits += 1;
        }
        self.admission.objects += 1;
        self.objects += 1;
        self.inner.object_header(object_type, size)
    }

    fn object_chunk(&mut self, chunk: &[u8]) -> Result<(), super::pack_decode::PackError> {
        self.check()?;
        let next = self
            .admission
            .decoded_bytes
            .saturating_add(chunk.len() as u64);
        if next > self.budget.max_decoded_bytes {
            return Err(super::pack_decode::PackError::Budget("decoded bytes"));
        }
        self.admission.decoded_bytes = next;
        self.decoded_bytes += chunk.len() as u64;
        self.inner.object_chunk(chunk)
    }

    fn object_end(&mut self) -> Result<(), super::pack_decode::PackError> {
        self.check()?;
        self.inner.object_end()
    }

    fn chunk_limit(&self) -> Option<usize> {
        Some(
            usize::try_from(
                self.budget
                    .max_decoded_bytes
                    .saturating_sub(self.admission.decoded_bytes)
                    .saturating_add(1),
            )
            .unwrap_or(usize::MAX),
        )
    }
}

/// The synchronous decoder never waits on caller-controlled blocking I/O.
struct PackInput {
    receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
    chunk: std::io::Cursor<Vec<u8>>,
    control: PackControl,
    #[cfg(test)]
    drop_probe: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[cfg(test)]
impl Drop for PackInput {
    fn drop(&mut self) {
        if let Some(probe) = &self.drop_probe {
            probe.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

impl std::io::Read for PackInput {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            self.control.check().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "pack ingest stopped")
            })?;
            let read = std::io::Read::read(&mut self.chunk, out)?;
            if read != 0 {
                return Ok(read);
            }
            match self.receiver.try_recv() {
                Ok(chunk) => self.chunk = std::io::Cursor::new(chunk),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return Ok(0),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
            }
        }
    }
}

fn decode_worker(
    admission: &mut IngestAdmission,
    kind: git_internal::hash::HashKind,
    input: PackInput,
    budget: PackBudget,
    control: PackControl,
) -> Result<BoundedDecodedPack, MegaError> {
    use super::pack_decode::{PackError, decode_pack};
    let mut sink = BoundedPackSink {
        inner: PackCollector::default(),
        admission,
        budget,
        control: control.clone(),
        decoded_bytes: 0,
        objects: 0,
    };
    let outcome = decode_pack(input, kind, &mut sink);
    control.check()?;
    match outcome {
        Ok(_) => {
            let depth = super::ingest::decoded_tree_depth(
                kind,
                &sink.inner.entries,
                budget.max_tree_depth,
                &control,
            )?;
            control.check()?;
            sink.admission.tree_depth = sink.admission.tree_depth.max(depth);
            Ok(BoundedDecodedPack {
                entries: sink.inner.entries,
                decoded_bytes: sink.decoded_bytes,
                objects: sink.objects,
                hash_kind: kind,
                continuation: IngestContinuation {
                    deadline: control.deadline,
                    cancelled: sink.admission.continuation_cancel.clone(),
                    external_cancel: control.cancel.clone(),
                },
            })
        }
        Err(PackError::Budget("deadline")) => Err(timeout()),
        Err(PackError::Budget(_)) => Err(MegaError::Traces(TracesError::ResponseBudget)),
        Err(_) => Err(MegaError::Traces(TracesError::InvalidParameter)),
    }
}

pub(super) async fn decode_wire_pack<R: tokio::io::AsyncRead + Unpin>(
    admission: &mut IngestAdmission,
    kind: git_internal::hash::HashKind,
    input: R,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<BoundedDecodedPack, MegaError> {
    decode_wire_pack_budgeted(admission, kind, input, PackBudget::fixed(), cancel).await
}

#[cfg(test)]
pub(super) async fn decode_wire_pack_with_budget<R: tokio::io::AsyncRead + Unpin>(
    admission: &mut IngestAdmission,
    kind: git_internal::hash::HashKind,
    input: R,
    budget: PackBudget,
) -> Result<BoundedDecodedPack, MegaError> {
    decode_wire_pack_budgeted(admission, kind, input, budget, None).await
}

async fn decode_wire_pack_budgeted<R: tokio::io::AsyncRead + Unpin>(
    admission: &mut IngestAdmission,
    kind: git_internal::hash::HashKind,
    input: R,
    budget: PackBudget,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<BoundedDecodedPack, MegaError> {
    let result = run_wire_pack(admission, kind, input, budget, cancel).await;
    if result.is_err() {
        admission.cancel();
    }
    result
}

async fn run_wire_pack<R: tokio::io::AsyncRead + Unpin>(
    admission: &mut IngestAdmission,
    kind: git_internal::hash::HashKind,
    mut input: R,
    budget: PackBudget,
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<BoundedDecodedPack, MegaError> {
    use tokio::io::AsyncReadExt;
    admit_ingest(admission, 0)?;
    let mut control = PackControl::new(admission, budget)?;
    control.cancel = cancel;
    control.check()?;
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let pending = std::sync::Arc::new(std::sync::Mutex::new(Some(PendingDecode {
        state: admission.clone(),
        input: PackInput {
            receiver,
            chunk: std::io::Cursor::new(Vec::new()),
            control: control.clone(),
            #[cfg(test)]
            drop_probe: admission.input_dropped.clone(),
        },
        kind,
        budget,
    })));
    let mut guard = StopWorker {
        control: control.clone(),
        admission,
        pending: pending.clone(),
        abort: None,
        finished: false,
    };
    let mut worker = tokio::task::spawn_blocking(move || {
        let PendingDecode {
            mut state,
            input,
            kind,
            budget,
        } = take_pending(&pending)?;
        #[cfg(test)]
        let _completion = WorkerCompletion(state.worker_done.clone());
        #[cfg(test)]
        if let Some(started) = &state.worker_started {
            started.store(true, std::sync::atomic::Ordering::Release);
        }
        let control = input.control.clone();
        let outcome = decode_worker(&mut state, kind, input, budget, control);
        Some((outcome, state))
    });
    guard.abort = Some(worker.abort_handle());
    let mut timer = tokio::time::interval(Duration::from_millis(5));
    let mut buffer = vec![0; super::pack_decode::MAX_SINK_CHUNK];
    let mut used = guard.admission.compressed_bytes;
    let feeder_error = 'feed: loop {
        if let Err(error) = control.check() {
            break Some(error);
        }
        let remaining = budget.max_pack_bytes.saturating_sub(used);
        let allowed = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len())
            .max(1);
        let read = tokio::select! {
            biased;
            result = &mut worker => {
                let (outcome, state) = result.map_err(|_| MegaError::Other("pack decoder worker failed".to_owned()))?.ok_or_else(timeout)?;
                *guard.admission = state;
                guard.admission.compressed_bytes = used;
                guard.finished = true;
                control.check()?;
                return outcome;
            }
            _ = timer.tick() => { continue; }
            result = input.read(&mut buffer[..allowed]) => result,
        };
        let read = match read {
            Ok(0) => break None,
            Ok(read) => read,
            Err(_) => break Some(MegaError::Traces(TracesError::InvalidParameter)),
        };
        if read as u64 > remaining {
            break Some(MegaError::Traces(TracesError::ResponseBudget));
        }
        used += read as u64;
        let send = sender.send(buffer[..read].to_vec());
        tokio::pin!(send);
        loop {
            tokio::select! {
                biased;
                result = &mut send => {
                    if result.is_err() { break 'feed None; }
                    break;
                }
                _ = timer.tick() => {
                    if let Err(error) = control.check() { break 'feed Some(error); }
                }
            }
        }
    };
    if feeder_error.is_some() {
        guard
            .control
            .stop
            .store(true, std::sync::atomic::Ordering::Release);
    }
    drop(sender);
    if feeder_error.is_some() && guard.reclaim_queued() {
        return Err(feeder_error.unwrap_or_else(timeout));
    }
    let joined = loop {
        tokio::select! {
            biased;
            result = &mut worker => break result,
            _ = timer.tick() => {
                if control.check().is_err() {
                    control.stop.store(true, std::sync::atomic::Ordering::Release);
                    if guard.reclaim_queued() { return Err(timeout()); }
                }
            }
        }
    };
    let (outcome, state) = joined
        .map_err(|_| MegaError::Other("pack decoder worker failed".to_owned()))?
        .ok_or_else(timeout)?;
    *guard.admission = state;
    guard.admission.compressed_bytes = used;
    guard.finished = true;
    if let Some(error) = feeder_error {
        return Err(error);
    }
    control.check()?;
    outcome
}

#[cfg(test)]
struct WorkerCompletion(Option<std::sync::Arc<std::sync::atomic::AtomicBool>>);

#[cfg(test)]
impl Drop for WorkerCompletion {
    fn drop(&mut self) {
        if let Some(done) = &self.0 {
            done.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}
