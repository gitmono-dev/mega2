//! Default-off typed projection observations. Buffer capacity is reserved
//! before serialization; logger filters and HTTP response bodies are unchanged.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::projection_observation::NativeProjectionObservation;

pub(crate) const RECORD_BYTES: usize = 32 * 1024;
pub(crate) const RECORD_LIMIT: usize = 64;
const STATUS_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum WriterFailure {
    QueueSaturated = 1,
    WriterUnavailable,
    RecordTooLarge,
    RecordLimitExceeded,
    ObservationBindingRejected,
    IoFailure,
    WorkerPanic,
    DrainTimeout,
}

impl WriterFailure {
    fn label(self) -> &'static str {
        match self {
            Self::QueueSaturated => "QUEUE_SATURATED",
            Self::WriterUnavailable => "WRITER_UNAVAILABLE",
            Self::RecordTooLarge => "RECORD_TOO_LARGE",
            Self::RecordLimitExceeded => "RECORD_LIMIT_EXCEEDED",
            Self::ObservationBindingRejected => "OBSERVATION_BINDING_REJECTED",
            Self::IoFailure => "IO_FAILURE",
            Self::WorkerPanic => "WORKER_PANIC",
            Self::DrainTimeout => "DRAIN_TIMEOUT",
        }
    }
}

struct Health {
    first_error: AtomicU8,
    accepted: AtomicU64,
}

impl Health {
    fn fail(&self, error: WriterFailure) {
        if self
            .first_error
            .compare_exchange(0, error as u8, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            tracing::warn!(target: "mst2::projection_writer", failure_code = error.label(), "projection observation delivery failed");
        }
    }
}

// Exactly 64 preallocated payload slots exist, including the slot currently
// being serialized and the worker's slot. No serialize-to-Vec-then-check path.
struct RecordBuffer<const CAPACITY: usize = RECORD_BYTES> {
    bytes: Box<[u8; CAPACITY]>,
    len: usize,
}

impl<const CAPACITY: usize> Write for RecordBuffer<CAPACITY> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .len
            .checked_add(bytes.len())
            .filter(|end| *end <= CAPACITY)
            .ok_or_else(|| io::Error::other("projection record capacity exhausted"))?;
        self.bytes[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct QueuedRecord {
    sequence: u64,
    buffer: RecordBuffer,
}
type Pool = Arc<Mutex<Vec<RecordBuffer>>>;
struct Producer {
    sender: Option<SyncSender<QueuedRecord>>,
}

pub(crate) struct ProjectionObservationSink {
    id: Uuid,
    producer: Mutex<Producer>,
    pool: Pool,
    health: Arc<Health>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ProjectionObservationSink {
    pub(crate) fn start(cache: &Path) -> io::Result<Arc<Self>> {
        let id = Uuid::new_v4();
        let root = prepare_directory(cache, id)?;
        let records = create_private(&root.join("records.jsonl"))?;
        let health = Arc::new(Health {
            first_error: AtomicU8::new(0),
            accepted: AtomicU64::new(0),
        });
        let pool = Arc::new(Mutex::new(
            (0..RECORD_LIMIT)
                .map(|_| RecordBuffer {
                    bytes: Box::new([0; RECORD_BYTES]),
                    len: 0,
                })
                .collect::<Vec<_>>(),
        ));
        let mut writer = FileWriter::new(root, records, id, health.clone());
        writer.status(false)?;
        let (sender, receiver) = mpsc::sync_channel(RECORD_LIMIT);
        let worker_pool = pool.clone();
        let worker_health = health.clone();
        let worker = thread::Builder::new()
            .name("mst2-projection-writer".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    writer.run(receiver, worker_pool);
                }));
                if outcome.is_err() {
                    worker_health.fail(WriterFailure::WorkerPanic);
                    let _ = writer.status(true);
                }
            })?;
        tracing::info!(target: "mst2::projection_writer", writer_revision = 1u16, sink_instance = %id,
                       payload_slots = RECORD_LIMIT, payload_slot_bytes = RECORD_BYTES,
                       "typed projection writer started");
        Ok(Arc::new(Self {
            id,
            producer: Mutex::new(Producer {
                sender: Some(sender),
            }),
            pool,
            health,
            worker: Mutex::new(Some(worker)),
        }))
    }

    pub(crate) fn reject_binding(&self) {
        self.health.fail(WriterFailure::ObservationBindingRejected);
    }

    pub(crate) fn enqueue(
        &self,
        observation: &NativeProjectionObservation,
    ) -> Result<(), WriterFailure> {
        let result = self.enqueue_inner(observation);
        if let Err(error) = result {
            self.health.fail(error);
        }
        result
    }

    fn enqueue_inner(
        &self,
        observation: &NativeProjectionObservation,
    ) -> Result<(), WriterFailure> {
        if self.health.first_error.load(Ordering::SeqCst) != 0 {
            return Err(WriterFailure::WriterUnavailable);
        }
        let producer = self
            .producer
            .try_lock()
            .map_err(|_| WriterFailure::QueueSaturated)?;
        let sender = producer
            .sender
            .as_ref()
            .ok_or(WriterFailure::WriterUnavailable)?;
        let sequence = self.health.accepted.load(Ordering::SeqCst) + 1;
        if sequence > RECORD_LIMIT as u64 {
            return Err(WriterFailure::RecordLimitExceeded);
        }
        let mut buffer = self
            .pool
            .try_lock()
            .map_err(|_| WriterFailure::QueueSaturated)?
            .pop()
            .ok_or(WriterFailure::QueueSaturated)?;
        buffer.len = 0;
        let serialized = (|| -> io::Result<()> {
            write!(
                buffer,
                "{{\"writer_revision\":1,\"sink_instance\":\"{}\",\"record_sequence\":{},\"payload\":",
                self.id, sequence
            )?;
            let begin = buffer.len;
            serde_json::to_writer(&mut buffer, &observation.wire_record())
                .map_err(io::Error::other)?;
            let digest = Sha256::digest(&buffer.bytes[begin..buffer.len]);
            writeln!(
                buffer,
                ",\"payload_sha256\":\"sha256:{}\"}}",
                hex::encode(digest)
            )
        })();
        if serialized.is_err() {
            self.return_slot(buffer);
            return Err(WriterFailure::RecordTooLarge);
        }
        self.health.accepted.store(sequence, Ordering::SeqCst);
        match sender.try_send(QueuedRecord { sequence, buffer }) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.health.accepted.store(sequence - 1, Ordering::SeqCst);
                let (error, record) = match error {
                    TrySendError::Full(record) => (WriterFailure::QueueSaturated, record),
                    TrySendError::Disconnected(record) => {
                        (WriterFailure::WriterUnavailable, record)
                    }
                };
                self.return_slot(record.buffer);
                Err(error)
            }
        }
    }

    fn return_slot(&self, mut buffer: RecordBuffer) {
        buffer.len = 0;
        if let Ok(mut pool) = self.pool.lock() {
            pool.push(buffer);
        }
    }

    pub(crate) async fn shutdown(&self, deadline: Instant) -> Result<(), WriterFailure> {
        self.producer
            .lock()
            .map_err(|_| WriterFailure::WriterUnavailable)?
            .sender
            .take();
        loop {
            let finished = self
                .worker
                .lock()
                .map_err(|_| WriterFailure::WriterUnavailable)?
                .as_ref()
                .is_none_or(JoinHandle::is_finished);
            if finished {
                break;
            }
            if Instant::now() >= deadline {
                self.health.fail(WriterFailure::DrainTimeout);
                return Err(WriterFailure::DrainTimeout);
            }
            tokio::time::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            )
            .await;
        }
        if let Some(worker) = self
            .worker
            .lock()
            .map_err(|_| WriterFailure::WriterUnavailable)?
            .take()
            && worker.join().is_err()
        {
            self.health.fail(WriterFailure::WorkerPanic);
        }
        if self.health.first_error.load(Ordering::SeqCst) != 0 {
            return Err(WriterFailure::WriterUnavailable);
        }
        if Instant::now() >= deadline {
            self.health.fail(WriterFailure::DrainTimeout);
            return Err(WriterFailure::DrainTimeout);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn test_directory(&self, cache: &Path) -> PathBuf {
        cache
            .join("logs/mst2-native-projection")
            .join(self.id.to_string())
    }
}

#[derive(Serialize)]
struct WriterStatus {
    writer_revision: u16,
    sink_instance: String,
    accepted_records: u64,
    written_sequence: u64,
    written_records: u64,
    written_bytes: u64,
    rolling_sha256: String,
    first_error_code: u8,
    closed: bool,
}

struct FileWriter {
    root: PathBuf,
    records: File,
    id: Uuid,
    health: Arc<Health>,
    sequence: u64,
    bytes: u64,
    digest: Sha256,
}
impl FileWriter {
    fn new(root: PathBuf, records: File, id: Uuid, health: Arc<Health>) -> Self {
        Self {
            root,
            records,
            id,
            health,
            sequence: 0,
            bytes: 0,
            digest: Sha256::new(),
        }
    }
    fn status(&self, closed: bool) -> io::Result<()> {
        let status = WriterStatus {
            writer_revision: 1,
            sink_instance: self.id.to_string(),
            accepted_records: self.health.accepted.load(Ordering::SeqCst),
            written_sequence: self.sequence,
            written_records: self.sequence,
            written_bytes: self.bytes,
            rolling_sha256: format!("sha256:{}", hex::encode(self.digest.clone().finalize())),
            first_error_code: self.health.first_error.load(Ordering::SeqCst),
            closed,
        };
        let mut buffer = RecordBuffer::<STATUS_BYTES> {
            bytes: Box::new([0; STATUS_BYTES]),
            len: 0,
        };
        serde_json::to_writer(&mut buffer, &status).map_err(io::Error::other)?;
        let temporary = self.root.join("status.tmp");
        let mut file = create_private(&temporary)?;
        file.write_all(&buffer.bytes[..buffer.len])?;
        file.sync_all()?;
        fs::rename(temporary, self.root.join("status.json"))?;
        sync_directory(&self.root)
    }
    fn run(&mut self, receiver: Receiver<QueuedRecord>, pool: Pool) {
        let mut reported_error = 0;
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(mut record) => {
                    let result = (|| -> io::Result<()> {
                        if record.sequence != self.sequence + 1
                            || record.sequence > RECORD_LIMIT as u64
                        {
                            return Err(io::Error::other("projection writer sequence mismatch"));
                        }
                        self.records
                            .write_all(&record.buffer.bytes[..record.buffer.len])?;
                        self.records.flush()?;
                        self.records.sync_all()?;
                        self.digest
                            .update(&record.buffer.bytes[..record.buffer.len]);
                        self.bytes += record.buffer.len as u64;
                        self.sequence = record.sequence;
                        self.status(false)
                    })();
                    record.buffer.len = 0;
                    if let Ok(mut slots) = pool.lock() {
                        slots.push(record.buffer);
                    }
                    if result.is_err() {
                        self.health.fail(WriterFailure::IoFailure);
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let error = self.health.first_error.load(Ordering::SeqCst);
            if error != 0 && error != reported_error {
                if self.status(false).is_err() {
                    self.health.fail(WriterFailure::IoFailure);
                    break;
                }
                reported_error = error;
            }
        }
        if self.status(true).is_err() {
            self.health.fail(WriterFailure::IoFailure);
        }
    }
}

fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn prepare_directory(cache: &Path, id: Uuid) -> io::Result<PathBuf> {
    prepare_directory_with(cache, id, sync_directory)
}

fn real_directory(path: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(io::Error::other(
            "projection storage must be a real directory",
        ));
    }
    Ok(())
}

fn create_directory(path: &Path, private: bool) -> io::Result<()> {
    #[cfg(unix)]
    let mut directory = fs::DirBuilder::new();
    #[cfg(not(unix))]
    let directory = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(if private { 0o700 } else { 0o755 });
    }
    #[cfg(not(unix))]
    let _ = private;
    directory.create(path)
}

fn private_directory(path: &Path) -> io::Result<()> {
    real_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::symlink_metadata(path)?;
        // SAFETY: geteuid reads the process identity and has no pointer arguments.
        let owner = unsafe { libc::geteuid() };
        if metadata.uid() != owner || metadata.permissions().mode() & 0o7777 != 0o700 {
            return Err(io::Error::other(
                "projection directory must be privately owned",
            ));
        }
    }
    Ok(())
}

fn prepare_directory_with(
    cache: &Path,
    id: Uuid,
    sync: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    // The actual configured cache is an existing anchor. No arbitrary output
    // path or recursively created ancestor is accepted by this writer.
    real_directory(cache)?;
    let logs = cache.join("logs");
    match create_directory(&logs, false) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    real_directory(&logs)?;
    sync(cache)?;
    let parent = logs.join("mst2-native-projection");
    match create_directory(&parent, true) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    private_directory(&parent)?;
    sync(&logs)?;
    let root = parent.join(id.to_string());
    create_directory(&root, true)?;
    private_directory(&root)?;
    sync(&parent)?;
    Ok(root)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable projection writer requires directory fsync",
        ))
    }
}

#[cfg(test)]
#[path = "projection_writer_tests.rs"]
mod tests;
