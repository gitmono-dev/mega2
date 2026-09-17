//! Bounded committed-write emitter runtime (WH-02).
//!
//! Default-disabled. Secrets and CLI/service drain wiring belong to later cards.
//!
//! Architecture: `try_emit` admits synchronously (permit before spawn), hands
//! every send task's `JoinHandle` and semaphore permit to a single lifecycle
//! task through a channel, and every terminating send task reports its
//! sequence number through a drop guard, so the lifecycle task joins normal
//! completions, panics and aborts alike. Capacity stays reserved until the
//! join, which bounds retained task records by `max_in_flight`. Shutdown
//! closes admission atomically, drains with grace, then aborts and joins what
//! remains.

use std::{
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, watch},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::{
    common::errors::MegaError,
    config::{Config, StorageEventsTargetConfig},
    jupiter::service::{
        storage_event::{
            CommittedEvent, EventData, EventScope, EventSource, EventType, ProjectionDrop, project,
            select_targets, validate_canonical_path,
        },
        storage_event_transport::{EventTarget, EventTransport, TransportSuccess},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDisposition {
    Accepted {
        accepted_targets: usize,
        dropped_targets: usize,
    },
    DroppedDisabled,
    DroppedInvalidScope,
    DroppedFilter,
    DroppedSize,
    DroppedCapacity,
    DroppedClosed,
    DroppedInvalidEvent,
}

impl AdmissionDisposition {
    /// Stable lowercase label used as the `disposition` field of drop logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Accepted { .. } => "accepted",
            Self::DroppedDisabled => "dropped_disabled",
            Self::DroppedInvalidScope => "dropped_invalid_scope",
            Self::DroppedFilter => "dropped_filter",
            Self::DroppedSize => "dropped_size",
            Self::DroppedCapacity => "dropped_capacity",
            Self::DroppedClosed => "dropped_closed",
            Self::DroppedInvalidEvent => "dropped_invalid_event",
        }
    }
}

/// Process-local drop accounting (WH-15). No metrics crate — same shape as
/// `PushQueueMetrics`: atomics bumped at the drop site, read through a
/// snapshot. One counter per non-accepted `AdmissionDisposition`, plus a
/// running total of per-target drops (which also covers partial fan-out on an
/// event that was otherwise accepted).
#[derive(Default)]
struct DropCounters {
    disabled: AtomicU64,
    invalid_scope: AtomicU64,
    invalid_event: AtomicU64,
    size: AtomicU64,
    filter: AtomicU64,
    capacity: AtomicU64,
    closed: AtomicU64,
    dropped_targets: AtomicU64,
}

/// Point-in-time copy of the drop counters. Counts only; never carries
/// payload, target URLs or secrets (ADR-WH-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct StorageEventDropSnapshot {
    pub disabled: u64,
    pub invalid_scope: u64,
    pub invalid_event: u64,
    pub size: u64,
    pub filter: u64,
    pub capacity: u64,
    pub closed: u64,
    /// Per-target drops across every event, including partial fan-out.
    pub dropped_targets: u64,
}

/// Git typed builder for the WH-03 `repo.push` committed event
/// (plan-20260912「事件身份与 scope 不变量」/ ADR-WH-04). The snapshot comes
/// entirely from the caller's committed round; `occurred_at` is stamped at
/// construction. `repo_path` must already be canonical — a non-canonical path
/// fails the builder and the caller maps that to a dropped event, never a
/// business error.
/// Wire data fields of a `repo.push` event (ADR-WH-04 whitelist).
pub struct RepoPushData {
    pub push_id: String,
    pub operation_id: String,
    pub ref_name: String,
    pub old_oid: String,
    pub requested_oid: String,
    pub landed_oid: String,
}

pub fn repo_push_event(
    installation_id: &str,
    repo_path: &str,
    data: RepoPushData,
) -> Result<CommittedEvent, MegaError> {
    validate_canonical_path(repo_path)?;
    let event_id = repo_push_event_id(
        installation_id,
        repo_path,
        &data.operation_id,
        &data.landed_oid,
    );
    Ok(CommittedEvent {
        event_id,
        event_type: EventType::RepoPush,
        occurred_at: chrono::Utc::now().timestamp() as u64,
        source: EventSource::Git,
        scope: EventScope::Git {
            repo_path: repo_path.to_owned(),
        },
        data: EventData::RepoPush {
            push_id: data.push_id,
            operation_id: data.operation_id,
            ref_name: data.ref_name,
            old_oid: data.old_oid,
            requested_oid: data.requested_oid,
            landed_oid: data.landed_oid,
        },
    })
}

/// `sha256("repo.push\0" + installation_id + "\0" + canonical_repo_path +
/// "\0" + operation_id + "\0" + landed_commit_id)`, first 16 bytes encoded
/// with the UUID v5 byte layout (version nibble 5, variant bits 10). The
/// queue's i64 id does not participate in the identity.
fn repo_push_event_id(
    installation_id: &str,
    repo_path: &str,
    operation_id: &str,
    landed_commit_id: &str,
) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(b"repo.push\0");
    hasher.update(installation_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(repo_path.as_bytes());
    hasher.update(b"\0");
    hasher.update(operation_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(landed_commit_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[derive(Clone)]
pub struct StorageEventEmitter {
    inner: Arc<Inner>,
}

/// A registered send task. The permit stays inside the registration until the
/// lifecycle task joins the handle, so outstanding records are bounded by
/// `max_in_flight` even while the consumer falls behind.
struct Registration {
    seq: u64,
    target_id: String,
    event_type: EventType,
    handle: JoinHandle<()>,
    _permit: OwnedSemaphorePermit,
}

struct Gate {
    closed: bool,
    /// Fixed at the first close: the grace deadline belongs to the shutdown
    /// request, not to whenever the lifecycle task happens to be scheduled.
    close_deadline: Option<tokio::time::Instant>,
}

struct LifecycleReceivers {
    regs: mpsc::UnboundedReceiver<Registration>,
    ends: mpsc::UnboundedReceiver<u64>,
    shutdown: watch::Receiver<bool>,
}

struct Inner {
    enabled: bool,
    /// Opaque installation namespace from `[storage_events].installation_id`.
    /// Recorded on every emitter log line; never enters the wire envelope.
    installation_id: Option<Arc<str>>,
    /// `installation_id` rendered for log lines (`-` when unset); an `Arc`
    /// so send tasks clone a pointer, not a string.
    installation_label: Arc<str>,
    drops: DropCounters,
    gate: std::sync::Mutex<Gate>,
    in_flight: AtomicUsize,
    pending: AtomicUsize,
    completed: AtomicUsize,
    cancelled: AtomicUsize,
    panicked: AtomicUsize,
    next_seq: AtomicU64,
    semaphore: Arc<Semaphore>,
    grace: Duration,
    transport: Arc<dyn EventTransport>,
    compiled_targets: Vec<(StorageEventsTargetConfig, EventTarget)>,
    regs_tx: mpsc::UnboundedSender<Registration>,
    ends_tx: mpsc::UnboundedSender<u64>,
    receivers: std::sync::Mutex<Option<LifecycleReceivers>>,
    shutdown_tx: watch::Sender<bool>,
    lifecycle_started: AtomicBool,
    drain_active: AtomicBool,
    drain_complete: AtomicBool,
    drain_done: Notify,
    /// Number of callers currently parked in the shutdown wait loop.
    shutdown_waiters: AtomicUsize,
}

/// Reports task termination to the lifecycle task. Constructed in `try_emit`
/// and moved into the send future as a capture, so it fires on normal return,
/// on panic unwind, and on abort — including abort before the first poll.
struct ReapSignal {
    seq: u64,
    ends_tx: mpsc::UnboundedSender<u64>,
}

impl Inner {
    /// `installation_id` for log lines; `-` when the deployment sets none.
    fn installation_label(&self) -> &str {
        &self.installation_label
    }
}

impl Drop for ReapSignal {
    fn drop(&mut self) {
        let _ = self.ends_tx.send(self.seq);
    }
}

/// Registers a parked shutdown caller; the count is released even when the
/// caller's future is cancelled mid-wait.
struct WaiterRegistration<'a> {
    counter: &'a AtomicUsize,
}

impl WaiterRegistration<'_> {
    fn new(counter: &AtomicUsize) -> WaiterRegistration<'_> {
        counter.fetch_add(1, Ordering::SeqCst);
        WaiterRegistration { counter }
    }
}

impl Drop for WaiterRegistration<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

fn lock_gate(gate: &std::sync::Mutex<Gate>) -> std::sync::MutexGuard<'_, Gate> {
    // A poisoned gate must not abandon registered tasks or block cleanup:
    // recover the guarded state and keep the lifecycle contract intact.
    gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl StorageEventEmitter {
    pub fn disabled() -> Self {
        Self::new_inner(
            false,
            None,
            1,
            Duration::from_secs(0),
            Arc::new(NoopTransport),
            Vec::new(),
        )
    }

    /// Disabled emitter that still records the configured `installation_id`.
    /// `Storage::new_with_connection` installs it, so a disabled deployment's
    /// drop lines carry the same namespace field as an enabled one;
    /// `bind_storage_event_emitter` replaces it only when enabled.
    pub fn from_config_disabled(config: &Config) -> Self {
        Self::new_inner(
            false,
            Self::installation_id_from(config),
            1,
            Duration::from_secs(0),
            Arc::new(NoopTransport),
            Vec::new(),
        )
    }

    pub fn new_with_transport(
        config: &Config,
        transport: Arc<dyn EventTransport>,
        compiled_targets: Vec<(StorageEventsTargetConfig, EventTarget)>,
    ) -> Self {
        Self::new_inner(
            config.storage_events.enabled,
            Self::installation_id_from(config),
            config.storage_events.max_in_flight.max(1) as usize,
            Duration::from_secs(config.storage_events.shutdown_grace_seconds),
            transport,
            compiled_targets,
        )
    }

    fn installation_id_from(config: &Config) -> Option<Arc<str>> {
        config
            .storage_events
            .installation_id
            .as_deref()
            .map(Arc::from)
    }

    fn new_inner(
        enabled: bool,
        installation_id: Option<Arc<str>>,
        max_in_flight: usize,
        grace: Duration,
        transport: Arc<dyn EventTransport>,
        compiled_targets: Vec<(StorageEventsTargetConfig, EventTarget)>,
    ) -> Self {
        let (regs_tx, regs) = mpsc::unbounded_channel();
        let (ends_tx, ends) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown) = watch::channel(false);
        let installation_label: Arc<str> = installation_id
            .as_deref()
            .map(Arc::from)
            .unwrap_or_else(|| Arc::from("-"));
        Self {
            inner: Arc::new(Inner {
                enabled,
                installation_id,
                installation_label,
                drops: DropCounters::default(),
                gate: std::sync::Mutex::new(Gate {
                    closed: false,
                    close_deadline: None,
                }),
                in_flight: AtomicUsize::new(0),
                pending: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
                cancelled: AtomicUsize::new(0),
                panicked: AtomicUsize::new(0),
                next_seq: AtomicU64::new(0),
                semaphore: Arc::new(Semaphore::new(max_in_flight)),
                grace,
                transport,
                compiled_targets,
                regs_tx,
                ends_tx,
                receivers: std::sync::Mutex::new(Some(LifecycleReceivers {
                    regs,
                    ends,
                    shutdown,
                })),
                shutdown_tx,
                lifecycle_started: AtomicBool::new(false),
                drain_active: AtomicBool::new(false),
                drain_complete: AtomicBool::new(false),
                drain_done: Notify::new(),
                shutdown_waiters: AtomicUsize::new(0),
            }),
        }
    }

    pub fn try_emit(&self, event: CommittedEvent) -> AdmissionDisposition {
        let event_type = event.event_type;
        if lock_gate(&self.inner.gate).closed {
            return self.note_drop(AdmissionDisposition::DroppedClosed, event_type, 0);
        }
        if !self.inner.enabled {
            return self.note_drop(AdmissionDisposition::DroppedDisabled, event_type, 0);
        }
        let body = match project(&event) {
            Ok(body) => body,
            Err(ProjectionDrop::Size) => {
                return self.note_drop(AdmissionDisposition::DroppedSize, event_type, 0);
            }
            Err(ProjectionDrop::InvalidScope) => {
                return self.note_drop(AdmissionDisposition::DroppedInvalidScope, event_type, 0);
            }
            Err(ProjectionDrop::InvalidEvent) => {
                return self.note_drop(AdmissionDisposition::DroppedInvalidEvent, event_type, 0);
            }
        };
        let configs: Vec<StorageEventsTargetConfig> = self
            .inner
            .compiled_targets
            .iter()
            .map(|(config, _)| config.clone())
            .collect();
        let selected_ids: Vec<&str> = select_targets(&event, &configs)
            .into_iter()
            .map(|config| config.id.as_str())
            .collect();
        if selected_ids.is_empty() {
            return self.note_drop(AdmissionDisposition::DroppedFilter, event_type, 0);
        }
        let installation = Arc::clone(&self.inner.installation_label);
        // Per-target drops are recorded here and logged only after the gate
        // is released: the critical section below never traces and does no
        // unbounded work — per selected target it clones the compiled target
        // and body, spawns the send task and does atomics + channel sends
        // (ADR-WH-03). `target_drops` is pre-sized to the selected count.
        let mut target_drops: Vec<(&str, &'static str)> = Vec::with_capacity(selected_ids.len());
        let mut accepted = 0usize;
        let mut dropped = 0usize;
        let gate = lock_gate(&self.inner.gate);
        if gate.closed {
            drop(gate);
            return self.note_drop(AdmissionDisposition::DroppedClosed, event_type, 0);
        }
        // Ids are unique (config validation), so membership in the selected
        // set is a complete description of the fan-out; walking the compiled
        // list directly means every selected target resolves by construction.
        for (config, compiled) in &self.inner.compiled_targets {
            if !selected_ids.contains(&config.id.as_str()) {
                continue;
            }
            let Ok(permit) = self.inner.semaphore.clone().try_acquire_owned() else {
                self.inner
                    .drops
                    .dropped_targets
                    .fetch_add(1, Ordering::SeqCst);
                target_drops.push((config.id.as_str(), "dropped_target_no_permit"));
                dropped += 1;
                continue;
            };
            let seq = self.inner.next_seq.fetch_add(1, Ordering::SeqCst);
            // Reserve accounting before publishing the registration; a fast
            // lifecycle consumer may join the task immediately afterwards.
            self.inner.pending.fetch_add(1, Ordering::SeqCst);
            self.inner.in_flight.fetch_add(1, Ordering::SeqCst);
            let transport = Arc::clone(&self.inner.transport);
            let target = compiled.clone();
            let payload = body.clone();
            let target_id = compiled.id.clone();
            let task_target_id = target_id.clone();
            let task_installation = Arc::clone(&installation);
            let reap = ReapSignal {
                seq,
                ends_tx: self.inner.ends_tx.clone(),
            };
            let inner_weak = Arc::downgrade(&self.inner);
            let handle = tokio::spawn(async move {
                let outcome = transport.post(&target, payload).await;
                if let Some(inner) = inner_weak.upgrade() {
                    inner.completed.fetch_add(1, Ordering::SeqCst);
                }
                let category = match &outcome {
                    Ok(TransportSuccess::Accepted2xx { .. }) => "delivered_2xx",
                    Err(err) => err.category(),
                };
                tracing::info!(
                    installation_id = %task_installation,
                    target_id = %task_target_id,
                    event_type = event_type.as_str(),
                    category,
                    "storage_events delivery"
                );
                // Dropping the capture reports termination; it is released
                // after the outcome has been recorded above.
                drop(reap);
            });
            let registration = Registration {
                seq,
                target_id,
                event_type,
                handle,
                _permit: permit,
            };
            match self.inner.regs_tx.send(registration) {
                Ok(()) => accepted += 1,
                Err(sent_back) => {
                    // Unreachable while an owner exists: the lifecycle task
                    // only stops after admission closed. Stay leak-free.
                    self.inner.pending.fetch_sub(1, Ordering::SeqCst);
                    self.inner.in_flight.fetch_sub(1, Ordering::SeqCst);
                    sent_back.0.handle.abort();
                    self.inner
                        .drops
                        .dropped_targets
                        .fetch_add(1, Ordering::SeqCst);
                    target_drops.push((config.id.as_str(), "dropped_target_lifecycle_closed"));
                    dropped += 1;
                }
            }
        }
        drop(gate);
        for (target_id, disposition) in target_drops {
            self.log_target_drop(event_type, target_id, disposition);
        }
        if accepted > 0 {
            self.ensure_lifecycle_started();
        }
        if accepted == 0 && dropped > 0 {
            return self.note_drop(AdmissionDisposition::DroppedCapacity, event_type, dropped);
        }
        AdmissionDisposition::Accepted {
            accepted_targets: accepted,
            dropped_targets: dropped,
        }
    }

    /// Record a builder failure in a source adapter (plan-20260912「固定 wire
    /// schema」). The adapter never reaches `try_emit` in that case, so this
    /// is the single accounting point for it, and it applies the same
    /// precedence `try_emit` would: a closed admission records
    /// `dropped_closed`, a disabled emitter `dropped_disabled`, otherwise
    /// `dropped_invalid_event`. It must never be turned into a business error.
    pub fn record_invalid_event(&self, event_type: EventType) {
        let disposition = if lock_gate(&self.inner.gate).closed {
            AdmissionDisposition::DroppedClosed
        } else if !self.inner.enabled {
            AdmissionDisposition::DroppedDisabled
        } else {
            AdmissionDisposition::DroppedInvalidEvent
        };
        self.note_drop(disposition, event_type, 0);
    }

    /// Point-in-time drop counts for logs/metrics consumers (WH-15).
    pub fn drop_counts(&self) -> StorageEventDropSnapshot {
        let d = &self.inner.drops;
        StorageEventDropSnapshot {
            disabled: d.disabled.load(Ordering::SeqCst),
            invalid_scope: d.invalid_scope.load(Ordering::SeqCst),
            invalid_event: d.invalid_event.load(Ordering::SeqCst),
            size: d.size.load(Ordering::SeqCst),
            filter: d.filter.load(Ordering::SeqCst),
            capacity: d.capacity.load(Ordering::SeqCst),
            closed: d.closed.load(Ordering::SeqCst),
            dropped_targets: d.dropped_targets.load(Ordering::SeqCst),
        }
    }

    /// The configured installation namespace, `None` when unset.
    pub fn installation_id(&self) -> Option<&str> {
        self.inner.installation_id.as_deref()
    }

    /// Bump the counter for an event-level drop and log one line. The line
    /// carries only category and count fields (ADR-WH-02): no payload, path,
    /// oid, URL or secret. Uses a message distinct from the delivery line so
    /// delivery counters that filter on `storage_events delivery` +
    /// `category=` are unaffected.
    fn note_drop(
        &self,
        disposition: AdmissionDisposition,
        event_type: EventType,
        dropped_targets: usize,
    ) -> AdmissionDisposition {
        let d = &self.inner.drops;
        let counter = match disposition {
            AdmissionDisposition::DroppedDisabled => &d.disabled,
            AdmissionDisposition::DroppedInvalidScope => &d.invalid_scope,
            AdmissionDisposition::DroppedInvalidEvent => &d.invalid_event,
            AdmissionDisposition::DroppedSize => &d.size,
            AdmissionDisposition::DroppedFilter => &d.filter,
            AdmissionDisposition::DroppedCapacity => &d.capacity,
            AdmissionDisposition::DroppedClosed => &d.closed,
            AdmissionDisposition::Accepted { .. } => return disposition,
        };
        counter.fetch_add(1, Ordering::SeqCst);
        // Disabled is the default deployment's steady state: every committed
        // write of the OCI/LFS/media/agent adapters passes through it, so it
        // logs at debug to keep the default log free of per-write noise. The
        // counter still moves; the other six dispositions stay at info.
        if matches!(disposition, AdmissionDisposition::DroppedDisabled) {
            tracing::debug!(
                installation_id = %self.inner.installation_label(),
                event_type = event_type.as_str(),
                disposition = disposition.as_str(),
                dropped_targets,
                "storage_events dropped"
            );
        } else {
            tracing::info!(
                installation_id = %self.inner.installation_label(),
                event_type = event_type.as_str(),
                disposition = disposition.as_str(),
                dropped_targets,
                "storage_events dropped"
            );
        }
        disposition
    }

    /// Log a single target that could not be admitted while the event as a
    /// whole may still fan out to other targets. The `dropped_targets`
    /// counter was already bumped at the drop site; this runs only after
    /// the admission gate is released.
    fn log_target_drop(&self, event_type: EventType, target_id: &str, disposition: &'static str) {
        tracing::info!(
            installation_id = %self.inner.installation_label(),
            event_type = event_type.as_str(),
            disposition,
            target_id = %target_id,
            "storage_events dropped"
        );
    }

    pub async fn shutdown(&self) {
        {
            let mut gate = lock_gate(&self.inner.gate);
            if !gate.closed {
                gate.closed = true;
                gate.close_deadline = Some(tokio::time::Instant::now() + self.inner.grace);
            }
        }
        self.ensure_lifecycle_started();
        let _ = self.inner.shutdown_tx.send(true);
        // The drain runs in the shared lifecycle task: if this caller is
        // cancelled the drain keeps going and later callers observe the same
        // completion flag. The timeout keeps the wait immune to a notify
        // that lands between the flag check and the wait itself.
        let _waiter = WaiterRegistration::new(&self.inner.shutdown_waiters);
        loop {
            if self.inner.drain_complete.load(Ordering::SeqCst) {
                return;
            }
            let _ =
                tokio::time::timeout(Duration::from_millis(20), self.inner.drain_done.notified())
                    .await;
        }
    }

    fn ensure_lifecycle_started(&self) {
        if self
            .inner
            .lifecycle_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let receivers = self
                .inner
                .receivers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(receivers) = receivers {
                let inner_weak = Arc::downgrade(&self.inner);
                let grace = self.inner.grace;
                tokio::spawn(async move {
                    run_lifecycle(inner_weak, grace, receivers).await;
                });
            }
        }
    }

    #[cfg(test)]
    fn task_count(&self) -> usize {
        self.inner.pending.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn in_flight_count(&self) -> usize {
        self.inner.in_flight.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn completed_count(&self) -> usize {
        self.inner.completed.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn cancelled_count(&self) -> usize {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn panicked_count(&self) -> usize {
        self.inner.panicked.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn shutdown_initiated(&self) -> bool {
        *self.inner.shutdown_tx.borrow()
    }

    #[cfg(test)]
    fn drain_active(&self) -> bool {
        self.inner.drain_active.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn shutdown_waiter_count(&self) -> usize {
        self.inner.shutdown_waiters.load(Ordering::SeqCst)
    }
}

async fn run_lifecycle(inner_weak: Weak<Inner>, grace: Duration, receivers: LifecycleReceivers) {
    let LifecycleReceivers {
        mut regs,
        mut ends,
        mut shutdown,
    } = receivers;
    let mut tasks: Vec<Registration> = Vec::new();
    // A completion report can beat its registration when the send task
    // finishes between `tokio::spawn` and the registration send.
    let mut early_ends: Vec<u64> = Vec::new();
    let mut owner_gone = false;
    loop {
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            msg = regs.recv() => {
                match msg {
                    Some(registration) => {
                        if let Some(pos) =
                            early_ends.iter().position(|seq| *seq == registration.seq)
                        {
                            early_ends.swap_remove(pos);
                            join_one(&inner_weak, registration).await;
                        } else {
                            tasks.push(registration);
                        }
                    }
                    None => {
                        // The registration sender lives in `Inner`, so the
                        // channel closes exactly when the last owner dropped.
                        owner_gone = true;
                        break;
                    }
                }
            }
            msg = ends.recv() => {
                if let Some(seq) = msg
                    && !join_by_seq(&inner_weak, &mut tasks, seq).await
                {
                    early_ends.push(seq);
                }
            }
            _ = shutdown.changed() => {}
        }
    }

    // Collect registrations that were sent before admission closed.
    while let Ok(registration) = regs.try_recv() {
        if let Some(pos) = early_ends.iter().position(|seq| *seq == registration.seq) {
            early_ends.swap_remove(pos);
            join_one(&inner_weak, registration).await;
        } else {
            tasks.push(registration);
        }
    }

    let deadline = if owner_gone {
        // Final-owner cleanup aborts immediately rather than waiting out a
        // grace period nobody can observe.
        tokio::time::Instant::now()
    } else {
        // The deadline belongs to the first close; repeated shutdown callers
        // never extend it.
        inner_weak
            .upgrade()
            .and_then(|inner| lock_gate(&inner.gate).close_deadline)
            .unwrap_or_else(|| tokio::time::Instant::now() + grace)
    };
    if let Some(inner) = inner_weak.upgrade() {
        inner.drain_active.store(true, Ordering::SeqCst);
    }
    let mut aborted = false;
    let mut regs_closed = false;
    loop {
        if tasks.is_empty() {
            break;
        }
        if !aborted && tokio::time::Instant::now() >= deadline {
            abort_all(&tasks);
            aborted = true;
        }
        tokio::select! {
            msg = regs.recv(), if !regs_closed => {
                match msg {
                    Some(registration) => tasks.push(registration),
                    None => {
                        // Disable the arm: a closed channel stays immediately
                        // ready and would busy-loop the drain.
                        regs_closed = true;
                        // The last owner disappeared mid-drain: abort and
                        // join immediately instead of waiting out grace.
                        if !aborted {
                            abort_all(&tasks);
                            aborted = true;
                        }
                    }
                }
            }
            msg = ends.recv() => {
                match msg {
                    Some(seq) => {
                        join_by_seq(&inner_weak, &mut tasks, seq).await;
                    }
                    None => {
                        // Every ReapSignal sender is gone: all send tasks
                        // terminated, so join whatever remains directly.
                        let remaining = std::mem::take(&mut tasks);
                        for registration in remaining {
                            join_one(&inner_weak, registration).await;
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline), if !aborted => {
                abort_all(&tasks);
                aborted = true;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                // Safety net: join anything already finished whose
                // termination report was never processed.
                let mut index = 0;
                while index < tasks.len() {
                    if tasks[index].handle.is_finished() {
                        let registration = tasks.swap_remove(index);
                        join_one(&inner_weak, registration).await;
                    } else {
                        index += 1;
                    }
                }
            }
        }
    }
    if let Some(inner) = inner_weak.upgrade() {
        inner.drain_complete.store(true, Ordering::SeqCst);
        inner.drain_done.notify_waiters();
    }
}

fn abort_all(tasks: &[Registration]) {
    for registration in tasks {
        registration.handle.abort();
    }
}

/// Joins the task with `seq`. Returns false when the registration has not
/// arrived yet; the caller records the seq as an early completion instead.
async fn join_by_seq(inner_weak: &Weak<Inner>, tasks: &mut Vec<Registration>, seq: u64) -> bool {
    if let Some(pos) = tasks
        .iter()
        .position(|registration| registration.seq == seq)
    {
        let registration = tasks.swap_remove(pos);
        join_one(inner_weak, registration).await;
        return true;
    }
    false
}

async fn join_one(inner_weak: &Weak<Inner>, registration: Registration) {
    let Registration {
        target_id,
        event_type,
        handle,
        _permit,
        ..
    } = registration;
    let outcome = handle.await;
    // The admission permit is released only after the task terminated and was
    // joined, and before any counter is published.
    drop(_permit);
    if let Some(inner) = inner_weak.upgrade() {
        match outcome {
            Ok(()) => {
                // The task already recorded its own transport outcome.
            }
            Err(err) if err.is_cancelled() => {
                inner.cancelled.fetch_add(1, Ordering::SeqCst);
                tracing::info!(
                    installation_id = %inner.installation_label(),
                    target_id = %target_id,
                    event_type = event_type.as_str(),
                    category = "cancelled",
                    "storage_events delivery"
                );
            }
            Err(_) => {
                inner.panicked.fetch_add(1, Ordering::SeqCst);
                tracing::warn!(
                    installation_id = %inner.installation_label(),
                    target_id = %target_id,
                    event_type = event_type.as_str(),
                    category = "panicked",
                    "storage_events delivery"
                );
            }
        }
        // `pending` is published last: observing it reach zero implies every
        // other counter and the in-flight slot were already released.
        inner.in_flight.fetch_sub(1, Ordering::SeqCst);
        inner.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

struct NoopTransport;

impl EventTransport for NoopTransport {
    fn post(
        &self,
        _target: &EventTarget,
        _body: bytes::Bytes,
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
        Box::pin(async {
            Ok(
                crate::jupiter::service::storage_event_transport::TransportSuccess::Accepted2xx {
                    status: 200,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex as StdMutex, time::Instant};

    use bytes::Bytes;
    use tokio::sync::watch;
    use uuid::Uuid;

    use super::*;
    use crate::{
        config::{PushAuth, PushPolicy, secret::SecretString},
        jupiter::service::{
            storage_event::{EventData, EventScope, EventSource},
            storage_event_transport::{EventTarget, TransportError, TransportSuccess},
        },
    };

    /// Records delivery attempts and lets the test block, fail or panic the
    /// send. `cancelled` is set by a drop probe only when the future was
    /// dropped before completing. Release uses a watch channel so a release
    /// that lands before the task starts waiting is never lost.
    struct ProbeTransport {
        posts: Arc<StdMutex<usize>>,
        cancelled: Arc<AtomicBool>,
        release: watch::Sender<bool>,
        mode: ProbeMode,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ProbeMode {
        Block,
        Fail,
        Panic,
    }

    impl ProbeTransport {
        fn release(&self) {
            let _ = self.release.send(true);
        }
    }

    struct CancelProbe {
        cancelled: Arc<AtomicBool>,
        completed: bool,
    }

    impl Drop for CancelProbe {
        fn drop(&mut self) {
            if !self.completed {
                self.cancelled.store(true, Ordering::SeqCst);
            }
        }
    }

    impl EventTransport for ProbeTransport {
        fn post(
            &self,
            _target: &EventTarget,
            _body: Bytes,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<TransportSuccess, TransportError>> + Send>,
        > {
            let posts = Arc::clone(&self.posts);
            let cancelled = Arc::clone(&self.cancelled);
            let mut release = self.release.subscribe();
            let mode = self.mode;
            Box::pin(async move {
                let mut probe = CancelProbe {
                    cancelled,
                    completed: false,
                };
                *posts.lock().expect("posts") += 1;
                if !*release.borrow() {
                    let _ = release.changed().await;
                }
                probe.completed = true;
                match mode {
                    ProbeMode::Block => Ok(TransportSuccess::Accepted2xx { status: 200 }),
                    ProbeMode::Fail => Err(TransportError::Timeout),
                    ProbeMode::Panic => panic!("deliberate transport panic for lifecycle test"),
                }
            })
        }
    }

    fn probe(mode: ProbeMode) -> Arc<ProbeTransport> {
        let (release, _) = watch::channel(false);
        Arc::new(ProbeTransport {
            posts: Arc::new(StdMutex::new(0)),
            cancelled: Arc::new(AtomicBool::new(false)),
            release,
            mode,
        })
    }

    fn posts(transport: &ProbeTransport) -> usize {
        *transport.posts.lock().expect("posts")
    }

    async fn wait_posted(transport: &ProbeTransport) {
        wait_until(|| posts(transport) >= 1).await;
    }

    fn sample_event() -> CommittedEvent {
        CommittedEvent {
            event_id: Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("uuid"),
            event_type: EventType::LfsObjectUploaded,
            occurred_at: 1,
            source: EventSource::Lfs,
            scope: EventScope::LfsUnscoped,
            data: EventData::LfsObjectUploaded {
                oid: "ab".to_string(),
                size: 1,
            },
        }
    }

    fn invalid_scope_event() -> CommittedEvent {
        let mut event = sample_event();
        event.scope = EventScope::Git {
            repo_path: "/team/a".to_string(),
        };
        event
    }

    fn invalid_data_event() -> CommittedEvent {
        let mut event = sample_event();
        event.data = EventData::LfsObjectUploaded {
            oid: String::new(),
            size: 1,
        };
        event
    }

    fn oversized_event() -> CommittedEvent {
        CommittedEvent {
            event_id: Uuid::parse_str("11111111-1111-4111-8111-111111111111").expect("uuid"),
            event_type: EventType::AgentCaptureEventsCommitted,
            occurred_at: 1,
            source: EventSource::AgentCapture,
            scope: EventScope::AgentCapture {
                tenant_id: "t".repeat(4096),
                repo_path: format!("/{}", "r".repeat(4000)),
            },
            data: EventData::AgentCaptureEventsCommitted {
                capture_id: "c".repeat(4096),
                receipt_id: "r".repeat(4096),
                new_event_count: 1,
                stream_kind: "external_capture".to_string(),
                generation: 1,
                completeness: "complete".to_string(),
            },
        }
    }

    fn compiled_target(id: &str) -> (StorageEventsTargetConfig, EventTarget) {
        let config = StorageEventsTargetConfig {
            id: id.to_string(),
            url: "https://events.example.invalid/ingest".to_string(),
            secret_ref: format!(
                "vault://secret/config/example/storage_events/targets/{id}/hmac#value"
            ),
            events: vec!["lfs.object.uploaded".to_string()],
            git_paths: Vec::new(),
            oci_repositories: Vec::new(),
            lfs_paths: Vec::new(),
            include_unscoped_lfs: true,
            agent_tenants: Vec::new(),
            agent_repo_paths: Vec::new(),
        };
        let secret = SecretString::new(
            "hex:1111111111111111111111111111111111111111111111111111111111111111",
        );
        let compiled = EventTarget::compile(&config.id, &config.url, &secret).expect("target");
        (config, compiled)
    }

    #[test]
    fn repo_push_event_id_derivation() {
        let event = repo_push_event(
            "prod-primary-01",
            "/team/a",
            RepoPushData {
                push_id: "42".to_owned(),
                operation_id: "op-1".to_owned(),
                ref_name: "refs/heads/main".to_owned(),
                old_oid: "a".repeat(40),
                requested_oid: "b".repeat(40),
                landed_oid: "c".repeat(40),
            },
        )
        .expect("builder");
        assert_eq!(event.event_type, EventType::RepoPush);
        assert_eq!(event.source, EventSource::Git);
        assert!(event.occurred_at > 0);

        // Identical inputs are stable; each identity input distinguishes.
        let again = repo_push_event(
            "prod-primary-01",
            "/team/a",
            RepoPushData {
                push_id: "43".to_owned(),
                operation_id: "op-1".to_owned(),
                ref_name: "refs/heads/main".to_owned(),
                old_oid: "a".repeat(40),
                requested_oid: "b".repeat(40),
                landed_oid: "c".repeat(40),
            },
        )
        .expect("builder");
        assert_eq!(event.event_id, again.event_id);
        for (installation, path, op, landed) in [
            ("other-install", "/team/a", "op-1", "c".repeat(40)),
            ("prod-primary-01", "/team/b", "op-1", "c".repeat(40)),
            ("prod-primary-01", "/team/a", "op-2", "c".repeat(40)),
            ("prod-primary-01", "/team/a", "op-1", "d".repeat(40)),
        ] {
            let other = repo_push_event(
                installation,
                path,
                RepoPushData {
                    push_id: "42".to_owned(),
                    operation_id: op.to_owned(),
                    ref_name: "refs/heads/main".to_owned(),
                    old_oid: "a".repeat(40),
                    requested_oid: "b".repeat(40),
                    landed_oid: landed,
                },
            )
            .expect("builder");
            assert_ne!(event.event_id, other.event_id);
        }

        // UUID v5 byte layout: version nibble 5, variant bits 10.
        let bytes = event.event_id.as_bytes();
        assert_eq!(bytes[6] >> 4, 5);
        assert_eq!(bytes[8] >> 6, 0b10);

        // Non-canonical paths fail the builder (the adapter drops, never errors).
        assert!(
            repo_push_event(
                "prod-primary-01",
                "/team/a/",
                RepoPushData {
                    push_id: "42".to_owned(),
                    operation_id: "op-1".to_owned(),
                    ref_name: "refs/heads/main".to_owned(),
                    old_oid: "a".repeat(40),
                    requested_oid: "b".repeat(40),
                    landed_oid: "c".repeat(40),
                },
            )
            .is_err()
        );
    }

    /// Thread-local tracing capture. The runtime is built and driven inside
    /// the closure so every record — including those from tasks the
    /// current-thread runtime polls — lands on this subscriber (same shape as
    /// `ceres::api_service::un25_freeze::capture_tracing`).
    fn capture_tracing<F: FnOnce()>(f: F) -> String {
        use std::io::Write;

        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone)]
        struct TestWriter(Arc<StdMutex<Vec<u8>>>);

        impl Write for TestWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().expect("log buffer").extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl MakeWriter<'_> for TestWriter {
            type Writer = TestWriter;
            fn make_writer(&self) -> Self::Writer {
                self.clone()
            }
        }

        // tracing-core caches a callsite's `Interest` from the *registering
        // thread's* default dispatcher while at most one dispatcher is
        // registered (`Dispatchers::has_just_one`). A parallel test that hits
        // one of the emitter's callsites first — under the global no-op
        // subscriber — would cache `never`, and this thread's scoped
        // subscriber would never see the event. Keeping a second live
        // dispatcher registered for the whole capture makes tracing-core
        // consult its registry instead, so interest is computed from real
        // subscribers regardless of which thread registers the callsite.
        let _pin_registry = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .with_max_level(tracing::Level::DEBUG)
                .finish(),
        );
        let buf = Arc::new(StdMutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(TestWriter(buf.clone()))
            .with_ansi(false)
            // `dropped_disabled` is debug-level; capture it too.
            .with_max_level(tracing::Level::DEBUG)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8(buf.lock().expect("log buffer").clone()).expect("utf8 logs")
    }

    fn git_event() -> CommittedEvent {
        repo_push_event(
            "prod-primary-01",
            "/team/a",
            RepoPushData {
                push_id: "1".to_string(),
                operation_id: "op-1".to_string(),
                ref_name: "refs/heads/main".to_string(),
                old_oid: "a".repeat(40),
                requested_oid: "b".repeat(40),
                landed_oid: "c".repeat(40),
            },
        )
        .expect("canonical repo.push event")
    }

    /// WH-15: every drop branch bumps its counter and logs one
    /// `storage_events dropped` line (`dropped_disabled` at debug, the rest at
    /// info); `record_invalid_event` accounts builder failures with the
    /// admission precedence; every emitter line carries `installation_id`
    /// (`-` when the deployment sets none); drop lines carry only the AC7
    /// field set and never match the delivery-line filter used by the
    /// process-level ITs.
    #[test]
    fn drop_accounting_and_installation_id() {
        let captured = capture_tracing(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");
            rt.block_on(async {
                // A bare `disabled()` emitter has no installation id at all.
                let bare = StorageEventEmitter::disabled();
                assert_eq!(bare.installation_id(), None);
                assert_eq!(
                    bare.try_emit(sample_event()),
                    AdmissionDisposition::DroppedDisabled
                );
                assert_eq!(bare.drop_counts().disabled, 1);

                // A config-derived disabled emitter still records the id.
                let cfg_disabled = StorageEventEmitter::from_config_disabled(&enabled_config());
                assert_eq!(cfg_disabled.installation_id(), Some("prod-primary-01"));
                assert_eq!(
                    cfg_disabled.try_emit(sample_event()),
                    AdmissionDisposition::DroppedDisabled
                );
                assert_eq!(cfg_disabled.drop_counts().disabled, 1);

                // Enabled, one permit, a transport that never completes.
                let transport = probe(ProbeMode::Block);
                let emitter = StorageEventEmitter::new_with_transport(
                    &enabled_config(),
                    transport.clone(),
                    vec![compiled_target("t1")],
                );
                assert_eq!(emitter.installation_id(), Some("prod-primary-01"));
                assert_eq!(
                    emitter.try_emit(invalid_scope_event()),
                    AdmissionDisposition::DroppedInvalidScope
                );
                assert_eq!(
                    emitter.try_emit(invalid_data_event()),
                    AdmissionDisposition::DroppedInvalidEvent
                );
                assert_eq!(
                    emitter.try_emit(oversized_event()),
                    AdmissionDisposition::DroppedSize
                );
                // A valid repo.push matches no target (t1 subscribes only lfs).
                assert_eq!(
                    emitter.try_emit(git_event()),
                    AdmissionDisposition::DroppedFilter
                );
                // Takes the single permit; the send blocks.
                assert_eq!(
                    emitter.try_emit(sample_event()),
                    AdmissionDisposition::Accepted {
                        accepted_targets: 1,
                        dropped_targets: 0,
                    }
                );
                wait_posted(&transport).await;
                // No permit left: per-target drop, then event-level capacity.
                assert_eq!(
                    emitter.try_emit(sample_event()),
                    AdmissionDisposition::DroppedCapacity
                );
                // Adapter builder failure is accounted without an event.
                emitter.record_invalid_event(EventType::RepoPush);
                // …and on a disabled emitter it follows try_emit's precedence.
                bare.record_invalid_event(EventType::RepoPush);
                assert_eq!(bare.drop_counts().disabled, 2);
                assert_eq!(bare.drop_counts().invalid_event, 0);
                // Grace is 0: shutdown aborts the blocked send and joins it.
                shutdown_bounded(&emitter).await;
                assert_eq!(
                    emitter.try_emit(sample_event()),
                    AdmissionDisposition::DroppedClosed
                );
                // Closed admission wins over "invalid" for builder failures too.
                emitter.record_invalid_event(EventType::RepoPush);

                assert_eq!(
                    emitter.drop_counts(),
                    StorageEventDropSnapshot {
                        disabled: 0,
                        invalid_scope: 1,
                        invalid_event: 2,
                        size: 1,
                        filter: 1,
                        capacity: 1,
                        closed: 2,
                        dropped_targets: 1,
                    }
                );
                // Accounting never touched the send/join counters.
                assert_eq!(emitter.cancelled_count(), 1);
                assert_eq!(emitter.task_count(), 0);
            });
        });

        let drop_lines: Vec<&str> = captured
            .lines()
            .filter(|line| line.contains("storage_events dropped"))
            .collect();
        // 11 event-level drops (bare: disabled ×2 incl. the builder failure;
        // cfg-disabled ×1; enabled: scope, event, size, filter, capacity,
        // builder, closed ×2 incl. the builder failure after shutdown) + 1
        // per-target line for the no-permit.
        assert_eq!(
            drop_lines.len(),
            12,
            "drop lines:\n{}",
            drop_lines.join("\n")
        );
        // AC7 as an exact field-name set, not a substring blacklist: every
        // `key=` after the message must be in the whitelist.
        let whitelist = [
            "installation_id",
            "event_type",
            "disposition",
            "target_id",
            "dropped_targets",
        ];
        for line in &drop_lines {
            let fields = line
                .split("storage_events dropped")
                .nth(1)
                .expect("message present");
            let keys: Vec<&str> = fields
                .split_whitespace()
                .filter_map(|token| token.split_once('=').map(|(k, _)| k))
                .collect();
            assert!(!keys.is_empty(), "{line}");
            for key in &keys {
                assert!(
                    whitelist.contains(key),
                    "field {key} is outside AC7 whitelist: {line}"
                );
            }
            for required in ["installation_id", "event_type", "disposition"] {
                assert!(keys.contains(&required), "missing {required}: {line}");
            }
            let disposition = fields
                .split_whitespace()
                .find_map(|token| token.strip_prefix("disposition="))
                .expect("disposition field")
                .trim_matches('"');
            assert!(disposition.starts_with("dropped_"), "{line}");
            // The disabled disposition is debug-level (default deployments
            // get no per-write INFO line); every other drop line is info.
            if disposition == "dropped_disabled" {
                assert!(line.contains("DEBUG"), "{line}");
            } else {
                assert!(line.contains("INFO"), "{line}");
            }
            // Must never satisfy the process-level delivery filter.
            assert!(!line.contains("storage_events delivery"), "{line}");
            assert!(!line.contains("category="), "{line}");
        }
        assert!(
            drop_lines
                .iter()
                .any(|l| l.contains("installation_id=-") && l.contains("dropped_disabled")),
            "bare disabled emitter must log `-` as its installation id"
        );
        assert_eq!(
            drop_lines
                .iter()
                .filter(|l| l.contains("installation_id=prod-primary-01"))
                .count(),
            10
        );
        assert_eq!(
            drop_lines
                .iter()
                .filter(|l| l.contains("installation_id=-"))
                .count(),
            2
        );
        assert!(
            drop_lines
                .iter()
                .any(|l| l.contains("dropped_target_no_permit") && l.contains("target_id=t1")),
            "per-target drop must name the target"
        );
        let expected_dispositions = [
            "dropped_disabled",
            "dropped_invalid_scope",
            "dropped_invalid_event",
            "dropped_size",
            "dropped_filter",
            "dropped_capacity",
            "dropped_closed",
        ];
        for disposition in expected_dispositions {
            assert!(
                drop_lines.iter().any(|l| l.contains(disposition)),
                "missing a drop line for {disposition}"
            );
        }

        // The one accepted send was cancelled by shutdown; its delivery line
        // (the only one) carries the installation id too (AC5).
        let delivery: Vec<&str> = captured
            .lines()
            .filter(|l| l.contains("storage_events delivery") && l.contains("category="))
            .collect();
        assert_eq!(
            delivery.len(),
            1,
            "delivery lines:\n{}",
            delivery.join("\n")
        );
        assert!(
            delivery[0].contains("installation_id=prod-primary-01"),
            "{}",
            delivery[0]
        );
        assert!(delivery[0].contains("cancelled"), "{}", delivery[0]);
    }

    fn enabled_config() -> Config {
        let mut config = crate::config::testing::isolated_config(
            std::env::temp_dir().join("storage-event-emitter"),
        );
        config.monorepo.push_policy = PushPolicy::Trunk;
        config.git.push_auth = Some(PushAuth::None);
        config.git.ssh_receive_pack = Some(false);
        config.cedar.enforcement = "off".to_string();
        config.storage_events.enabled = true;
        config.storage_events.installation_id = Some("prod-primary-01".to_string());
        config.storage_events.max_in_flight = 1;
        config.storage_events.shutdown_grace_seconds = 0;
        config
    }

    fn graceful_config() -> Config {
        let mut config = enabled_config();
        config.storage_events.shutdown_grace_seconds = 5;
        config
    }

    async fn wait_until<F: FnMut() -> bool>(mut condition: F) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if condition() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition within 2s");
    }

    /// Every shutdown path gets a bounded wait so a cleanup regression fails
    /// instead of hanging the suite.
    async fn shutdown_bounded(emitter: &StorageEventEmitter) {
        tokio::time::timeout(Duration::from_secs(3), emitter.shutdown())
            .await
            .expect("shutdown within 3s");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bounded_admission_and_reaping() {
        // Disabled emitters drop before any work; closed beats every other
        // admission disposition.
        let disabled = StorageEventEmitter::disabled();
        assert_eq!(
            disabled.try_emit(sample_event()),
            AdmissionDisposition::DroppedDisabled
        );
        shutdown_bounded(&disabled).await;
        assert_eq!(
            disabled.try_emit(sample_event()),
            AdmissionDisposition::DroppedClosed
        );

        // Projection and validation failures map to their own dispositions.
        let transport = probe(ProbeMode::Block);
        let config = enabled_config();
        let emitter = StorageEventEmitter::new_with_transport(
            &config,
            transport.clone(),
            vec![compiled_target("ops-main")],
        );
        assert_eq!(
            emitter.try_emit(invalid_scope_event()),
            AdmissionDisposition::DroppedInvalidScope
        );
        assert_eq!(
            emitter.try_emit(invalid_data_event()),
            AdmissionDisposition::DroppedInvalidEvent
        );
        assert_eq!(
            emitter.try_emit(oversized_event()),
            AdmissionDisposition::DroppedSize
        );

        // First send acquires the single permit before spawn; the next send
        // saturates capacity.
        let first = emitter.try_emit(sample_event());
        assert_eq!(
            first,
            AdmissionDisposition::Accepted {
                accepted_targets: 1,
                dropped_targets: 0,
            }
        );
        wait_posted(&transport).await;
        assert_eq!(
            emitter.try_emit(sample_event()),
            AdmissionDisposition::DroppedCapacity
        );
        assert!(emitter.in_flight_count() <= 1);

        // Completed tasks are reaped and joined by the lifecycle task without
        // any further emission; only then is the permit released.
        transport.release();
        wait_until(|| emitter.task_count() == 0).await;
        assert_eq!(emitter.completed_count(), 1);
        assert_eq!(emitter.in_flight_count(), 0);
        shutdown_bounded(&emitter).await;

        // Partial fan-out: two matching targets but a single permit, so one
        // target is accepted and the other dropped in the same call.
        let partial_transport = probe(ProbeMode::Block);
        let partial = StorageEventEmitter::new_with_transport(
            &config,
            partial_transport.clone(),
            vec![compiled_target("ops-main"), compiled_target("ops-backup")],
        );
        assert_eq!(
            partial.try_emit(sample_event()),
            AdmissionDisposition::Accepted {
                accepted_targets: 1,
                dropped_targets: 1,
            }
        );
        wait_posted(&partial_transport).await;
        partial_transport.release();
        // Join first, then the zero-grace shutdown has nothing to abort.
        wait_until(|| partial.task_count() == 0).await;
        shutdown_bounded(&partial).await;
        assert_eq!(partial.completed_count(), 1);
        assert_eq!(partial.cancelled_count(), 0);

        // Graceful drain: nonzero grace lets an in-flight task finish, and
        // shutdown returns only after the task was joined and destroyed.
        let graceful_transport = probe(ProbeMode::Block);
        let graceful = StorageEventEmitter::new_with_transport(
            &graceful_config(),
            graceful_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            graceful.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&graceful_transport).await;
        let shutdown = tokio::spawn({
            let graceful = graceful.clone();
            async move { graceful.shutdown().await }
        });
        wait_until(|| graceful.drain_active()).await;
        assert!(!shutdown.is_finished());
        graceful_transport.release();
        tokio::time::timeout(Duration::from_secs(3), shutdown)
            .await
            .expect("graceful shutdown bounded")
            .expect("graceful shutdown task");
        assert_eq!(graceful.task_count(), 0);
        assert_eq!(graceful.completed_count(), 1);
        assert_eq!(graceful.cancelled_count(), 0);
        assert_eq!(graceful.in_flight_count(), 0);
        assert_eq!(
            graceful.try_emit(sample_event()),
            AdmissionDisposition::DroppedClosed
        );

        // Expired grace aborts and joins the remaining task, recording a
        // sanitized cancelled outcome and releasing the in-flight slot even
        // though the task never returned from post().
        let abort_transport = probe(ProbeMode::Block);
        let abort_emitter = StorageEventEmitter::new_with_transport(
            &config,
            abort_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            abort_emitter.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&abort_transport).await;
        let started = Instant::now();
        shutdown_bounded(&abort_emitter).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(abort_emitter.task_count(), 0);
        assert_eq!(abort_emitter.cancelled_count(), 1);
        assert_eq!(abort_emitter.in_flight_count(), 0);
        assert_eq!(
            abort_emitter.try_emit(sample_event()),
            AdmissionDisposition::DroppedClosed
        );

        // Aborting with an unreleased blocking transport always ends in
        // exactly one cancellation, whether or not the task was polled first;
        // the blocking post can never complete here.
        let early_transport = probe(ProbeMode::Block);
        let early = StorageEventEmitter::new_with_transport(
            &config,
            early_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            early.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        shutdown_bounded(&early).await;
        assert_eq!(early.task_count(), 0);
        assert_eq!(early.in_flight_count(), 0);
        assert_eq!(early.completed_count(), 0);
        assert_eq!(early.cancelled_count(), 1);

        // Transport errors are completion outcomes, not panics or retries.
        let failing = probe(ProbeMode::Fail);
        let failing_emitter = StorageEventEmitter::new_with_transport(
            &config,
            failing.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            failing_emitter.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&failing).await;
        failing.release();
        wait_until(|| failing_emitter.task_count() == 0).await;
        shutdown_bounded(&failing_emitter).await;
        assert_eq!(failing_emitter.completed_count(), 1);
        assert_eq!(failing_emitter.cancelled_count(), 0);
        assert_eq!(failing_emitter.panicked_count(), 0);
        assert_eq!(posts(&failing), 1);

        // A panicking transport is reaped by the lifecycle task and recorded
        // separately from cancellations.
        let panicking = probe(ProbeMode::Panic);
        let panicking_emitter = StorageEventEmitter::new_with_transport(
            &graceful_config(),
            panicking.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            panicking_emitter.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&panicking).await;
        panicking.release();
        wait_until(|| panicking_emitter.panicked_count() == 1).await;
        shutdown_bounded(&panicking_emitter).await;
        assert_eq!(panicking_emitter.task_count(), 0);
        assert_eq!(panicking_emitter.cancelled_count(), 0);
        assert_eq!(panicking_emitter.in_flight_count(), 0);

        // Repeated and concurrent shutdown during an in-flight task all
        // observe the same completion.
        let concurrent_transport = probe(ProbeMode::Block);
        let concurrent = StorageEventEmitter::new_with_transport(
            &graceful_config(),
            concurrent_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            concurrent.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&concurrent_transport).await;
        let first_shutdown = tokio::spawn({
            let concurrent = concurrent.clone();
            async move { concurrent.shutdown().await }
        });
        let second_shutdown = tokio::spawn({
            let concurrent = concurrent.clone();
            async move { concurrent.shutdown().await }
        });
        // Both callers are parked in the shutdown wait loop of the same
        // drain; the waiter count is maintained inside shutdown() itself and
        // the lifecycle publishes drain entry separately, so wait for both
        // in a single condition.
        wait_until(|| concurrent.shutdown_waiter_count() == 2 && concurrent.drain_active()).await;
        assert!(!first_shutdown.is_finished());
        assert!(!second_shutdown.is_finished());
        concurrent_transport.release();
        tokio::time::timeout(Duration::from_secs(3), first_shutdown)
            .await
            .expect("first shutdown bounded")
            .expect("first shutdown task");
        tokio::time::timeout(Duration::from_secs(3), second_shutdown)
            .await
            .expect("second shutdown bounded")
            .expect("second shutdown task");
        shutdown_bounded(&concurrent).await;
        assert_eq!(concurrent.completed_count(), 1);
        assert_eq!(concurrent.cancelled_count(), 0);
        assert_eq!(concurrent.task_count(), 0);

        // Cancelling the shutdown initiator leaves the shared drain running;
        // a later caller observes the same completion.
        let orphan_transport = probe(ProbeMode::Block);
        let orphan = StorageEventEmitter::new_with_transport(
            &graceful_config(),
            orphan_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            orphan.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&orphan_transport).await;
        let initiator = tokio::spawn({
            let orphan = orphan.clone();
            async move { orphan.shutdown().await }
        });
        wait_until(|| orphan.drain_active()).await;
        assert!(orphan.shutdown_initiated());
        initiator.abort();
        let _ = initiator.await;
        orphan_transport.release();
        shutdown_bounded(&orphan).await;
        assert_eq!(orphan.task_count(), 0);
        assert_eq!(orphan.completed_count(), 1);
        assert_eq!(orphan.cancelled_count(), 0);

        // A poisoned gate recovers: admission and cleanup keep working.
        let poisoned_transport = probe(ProbeMode::Block);
        let poisoned = StorageEventEmitter::new_with_transport(
            &config,
            poisoned_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        let gate_ref = &poisoned.inner.gate;
        let poisoned_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = gate_ref.lock().expect("lock before poisoning");
            panic!("deliberate poison for cleanup test");
        }));
        assert!(poisoned_panic.is_err());
        assert!(matches!(
            poisoned.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&poisoned_transport).await;
        poisoned_transport.release();
        wait_until(|| poisoned.task_count() == 0).await;
        shutdown_bounded(&poisoned).await;
        assert_eq!(poisoned.completed_count(), 1);

        // Concurrent close and admission never panic and always settle.
        // Producers emit until they observe the close, so the shutdown is
        // guaranteed to overlap with live producers.
        let race_transport = probe(ProbeMode::Block);
        let mut race_config = enabled_config();
        race_config.storage_events.shutdown_grace_seconds = 2;
        let race = StorageEventEmitter::new_with_transport(
            &race_config,
            race_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        // Producers only terminate by observing the close, so every producer
        // is alive across the shutdown and the overlap is proven.
        let mut emitters = Vec::new();
        for _ in 0..4 {
            let race = race.clone();
            emitters.push(tokio::spawn(async move {
                let mut accepted = 0usize;
                loop {
                    match race.try_emit(sample_event()) {
                        AdmissionDisposition::Accepted { .. } => accepted += 1,
                        AdmissionDisposition::DroppedCapacity => {}
                        AdmissionDisposition::DroppedClosed => break,
                        other => panic!("unexpected disposition during close race: {other:?}"),
                    }
                    tokio::task::yield_now().await;
                }
                accepted
            }));
        }
        wait_posted(&race_transport).await;
        race_transport.release();
        shutdown_bounded(&race).await;
        let mut saw_accepted = 0usize;
        for emitter in emitters {
            saw_accepted += tokio::time::timeout(Duration::from_secs(3), emitter)
                .await
                .expect("producer loop bounded")
                .expect("producer task");
        }
        assert!(saw_accepted > 0, "close race must include admitted sends");
        assert_eq!(race.task_count(), 0);
        assert_eq!(race.in_flight_count(), 0);

        // Final-owner drop with no shutdown running aborts the in-flight
        // task through the lifecycle task.
        let dropped_transport = probe(ProbeMode::Block);
        let dropped = StorageEventEmitter::new_with_transport(
            &config,
            dropped_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            dropped.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&dropped_transport).await;
        drop(dropped);
        wait_until(|| dropped_transport.cancelled.load(Ordering::SeqCst)).await;

        // Owner loss during an existing graceful drain interrupts the grace
        // wait: the in-flight task is aborted and joined immediately.
        let mid_drain_transport = probe(ProbeMode::Block);
        let mid_drain = StorageEventEmitter::new_with_transport(
            &graceful_config(),
            mid_drain_transport.clone(),
            vec![compiled_target("ops-main")],
        );
        assert!(matches!(
            mid_drain.try_emit(sample_event()),
            AdmissionDisposition::Accepted { .. }
        ));
        wait_posted(&mid_drain_transport).await;
        let initiator = tokio::spawn({
            let mid_drain = mid_drain.clone();
            async move { mid_drain.shutdown().await }
        });
        wait_until(|| mid_drain.drain_active()).await;
        initiator.abort();
        let _ = initiator.await;
        // Dropping the last owner mid-drain must not wait out the grace.
        let started = Instant::now();
        drop(mid_drain);
        wait_until(|| mid_drain_transport.cancelled.load(Ordering::SeqCst)).await;
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
