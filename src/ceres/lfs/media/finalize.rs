//! Finalize: page-ordered coverage verification, bounded async task, LFS fallback.
//!
//! MF-03 / ADR-MF-03 / P-04a: continuous cover + per-chunk hash/length + full
//! size/oid only — no fresh FastCDC boundary equality. Disk I/O uses
//! `spawn_blocking`. Concurrent workers ≤ [`MAX_CONCURRENT_FINALIZE`].

use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::OnceLock,
    time::{Duration, Instant},
};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    callisto::lfs_objects,
    ceres::lfs::{
        digest::LfsDigest,
        media::{
            protocol::{
                FinalizeAcceptedResponse, FinalizeTaskResponse, MAX_ENVELOPE_SIZE, ManifestError,
                ManifestResponse, MediaManifest,
            },
            scope::{MediaObjectKind, MediaScope, redact_storage_error},
            service::{MediaError, MediaService, map_store, scope_key, unix_now},
        },
    },
    jupiter::storage::{
        lfs_db_storage::LfsDbStorage,
        media_paging_storage::{MediaPagingStorage, TASK_COMPLETE, TASK_FAILED, TASK_PENDING},
    },
    orbit_api::object_storage::{ObjectByteStream, ObjectKey, ObjectMeta, ObjectNamespace},
};

/// Persistent finalize concurrency (P-04b / ADR-MF-03).
pub const MAX_CONCURRENT_FINALIZE: usize = 2;
/// Max pending+running tasks before 429 (P-04b).
pub const MAX_QUEUED_FINALIZE: u64 = 128;
/// Lease renew interval while making progress (P-04b).
pub const LEASE_RENEW_SECS: u64 = 20;
/// No-progress deadline (P-04b); not a whole-file wall clock.
pub const NO_PROGRESS_DEADLINE_SECS: u64 = 600;
/// Single I/O timeout budget (P-04b).
pub const IO_TIMEOUT_SECS: u64 = 120;

const STATUS_URL_PREFIX: &str = "libra/media/v1/tasks/";

fn finalizer_semaphore() -> &'static Semaphore {
    static SEM: OnceLock<Semaphore> = OnceLock::new();
    SEM.get_or_init(|| Semaphore::new(MAX_CONCURRENT_FINALIZE))
}

/// Tunable deadlines for tests (no-progress / lease renew).
#[derive(Debug, Clone, Copy)]
pub struct FinalizeBounds {
    pub no_progress: Duration,
    pub lease_renew: Duration,
    pub io_timeout: Duration,
}

impl Default for FinalizeBounds {
    fn default() -> Self {
        Self {
            no_progress: Duration::from_secs(NO_PROGRESS_DEADLINE_SECS),
            lease_renew: Duration::from_secs(LEASE_RENEW_SECS),
            io_timeout: Duration::from_secs(IO_TIMEOUT_SECS),
        }
    }
}

/// Synchronous finalize helper (unit tests / internal). Requires a sealed session.
pub async fn finalize(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    scope: &MediaScope,
    manifest_id: &str,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
) -> Result<ManifestResponse, MediaError> {
    finalize_at(media, lfs_db, scope, manifest_id, unix_now(), emitter).await
}

pub async fn finalize_at(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    scope: &MediaScope,
    manifest_id: &str,
    now: u64,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
) -> Result<ManifestResponse, MediaError> {
    finalize_at_with_bounds(
        media,
        lfs_db,
        scope,
        manifest_id,
        now,
        emitter,
        FinalizeBounds::default(),
        None,
    )
    .await
}

/// Sync path that still acquires the concurrency semaphore and walks sealed pages.
pub async fn finalize_at_with_bounds(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    scope: &MediaScope,
    manifest_id: &str,
    now: u64,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
    bounds: FinalizeBounds,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<ManifestResponse, MediaError> {
    let _permit = finalizer_semaphore()
        .acquire()
        .await
        .map_err(|_| MediaError::Invalid("finalize semaphore closed".to_string()))?;

    run_finalize_body(
        media,
        lfs_db,
        scope,
        manifest_id,
        now,
        emitter,
        bounds,
        cancel,
        None,
    )
    .await
}

/// P-04a: enqueue (or reuse) a durable finalize task and spawn a worker.
pub async fn enqueue_finalize(
    media: MediaService,
    lfs_db: LfsDbStorage,
    scope: MediaScope,
    manifest_id: String,
    emitter: crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
) -> Result<FinalizeAcceptedResponse, MediaError> {
    media.require_sealed_session(&scope, &manifest_id).await?;
    let digest = scope.digest();
    let paging = media.paging();

    let mut task = paging
        .ensure_task(&digest, &manifest_id)
        .await
        .map_err(map_paging)?;

    if task.state == TASK_COMPLETE {
        return Ok(accepted_from_task(&task));
    }
    if task.state == TASK_FAILED {
        if !task.retryable {
            return Err(MediaError::Conflict(
                "finalize permanently failed; re-prepare a valid layout".into(),
            ));
        }
        task = paging
            .requeue_failed_task(&task.task_id)
            .await
            .map_err(map_paging)?;
    }

    if task.state == TASK_PENDING {
        let active = paging.count_active_tasks().await.map_err(map_paging)?;
        // Count includes this pending task; reject only when over capacity.
        if active > MAX_QUEUED_FINALIZE {
            return Err(MediaError::TooManyRequests);
        }
    }

    let task_id = task.task_id.clone();
    let owner = format!("worker-{}", Uuid::new_v4());
    let media_bg = media.clone();
    let lfs_bg = lfs_db.clone();
    let scope_bg = scope.clone();
    let mid_bg = manifest_id.clone();
    let emitter_bg = emitter.clone();
    let task_id_bg = task_id.clone();

    tokio::spawn(async move {
        let result = run_task_worker(
            media_bg,
            lfs_bg,
            scope_bg,
            mid_bg,
            task_id_bg.clone(),
            owner,
            emitter_bg,
            FinalizeBounds::default(),
        )
        .await;
        if let Err(err) = result {
            tracing::warn!(
                task_id = %task_id_bg,
                error = %err,
                "media finalize worker exited with error"
            );
        }
    });

    Ok(accepted_from_task(&task))
}

/// Scope-reauth task status (P-04a).
pub async fn task_status(
    media: &MediaService,
    scope: &MediaScope,
    task_id: &str,
) -> Result<FinalizeTaskResponse, MediaError> {
    let task = media.paging().get_task(task_id).await.map_err(map_paging)?;
    if task.scope_digest != scope.digest() {
        return Err(MediaError::NotFound);
    }
    let mut resp = FinalizeTaskResponse {
        task_id: task.task_id.clone(),
        manifest_id: task.manifest_id.clone(),
        state: task.state.clone(),
        stage: task.stage.clone(),
        bytes_verified: u64::try_from(task.bytes_verified.max(0)).unwrap_or(0),
        pages_verified: task.pages_verified,
        retryable: task.retryable,
        error_code: task.error_code.clone(),
        oid: None,
        size: None,
    };
    if task.state == TASK_COMPLETE {
        let session = media
            .paging()
            .get_session(&task.scope_digest, &task.manifest_id)
            .await
            .map_err(map_paging)?;
        resp.oid = Some(session.oid);
        resp.size = Some(u64::try_from(session.size.max(0)).unwrap_or(0));
    }
    Ok(resp)
}

fn accepted_from_task(task: &crate::callisto::media_task::Model) -> FinalizeAcceptedResponse {
    FinalizeAcceptedResponse {
        task_id: task.task_id.clone(),
        manifest_id: task.manifest_id.clone(),
        state: task.state.clone(),
        status_url: format!("{STATUS_URL_PREFIX}{}", task.task_id),
    }
}

async fn run_task_worker(
    media: MediaService,
    lfs_db: LfsDbStorage,
    scope: MediaScope,
    manifest_id: String,
    task_id: String,
    owner: String,
    emitter: crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
    bounds: FinalizeBounds,
) -> Result<(), MediaError> {
    let _permit = match finalizer_semaphore().acquire().await {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    let claimed = match media
        .paging()
        .claim_or_renew_lease(&task_id, &owner, None)
        .await
    {
        Ok(t) => t,
        Err(crate::jupiter::storage::media_paging_storage::MediaPagingError::Conflict(_)) => {
            // Another worker holds the lease.
            return Ok(());
        }
        Err(e) => return Err(map_paging(e)),
    };
    let epoch = claimed.lease_epoch;
    let progress = TaskProgressHandle {
        paging: media.paging().clone(),
        task_id: task_id.clone(),
        epoch,
    };

    let result = run_finalize_body(
        &media,
        &lfs_db,
        &scope,
        &manifest_id,
        unix_now(),
        &emitter,
        bounds,
        None,
        Some(&progress),
    )
    .await;

    match result {
        Ok(_) => {
            media
                .paging()
                .complete_task(&task_id, epoch)
                .await
                .map_err(map_paging)?;
            Ok(())
        }
        Err(err) => {
            let (code, retryable) = classify_finalize_error(&err);
            let _ = media
                .paging()
                .fail_task(&task_id, epoch, code, retryable)
                .await;
            Err(err)
        }
    }
}

struct TaskProgressHandle {
    paging: MediaPagingStorage,
    task_id: String,
    epoch: i64,
}

impl TaskProgressHandle {
    async fn update(&self, bytes: u64, pages: i32, stage: &str) -> Result<(), MediaError> {
        self.paging
            .update_task_progress(&self.task_id, self.epoch, bytes, pages, stage)
            .await
            .map_err(map_paging)?;
        Ok(())
    }
}

fn classify_finalize_error(err: &MediaError) -> (&'static str, bool) {
    match err {
        MediaError::Invalid(_) => ("validation_failed", false),
        MediaError::Conflict(_) => ("conflict", true),
        MediaError::NotFound => ("not_found", true),
        MediaError::Storage | MediaError::Io(_) => ("storage_error", true),
        MediaError::Json(_) => ("json_error", false),
        MediaError::TooManyRequests => ("queue_full", true),
    }
}

async fn run_finalize_body(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    scope: &MediaScope,
    manifest_id: &str,
    now: u64,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
    bounds: FinalizeBounds,
    mut cancel: Option<tokio::sync::watch::Receiver<bool>>,
    progress: Option<&TaskProgressHandle>,
) -> Result<ManifestResponse, MediaError> {
    let sealed = media.require_sealed_session(scope, manifest_id).await?;
    let pending = media
        .require_active_pending(scope, manifest_id, now)
        .await?;
    let manifest = pending.manifest;
    let expected_id = manifest.id().map_err(map_manifest)?;
    if expected_id != manifest_id {
        return Err(MediaError::Invalid(
            "pending manifest id does not match canonical id".to_string(),
        ));
    }
    if manifest.fallback_oid.as_ref() != Some(&manifest.media_oid) {
        return Err(MediaError::Invalid(
            "fallback_oid must equal media_oid".to_string(),
        ));
    }

    let tmp = tempfile::NamedTempFile::new().map_err(MediaError::Io)?;
    let tmp_path = tmp.path().to_path_buf();

    let verify = async {
        check_cancel(&mut cancel)?;
        rebuild_and_verify(
            media,
            scope,
            manifest_id,
            &manifest,
            sealed.page_count,
            &tmp_path,
            bounds,
            &mut cancel,
            progress,
        )
        .await?;
        check_cancel(&mut cancel)?;
        if let Some(p) = progress {
            p.update(manifest.media_size, sealed.page_count, "fallback")
                .await?;
        }
        put_fallback_from_path(media, lfs_db, &manifest, &tmp_path).await?;
        check_cancel(&mut cancel)?;
        if let Some(p) = progress {
            p.update(manifest.media_size, sealed.page_count, "publish")
                .await?;
        }
        publish_finalized(media, scope, manifest_id, &manifest, now, emitter).await
    }
    .await;

    // Always drop temp (success and failure). NamedTempFile closes on drop.
    drop(tmp);
    verify
}

fn check_cancel(cancel: &mut Option<tokio::sync::watch::Receiver<bool>>) -> Result<(), MediaError> {
    if let Some(rx) = cancel.as_mut()
        && *rx.borrow_and_update()
    {
        return Err(MediaError::Invalid("finalize cancelled".into()));
    }
    Ok(())
}

/// Page-ordered reassembly: continuous cover, per-chunk length/hash, full oid/size.
/// Does **not** require fresh CDC boundaries.
async fn rebuild_and_verify(
    media: &MediaService,
    scope: &MediaScope,
    manifest_id: &str,
    manifest: &MediaManifest,
    page_count: i32,
    path: &Path,
    bounds: FinalizeBounds,
    cancel: &mut Option<tokio::sync::watch::Receiver<bool>>,
    progress: Option<&TaskProgressHandle>,
) -> Result<(), MediaError> {
    let path_buf = path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || File::create(&path_buf))
        .await
        .map_err(|e| MediaError::Io(std::io::Error::other(e.to_string())))?
        .map_err(MediaError::Io)?;

    // Hold the file handle across async chunk reads via a blocking write channel.
    let (tx, rx) = std::sync::mpsc::channel::<Option<Bytes>>();
    let writer = tokio::task::spawn_blocking(move || -> Result<(u64, String), MediaError> {
        let mut file = file;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        while let Ok(Some(bytes)) = rx.recv() {
            file.write_all(&bytes).map_err(MediaError::Io)?;
            hasher.update(&bytes);
            written += bytes.len() as u64;
        }
        file.flush().map_err(MediaError::Io)?;
        Ok((written, hex::encode(hasher.finalize())))
    });

    let digest = scope.digest();
    let mut expected_offset = 0u64;
    let mut bytes_verified = 0u64;
    let mut last_progress = Instant::now();
    let mut last_renew = Instant::now();

    for page_no in 0..page_count {
        check_cancel(cancel)?;
        let entries = media
            .paging()
            .list_entries_for_page(&digest, manifest_id, page_no)
            .await
            .map_err(map_paging)?;

        for entry in &entries {
            check_cancel(cancel)?;
            if last_progress.elapsed() > bounds.no_progress {
                let _ = tx.send(None);
                let _ = writer.await;
                return Err(MediaError::Invalid(
                    "finalize no-progress deadline exceeded".into(),
                ));
            }

            let offset = u64::try_from(entry.offset)
                .map_err(|_| MediaError::Invalid("entry offset exceeds unsigned range".into()))?;
            let length = u64::try_from(entry.length)
                .map_err(|_| MediaError::Invalid("entry length exceeds unsigned range".into()))?;
            if offset != expected_offset {
                let _ = tx.send(None);
                let _ = writer.await;
                return Err(MediaError::Invalid(format!(
                    "chunk coverage gap or overlap at offset {offset} (expected {expected_offset})"
                )));
            }
            let next = expected_offset.checked_add(length).ok_or_else(|| {
                MediaError::Invalid("chunk offset overflow during finalize".into())
            })?;
            if next > manifest.media_size {
                let _ = tx.send(None);
                let _ = writer.await;
                return Err(MediaError::Invalid(
                    "chunk coverage overflows media_size".into(),
                ));
            }

            let read = tokio::time::timeout(
                bounds.io_timeout,
                media.read_chunk_for_entry(scope, &entry.chunk_hash, length),
            )
            .await
            .map_err(|_| MediaError::Invalid("chunk I/O timeout during finalize".into()))??;

            if tx.send(Some(read)).is_err() {
                let _ = writer.await;
                return Err(MediaError::Io(std::io::Error::other(
                    "finalize writer channel closed",
                )));
            }

            expected_offset = next;
            bytes_verified = expected_offset;
            last_progress = Instant::now();

            if let Some(p) = progress
                && last_renew.elapsed() >= bounds.lease_renew
            {
                p.update(bytes_verified, page_no + 1, "verify").await?;
                last_renew = Instant::now();
            }
        }

        if let Some(p) = progress {
            p.update(bytes_verified, page_no + 1, "verify").await?;
            last_renew = Instant::now();
        }
    }

    let _ = tx.send(None);
    let (written, digest_hex) = writer
        .await
        .map_err(|e| MediaError::Io(std::io::Error::other(e.to_string())))??;

    if expected_offset != manifest.media_size || written != manifest.media_size {
        return Err(MediaError::Invalid(format!(
            "chunk coverage incomplete: covered {expected_offset}, media_size {}",
            manifest.media_size
        )));
    }
    if digest_hex != manifest.media_oid {
        return Err(MediaError::Invalid(
            "reassembled media SHA-256 does not match the manifest oid".to_string(),
        ));
    }
    Ok(())
}

async fn put_fallback_from_path(
    media: &MediaService,
    lfs_db: &LfsDbStorage,
    manifest: &MediaManifest,
    path: &Path,
) -> Result<(), MediaError> {
    let key = ObjectKey {
        namespace: ObjectNamespace::Lfs,
        key: manifest.media_oid.clone(),
    };
    let file = tokio::fs::File::open(path).await?;
    let stream: ObjectByteStream = Box::pin(ReaderStream::new(file));
    let meta = ObjectMeta {
        size: manifest.media_size as i64,
        ..ObjectMeta::default()
    };
    media
        .object_store()
        .inner
        .put_stream_bounded(&key, stream, meta)
        .await
        .map_err(map_store)?;

    lfs_db
        .new_lfs_object(lfs_objects::Model {
            oid: manifest.media_oid.clone(),
            size: manifest.media_size as i64,
            exist: true,
        })
        .await
        .map_err(map_db)?;

    let stored = lfs_db
        .get_lfs_object(&manifest.media_oid)
        .await
        .map_err(map_db)?
        .ok_or(MediaError::NotFound)?;
    if stored.oid != manifest.media_oid
        || stored.size != manifest.media_size as i64
        || !stored.exist
    {
        return Err(MediaError::Conflict(
            "lfs_objects metadata does not match the published fallback".to_string(),
        ));
    }
    if !media.exists(&key).await? {
        return Err(MediaError::NotFound);
    }
    Ok(())
}

async fn publish_finalized(
    media: &MediaService,
    scope: &MediaScope,
    manifest_id: &str,
    manifest: &MediaManifest,
    now: u64,
    emitter: &crate::jupiter::service::storage_event_emitter::StorageEventEmitter,
) -> Result<ManifestResponse, MediaError> {
    let response = ManifestResponse {
        manifest_id: manifest_id.to_string(),
        manifest: manifest.clone(),
    };
    let key = scope_key(scope, MediaObjectKind::Finalized, &manifest.media_oid)?;
    if media.exists(&key).await? {
        let existing = media.read_bytes(&key, MAX_ENVELOPE_SIZE).await?;
        let parsed: ManifestResponse =
            serde_json::from_slice(&existing).map_err(|e| MediaError::Json(e.to_string()))?;
        if parsed.manifest_id != manifest_id || parsed.manifest.media_oid != manifest.media_oid {
            return Err(MediaError::Conflict(
                "finalized manifest does not match media oid or scope session".to_string(),
            ));
        }
        return Ok(parsed);
    }
    let payload =
        Bytes::from(serde_json::to_vec(&response).map_err(|e| MediaError::Json(e.to_string()))?);
    if payload.len() > MAX_ENVELOPE_SIZE {
        return Err(MediaError::Invalid(
            "finalized manifest exceeds size limit".to_string(),
        ));
    }
    media.put_bytes(&key, payload).await?;

    // WH-06 (plan-20260912 / ADR-WH-04/05): exactly one `lfs.media.finalized`
    // when THIS finalize actually put the finalized manifest (the exists
    // no-op above returns before this point; intermediate chunk/fallback
    // steps never emit). Scope metadata comes from the server-side
    // MediaScope's canonical repository, never from the request body; the
    // actor never leaves the process. A repeat cross-process finalize may
    // still rewrite the fallback and re-notify (no uniqueness lock added).
    let event = crate::jupiter::service::storage_event::CommittedEvent {
        event_id: uuid::Uuid::new_v4(),
        event_type: crate::jupiter::service::storage_event::EventType::LfsMediaFinalized,
        occurred_at: now,
        source: crate::jupiter::service::storage_event::EventSource::Lfs,
        scope: crate::jupiter::service::storage_event::EventScope::Media {
            repo_path: scope.repository().to_owned(),
        },
        data: crate::jupiter::service::storage_event::EventData::LfsMediaFinalized {
            oid: manifest.media_oid.clone(),
            size: manifest.media_size,
            manifest_id: manifest_id.to_owned(),
        },
    };
    let _ = emitter.try_emit(event);
    Ok(response)
}

fn map_manifest(err: ManifestError) -> MediaError {
    match err {
        ManifestError::Invalid(msg) => MediaError::Invalid(msg),
        ManifestError::Serde(msg) => MediaError::Json(msg),
    }
}

fn map_paging(err: crate::jupiter::storage::media_paging_storage::MediaPagingError) -> MediaError {
    match err {
        crate::jupiter::storage::media_paging_storage::MediaPagingError::NotFound => {
            MediaError::NotFound
        }
        crate::jupiter::storage::media_paging_storage::MediaPagingError::Conflict(msg) => {
            MediaError::Conflict(msg)
        }
        crate::jupiter::storage::media_paging_storage::MediaPagingError::Storage(_) => {
            MediaError::Storage
        }
        crate::jupiter::storage::media_paging_storage::MediaPagingError::StaleLease => {
            MediaError::Conflict("stale media lease".into())
        }
    }
}

fn map_db(err: crate::common::errors::MegaError) -> MediaError {
    let _ = redact_storage_error(&err);
    MediaError::Storage
}

/// Test helper: put P-01a pages for a prepared manifest and seal.
pub async fn put_pages_and_seal(
    media: &MediaService,
    scope: &MediaScope,
    manifest: &MediaManifest,
    manifest_id: &str,
    now: u64,
) -> Result<(), MediaError> {
    use crate::ceres::lfs::media::protocol::{ManifestPage, split_pages};
    let pages = split_pages(&manifest.chunks).map_err(map_manifest)?;
    for (page_no, entries) in pages.into_iter().enumerate() {
        media
            .put_page_at(
                scope,
                manifest_id,
                ManifestPage {
                    page_no: page_no as u32,
                    entries,
                },
                now,
            )
            .await?;
    }
    media.seal_at(scope, manifest_id, now).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::{
        ceres::lfs::media::{
            chunker,
            protocol::{ChunkEntry, CreatedBy},
            service::MediaService,
        },
        jupiter::{
            service::storage_event_emitter::StorageEventEmitter,
            storage::{
                base_storage::{BaseStorage, StorageConnector},
                lfs_db_storage::LfsDbStorage,
                object_storage::build_object_storage,
            },
            tests::test_db_connection,
        },
        orbit_api::factory::{LocalConfig, ObjectStorageBackend, ObjectStorageConfig},
    };

    /// Recording fake transport (WH-09 seam): exact bodies + call counter.
    #[derive(Default)]
    struct RecordingTransport {
        calls: std::sync::atomic::AtomicUsize,
        bodies: std::sync::Mutex<Vec<Bytes>>,
        fail: bool,
    }

    impl RecordingTransport {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<Bytes> {
            self.bodies.lock().expect("bodies").clone()
        }
    }

    impl crate::jupiter::service::storage_event_transport::EventTransport for RecordingTransport {
        fn post(
            &self,
            _target: &crate::jupiter::service::storage_event_transport::EventTarget,
            body: Bytes,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::jupiter::service::storage_event_transport::TransportSuccess,
                            crate::jupiter::service::storage_event_transport::TransportError,
                        >,
                    > + Send,
            >,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.bodies.lock().expect("bodies").push(body);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(crate::jupiter::service::storage_event_transport::TransportError::Timeout)
                } else {
                    Ok(
                        crate::jupiter::service::storage_event_transport::TransportSuccess::Accepted2xx {
                            status: 200,
                        },
                    )
                }
            })
        }
    }

    const WH06_INSTALLATION: &str = "it-wh06";

    fn wh06_target(
        id: &str,
        lfs_paths: Vec<String>,
    ) -> (
        crate::config::StorageEventsTargetConfig,
        crate::jupiter::service::storage_event_transport::EventTarget,
    ) {
        let config = crate::config::StorageEventsTargetConfig {
            id: id.to_owned(),
            url: "https://events.example.invalid/ingest".to_owned(),
            secret_ref: format!("vault://secret/config/it/storage_events/targets/{id}/hmac#value"),
            events: vec!["lfs.media.finalized".to_owned()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths,
            include_unscoped_lfs: false,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        };
        let secret = crate::config::secret::SecretString::new(
            "hex:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let compiled = crate::jupiter::service::storage_event_transport::EventTarget::compile(
            &config.id,
            &config.url,
            &secret,
        )
        .expect("compile target");
        (config, compiled)
    }

    fn wh06_emitter(
        transport: std::sync::Arc<RecordingTransport>,
        targets: Vec<(
            crate::config::StorageEventsTargetConfig,
            crate::jupiter::service::storage_event_transport::EventTarget,
        )>,
    ) -> StorageEventEmitter {
        let mut config =
            crate::config::testing::isolated_config(std::env::temp_dir().join("wh06-finalize"));
        config.monorepo.push_policy = crate::config::PushPolicy::Trunk;
        config.git.push_auth = Some(crate::config::PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some(WH06_INSTALLATION.to_owned());
        StorageEventEmitter::new_with_transport(&config, transport, targets)
    }

    struct Wh06Fixture {
        _obj_dir: tempfile::TempDir,
        _db_dir: tempfile::TempDir,
        service: MediaService,
        lfs_db: LfsDbStorage,
        scope: MediaScope,
    }

    async fn wh06_fixture(repo: &str) -> Wh06Fixture {
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
        crate::jupiter::migration::apply_migrations(&db, true)
            .await
            .unwrap();
        let lfs_db = LfsDbStorage {
            base: BaseStorage::new(std::sync::Arc::new(db)),
        };
        let paging = crate::jupiter::storage::media_paging_storage::MediaPagingStorage::new(
            lfs_db.base.clone(),
        );
        Wh06Fixture {
            _obj_dir: obj_dir,
            _db_dir: db_dir,
            service: MediaService::new(store, paging),
            lfs_db,
            scope: MediaScope::from_server("user-1", repo).unwrap(),
        }
    }

    fn wh06_manifest(data: &[u8]) -> (MediaManifest, Vec<(String, Bytes)>) {
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
            algorithm: chunker::ALGORITHM.to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid: LfsDigest::sha256_of(data).hex().to_owned(),
            media_size: data.len() as u64,
            chunks,
            created_by: CreatedBy {
                client: "test".to_string(),
                version: "0".to_string(),
                capabilities: vec![chunker::ALGORITHM.to_string()],
            },
            fallback_oid: None,
        };
        (manifest, bodies)
    }

    /// Manual non-cold-cut layout: two MIN_SIZE chunks (FastCDC may differ).
    fn non_cold_cut_manifest(parts: &[&[u8]]) -> (MediaManifest, Vec<(String, Bytes)>) {
        use bytes::BytesMut;
        let mut offset = 0u64;
        let mut chunks = Vec::new();
        let mut bodies = Vec::new();
        let mut all = BytesMut::new();
        let n = parts.len();
        for (i, part) in parts.iter().enumerate() {
            let is_tail = i + 1 == n;
            let declared = if is_tail {
                part.len()
            } else {
                part.len().max(chunker::MIN_SIZE)
            };
            let mut body = BytesMut::from(*part);
            if body.len() < declared {
                body.resize(declared, 0);
            }
            all.extend_from_slice(&body);
            let hash = LfsDigest::sha256_of(&body).hex().to_owned();
            chunks.push(ChunkEntry {
                offset,
                length: declared as u64,
                chunk_hash: hash.clone(),
                encoded_length: declared as u64,
                compression: "none".to_string(),
                checksum: None,
            });
            bodies.push((hash, body.freeze()));
            offset += declared as u64;
        }
        let media_oid = LfsDigest::sha256_of(&all).hex().to_owned();
        let manifest = MediaManifest {
            version: 1,
            algorithm: chunker::ALGORITHM.to_string(),
            hash_algorithm: "sha256".to_string(),
            media_oid,
            media_size: offset,
            chunks,
            created_by: CreatedBy {
                client: "test".to_string(),
                version: "0".to_string(),
                capabilities: vec![chunker::ALGORITHM.to_string()],
            },
            fallback_oid: None,
        };
        (manifest, bodies)
    }

    async fn wh06_prepare(fx: &Wh06Fixture, data: &[u8], now: u64) -> (MediaManifest, String) {
        let (manifest, bodies) = wh06_manifest(data);
        let prepared = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), now)
            .await
            .unwrap();
        for (hash, body) in &bodies {
            fx.service
                .upload_chunk_at(&fx.scope, &prepared.manifest_id, hash, body.clone(), now)
                .await
                .unwrap();
        }
        put_pages_and_seal(
            &fx.service,
            &fx.scope,
            &manifest,
            &prepared.manifest_id,
            now,
        )
        .await
        .unwrap();
        (manifest, prepared.manifest_id)
    }

    async fn wh06_wait_calls(transport: &RecordingTransport, n: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if transport.calls() >= n {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("delivery within 2s");
    }

    #[tokio::test]
    async fn non_cold_cut_layout_passes_reassembly() {
        let fx = wh06_fixture("/acme/app").await;
        let (manifest, bodies) = non_cold_cut_manifest(&[b"alpha-payload", b"beta-tail"]);
        // Two padded MIN_SIZE chunks: a legal layout that need not equal cold-cut.
        let mut full = Vec::new();
        for (_, b) in &bodies {
            full.extend_from_slice(b);
        }
        let cdc = chunker::chunk_bytes(&full);
        let differs = cdc.len() != manifest.chunks.len()
            || cdc
                .iter()
                .zip(manifest.chunks.iter())
                .any(|(a, b)| a.offset != b.offset || a.length != b.length);
        assert!(
            differs,
            "use a layout FastCDC would not emit so MF-03's removed cold-cut gate is exercised"
        );

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
        put_pages_and_seal(&fx.service, &fx.scope, &manifest, &prepared.manifest_id, 10)
            .await
            .unwrap();
        let published = finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &prepared.manifest_id,
            11,
            &StorageEventEmitter::disabled(),
        )
        .await
        .expect("non-cold-cut layout must finalize without fresh CDC equality");
        assert_eq!(published.manifest.media_oid, manifest.media_oid);
    }

    #[tokio::test]
    async fn coverage_gap_rejects_without_publish() {
        let fx = wh06_fixture("/acme/app").await;
        let data = b"gap-coverage-object";
        let (manifest, manifest_id) = wh06_prepare(&fx, data, 10).await;
        // Corrupt sealed index: shift second entry (or sole entry) to open a gap.
        let entries = fx
            .service
            .paging()
            .list_entries(&fx.scope.digest(), &manifest_id)
            .await
            .unwrap();
        if entries.is_empty() {
            // Empty object: inject a spurious entry that overflows.
            use crate::jupiter::storage::media_paging_storage::ChunkIndexRow;
            fx.service
                .paging()
                .rebuild_index_from_pages(
                    &fx.scope.digest(),
                    &manifest_id,
                    &[(
                        0,
                        vec![ChunkIndexRow {
                            page_no: 0,
                            ordinal: 0,
                            offset: 1,
                            length: 1,
                            chunk_hash: "a".repeat(64),
                        }],
                    )],
                )
                .await
                .unwrap();
        } else {
            let mut rebuilt = Vec::new();
            for e in &entries {
                rebuilt.push((
                    e.page_no,
                    crate::jupiter::storage::media_paging_storage::ChunkIndexRow {
                        page_no: e.page_no,
                        ordinal: e.ordinal,
                        offset: if e.ordinal == 0 && e.page_no == 0 {
                            1
                        } else {
                            e.offset as u64
                        },
                        length: e.length as u64,
                        chunk_hash: e.chunk_hash.clone(),
                    },
                ));
            }
            // Group by page.
            let mut pages: std::collections::BTreeMap<i32, Vec<_>> =
                std::collections::BTreeMap::new();
            for (pn, row) in rebuilt {
                pages.entry(pn).or_default().push(row);
            }
            let pages: Vec<_> = pages.into_iter().collect();
            fx.service
                .paging()
                .rebuild_index_from_pages(&fx.scope.digest(), &manifest_id, &pages)
                .await
                .unwrap();
        }
        let err = finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &manifest_id,
            11,
            &StorageEventEmitter::disabled(),
        )
        .await
        .expect_err("gap must fail");
        assert!(matches!(err, MediaError::Invalid(_)), "{err:?}");
        let lfs_key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: manifest.media_oid.clone(),
        };
        assert!(!fx.service.exists(&lfs_key).await.unwrap());
    }

    #[tokio::test]
    async fn bad_hash_and_wrong_oid_reject() {
        let fx = wh06_fixture("/acme/app").await;
        let (manifest, bodies) = wh06_manifest(b"bad-hash-object");
        let prepared = fx
            .service
            .prepare_at(&fx.scope, manifest.clone(), 20)
            .await
            .unwrap();
        for (hash, body) in &bodies {
            fx.service
                .upload_chunk_at(&fx.scope, &prepared.manifest_id, hash, body.clone(), 20)
                .await
                .unwrap();
        }
        put_pages_and_seal(&fx.service, &fx.scope, &manifest, &prepared.manifest_id, 20)
            .await
            .unwrap();
        if let Some((hash, _)) = bodies.first() {
            fx.service
                .overwrite_chunk_for_test(&fx.scope, hash, Bytes::from_static(b"tampered"))
                .await
                .unwrap();
        }
        assert!(
            finalize_at(
                &fx.service,
                &fx.lfs_db,
                &fx.scope,
                &prepared.manifest_id,
                21,
                &StorageEventEmitter::disabled(),
            )
            .await
            .is_err()
        );

        // Wrong oid: reassemble ok but claim different oid via pending mutate is
        // hard; instead finalize a layout whose media_oid does not match bytes.
        let fx2 = wh06_fixture("/acme/app").await;
        let (mut bad_oid, bodies2) = non_cold_cut_manifest(&[b"wrong-oid-tail"]);
        bad_oid.media_oid = "f".repeat(64);
        bad_oid.fallback_oid = None;
        // prepare forces fallback_oid = media_oid and validate fails on size/oid
        // mismatch with chunks — so craft matching size but wrong oid hash.
        // media_size still matches bytes; only oid is wrong.
        let prepared2 = fx2
            .service
            .prepare_at(&fx2.scope, bad_oid.clone(), 30)
            .await;
        // prepare validates oid is hex but not that it matches content.
        // MediaManifest::validate does NOT check oid == hash(content).
        if let Ok(prep) = prepared2 {
            for (hash, body) in &bodies2 {
                fx2.service
                    .upload_chunk_at(&fx2.scope, &prep.manifest_id, hash, body.clone(), 30)
                    .await
                    .unwrap();
            }
            put_pages_and_seal(&fx2.service, &fx2.scope, &bad_oid, &prep.manifest_id, 30)
                .await
                .unwrap();
            let err = finalize_at(
                &fx2.service,
                &fx2.lfs_db,
                &fx2.scope,
                &prep.manifest_id,
                31,
                &StorageEventEmitter::disabled(),
            )
            .await
            .expect_err("wrong oid");
            assert!(
                matches!(err, MediaError::Invalid(ref msg) if msg.contains("SHA-256")),
                "{err:?}"
            );
        }
    }

    #[tokio::test]
    async fn cancel_releases_temp_and_lease() {
        let fx = wh06_fixture("/acme/app").await;
        let (manifest, manifest_id) = wh06_prepare(&fx, b"cancel-object", 40).await;
        let (tx, rx) = tokio::sync::watch::channel(false);
        let media = fx.service.clone();
        let lfs = fx.lfs_db.clone();
        let scope = fx.scope.clone();
        let mid = manifest_id.clone();
        let handle = tokio::spawn(async move {
            finalize_at_with_bounds(
                &media,
                &lfs,
                &scope,
                &mid,
                41,
                &StorageEventEmitter::disabled(),
                FinalizeBounds {
                    no_progress: Duration::from_secs(600),
                    lease_renew: Duration::from_secs(20),
                    io_timeout: Duration::from_secs(120),
                },
                Some(rx),
            )
            .await
        });
        // Cancel before/during work.
        let _ = tx.send(true);
        let err = handle.await.unwrap().expect_err("cancelled");
        assert!(
            matches!(err, MediaError::Invalid(ref msg) if msg.contains("cancelled")),
            "{err:?}"
        );
        let lfs_key = ObjectKey {
            namespace: ObjectNamespace::Lfs,
            key: manifest.media_oid.clone(),
        };
        assert!(!fx.service.exists(&lfs_key).await.unwrap());

        // Persistent cancel_task clears lease.
        let task = fx
            .service
            .paging()
            .ensure_task(&fx.scope.digest(), &manifest_id)
            .await
            .unwrap();
        let claimed = fx
            .service
            .paging()
            .claim_or_renew_lease(&task.task_id, "w", None)
            .await
            .unwrap();
        assert!(claimed.lease_owner.is_some());
        let cancelled = fx
            .service
            .paging()
            .cancel_task(&task.task_id)
            .await
            .unwrap();
        assert_eq!(cancelled.state, TASK_FAILED);
        assert_eq!(cancelled.error_code.as_deref(), Some("cancelled"));
        assert!(cancelled.lease_owner.is_none());
    }

    #[tokio::test]
    async fn no_progress_deadline_is_bounded() {
        let fx = wh06_fixture("/acme/app").await;
        let (manifest, manifest_id) = wh06_prepare(&fx, b"deadline-object", 50).await;
        // Delete all chunks so each read fails quickly — but use a short
        // no-progress window with a hanging read via missing object.
        for c in &manifest.chunks {
            let key = scope_key(&fx.scope, MediaObjectKind::Chunk, &c.chunk_hash).unwrap();
            let _ = fx.service.object_store().inner.delete(&key).await;
        }
        let err = finalize_at_with_bounds(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &manifest_id,
            51,
            &StorageEventEmitter::disabled(),
            FinalizeBounds {
                no_progress: Duration::from_millis(50),
                lease_renew: Duration::from_millis(10),
                io_timeout: Duration::from_millis(20),
            },
            None,
        )
        .await
        .expect_err("must fail");
        // Either I/O timeout, not found, or no-progress — all bounded failures.
        assert!(
            matches!(
                err,
                MediaError::Invalid(_) | MediaError::NotFound | MediaError::Conflict(_)
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn async_enqueue_and_poll_completes() {
        let fx = wh06_fixture("/acme/app").await;
        let now = unix_now();
        let (manifest, manifest_id) = wh06_prepare(&fx, b"async-enqueue-object", now).await;
        let accepted = enqueue_finalize(
            fx.service.clone(),
            fx.lfs_db.clone(),
            fx.scope.clone(),
            manifest_id.clone(),
            StorageEventEmitter::disabled(),
        )
        .await
        .unwrap();
        assert_eq!(accepted.manifest_id, manifest_id);
        assert!(accepted.status_url.contains(&accepted.task_id));

        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let st = task_status(&fx.service, &fx.scope, &accepted.task_id)
                    .await
                    .unwrap();
                if st.state == TASK_COMPLETE || st.state == TASK_FAILED {
                    return st;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("task finish");
        assert_eq!(
            status.state, TASK_COMPLETE,
            "async finalize completes: stage={} err={:?} retryable={}",
            status.stage, status.error_code, status.retryable
        );
        assert_eq!(status.oid.as_deref(), Some(manifest.media_oid.as_str()));
        assert_eq!(status.size, Some(manifest.media_size));
    }

    #[tokio::test]
    async fn storage_event_finalize_matrix() {
        let transport = std::sync::Arc::new(RecordingTransport::default());
        let emitter = wh06_emitter(
            transport.clone(),
            vec![wh06_target("ops-main", vec!["/".to_owned()])],
        );
        let fx = wh06_fixture("/acme/app").await;
        let data = b"wh06 finalize matrix object";
        let (manifest, manifest_id) = wh06_prepare(&fx, data, 10).await;
        let published = finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &manifest_id,
            11,
            &emitter,
        )
        .await
        .expect("finalize");
        assert_eq!(published.manifest.media_oid, manifest.media_oid);
        wh06_wait_calls(&transport, 1).await;
        let bodies = transport.bodies();
        assert_eq!(bodies.len(), 1, "exactly one finalized event");
        let envelope: serde_json::Value = serde_json::from_slice(&bodies[0]).expect("envelope");
        assert_eq!(envelope["schema_version"], 1);
        assert_eq!(envelope["event_type"], "lfs.media.finalized");
        assert_eq!(envelope["source"], "lfs");
        assert_eq!(envelope["scope"]["repo_path"], "/acme/app");
        assert!(envelope["scope"]["tenant_id"].is_null());
        assert!(envelope["scope"]["oci_repository"].is_null());
        assert_eq!(envelope["data"]["oid"], manifest.media_oid);
        assert_eq!(
            envelope["data"]["size"].as_u64().expect("size"),
            manifest.media_size
        );
        assert_eq!(envelope["data"]["manifest_id"], manifest_id);
        assert_eq!(envelope["data"]["transfer"], "fastcdc");
        assert_eq!(envelope["occurred_at"].as_u64().expect("ts"), 11);

        let again = finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &manifest_id,
            12,
            &emitter,
        )
        .await
        .expect("repeat finalize is a no-op");
        assert_eq!(again.manifest_id, manifest_id);
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "repeat finalize delivers nothing");

        let (manifest2, manifest_id2) = wh06_prepare(&fx, b"wh06 corrupt", 20).await;
        if let Some((hash, _)) = manifest2.chunks.first().map(|c| (c.chunk_hash.clone(), ())) {
            fx.service
                .overwrite_chunk_for_test(&fx.scope, &hash, Bytes::from_static(b"tampered"))
                .await
                .unwrap();
        }
        let failed = finalize_at(
            &fx.service,
            &fx.lfs_db,
            &fx.scope,
            &manifest_id2,
            21,
            &emitter,
        )
        .await;
        assert!(failed.is_err(), "corrupt chunk must fail finalize");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(transport.calls(), 1, "failed finalize delivers nothing");

        tokio::time::timeout(std::time::Duration::from_secs(3), emitter.shutdown())
            .await
            .expect("shutdown within 3s");
        assert_eq!(transport.calls(), 1, "no late delivery after drain");

        let other_transport = std::sync::Arc::new(RecordingTransport::default());
        let other_emitter = wh06_emitter(
            other_transport.clone(),
            vec![wh06_target("ops-other", vec!["/other/repo".to_owned()])],
        );
        let fx2 = wh06_fixture("/acme/app").await;
        let (_manifest3, manifest_id3) = wh06_prepare(&fx2, b"wh06 isolation", 30).await;
        finalize_at(
            &fx2.service,
            &fx2.lfs_db,
            &fx2.scope,
            &manifest_id3,
            31,
            &other_emitter,
        )
        .await
        .expect("finalize");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            other_transport.calls(),
            0,
            "a non-matching lfs_paths filter receives nothing"
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), other_emitter.shutdown())
            .await
            .expect("shutdown within 3s");
        assert_eq!(other_transport.calls(), 0);

        let failing = std::sync::Arc::new(RecordingTransport {
            fail: true,
            ..Default::default()
        });
        let failing_emitter = wh06_emitter(
            failing.clone(),
            vec![wh06_target("ops-main", vec!["/".to_owned()])],
        );
        let fx3 = wh06_fixture("/acme/app").await;
        let (manifest4, manifest_id4) = wh06_prepare(&fx3, b"wh06 failing emitter", 40).await;
        let published = finalize_at(
            &fx3.service,
            &fx3.lfs_db,
            &fx3.scope,
            &manifest_id4,
            41,
            &failing_emitter,
        )
        .await
        .expect("finalize succeeds despite emitter failure");
        assert_eq!(published.manifest.media_oid, manifest4.media_oid);
        wh06_wait_calls(&failing, 1).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            failing_emitter.shutdown(),
        )
        .await
        .expect("shutdown within 3s");
        assert_eq!(failing.calls(), 1);
    }
}
