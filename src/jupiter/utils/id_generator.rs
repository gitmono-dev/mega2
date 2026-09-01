use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use idgenerator::{CoreIdGenerator, IdGeneratorOptions, OptionError};

use crate::common::errors::MegaError;

/// Eight worker bits provide 256 independently leased worker slots.
pub const WORKER_ID_BIT_LEN: u8 = 8;
/// Keep the existing 256 IDs/ms per worker sequence capacity.
pub const SEQ_BIT_LEN: u8 = 8;
/// `idgenerator` shifts the timestamp above worker and sequence bits.
pub const TIMESTAMP_SHIFT: u8 = WORKER_ID_BIT_LEN + SEQ_BIT_LEN;
pub const MAX_WORKER_ID: u32 = (1 << WORKER_ID_BIT_LEN) - 1;
pub const ENV_WORKER_ID: &str = "MEGA_ID_GENERATOR_WORKER_ID";

static CLAIMED_WORKER_ID: OnceLock<u32> = OnceLock::new();
static GENERATOR_STATE: OnceLock<Mutex<GeneratorState>> = OnceLock::new();
static MONOTONIC_EPOCH: OnceLock<Instant> = OnceLock::new();

const WORKER_LEASE_TTL_MS: u64 = 30_000;
const WORKER_LEASE_SAFETY_MARGIN_MS: u64 = 5_000;

struct GeneratorState {
    generator: Option<CoreIdGenerator>,
    worker_id: Option<u32>,
    source: Option<WorkerIdSource>,
    lease_health: Option<Arc<WorkerLeaseHealth>>,
}

impl Default for GeneratorState {
    fn default() -> Self {
        Self {
            generator: None,
            worker_id: None,
            source: None,
            lease_health: None,
        }
    }
}

/// Shared process-local health state for the Redis worker lease.
///
/// The refresh task updates both fields. ID generation also checks the local
/// deadline, so a delayed refresh task cannot leave the process generating IDs
/// for the whole Redis TTL after the lease should have been considered stale.
pub(crate) struct WorkerLeaseHealth {
    healthy: AtomicBool,
    revoked: AtomicBool,
    deadline_ms: AtomicU64,
}

impl WorkerLeaseHealth {
    pub(crate) fn claimed() -> Arc<Self> {
        let health = Arc::new(Self {
            healthy: AtomicBool::new(false),
            revoked: AtomicBool::new(false),
            deadline_ms: AtomicU64::new(0),
        });
        health.refresh_deadline();
        health
    }

    pub(crate) fn activate(&self) -> bool {
        if self.revoked.load(Ordering::Acquire) {
            return false;
        }

        let now = monotonic_millis();
        let deadline = self.deadline_ms.load(Ordering::Acquire);
        if deadline != 0 && now >= deadline {
            self.lost();
            return false;
        }
        if deadline == 0 {
            self.refresh_deadline();
        }
        self.healthy.store(true, Ordering::Release);
        true
    }

    pub(crate) fn refreshed(&self) -> bool {
        if self.revoked.load(Ordering::Acquire) {
            return false;
        }

        let now = monotonic_millis();
        let deadline = self.deadline_ms.load(Ordering::Acquire);
        if deadline != 0 && now >= deadline {
            self.lost();
            return false;
        }
        self.refresh_deadline();
        self.healthy.store(true, Ordering::Release);
        true
    }

    pub(crate) fn lost(&self) {
        self.revoked.store(true, Ordering::Release);
        self.healthy.store(false, Ordering::Release);
        self.deadline_ms.store(0, Ordering::Release);
    }

    pub(crate) fn is_healthy(&self) -> bool {
        let healthy = self.healthy.load(Ordering::Acquire);
        let deadline = self.deadline_ms.load(Ordering::Acquire);
        let within_deadline = deadline != 0 && monotonic_millis() < deadline;
        if healthy && !within_deadline {
            self.lost();
        }
        healthy && within_deadline
    }

    fn refresh_deadline(&self) {
        let deadline = monotonic_millis()
            .saturating_add(WORKER_LEASE_TTL_MS.saturating_sub(WORKER_LEASE_SAFETY_MARGIN_MS));
        self.deadline_ms.store(deadline, Ordering::Release);
    }

    #[cfg(test)]
    fn expire(&self) {
        self.deadline_ms.store(1, Ordering::Release);
        while monotonic_millis() < 1 {
            std::thread::yield_now();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerIdSource {
    Env,
    Redis,
    Hash,
}

/// Record a Redis-claimed worker ID for the legacy standalone initializer.
/// Production bootstrap uses [`initialize_worker`] so the lease and generator
/// are bound while holding one process-wide initialization lock.
pub fn claim_worker_id(id: u32) -> bool {
    id <= MAX_WORKER_ID && CLAIMED_WORKER_ID.set(id).is_ok()
}

/// Generate an ID while the process still owns its Redis worker lease.
///
/// Once a lease is lost, refusing to generate further IDs is safer than
/// continuing with a worker ID that another process may already have claimed.
pub(crate) fn next_id() -> Result<i64, MegaError> {
    #[cfg(test)]
    ensure_test_initialized();

    let mut state = lock_generator_state();
    next_id_from_state(&mut state)
}

fn next_id_from_state(state: &mut GeneratorState) -> Result<i64, MegaError> {
    let source = state.source.ok_or_else(|| {
        MegaError::IdGenerationUnavailable("worker ID has not been initialized".to_string())
    })?;

    if source == WorkerIdSource::Hash {
        return Err(MegaError::IdGenerationUnavailable(
            "an exclusive worker ID is required; stable identity hash is diagnostic-only"
                .to_string(),
        ));
    }

    if source == WorkerIdSource::Redis && state.lease_health.is_none() {
        return Err(MegaError::IdGenerationUnavailable(
            "Redis worker selection is missing its active lease".to_string(),
        ));
    }

    if let Some(health) = state.lease_health.as_ref()
        && !health.is_healthy()
    {
        return Err(MegaError::IdGenerationUnavailable(
            "Redis worker lease was lost or expired".to_string(),
        ));
    }

    let generator = state.generator.as_mut().ok_or_else(|| {
        MegaError::IdGenerationUnavailable("ID generator has not been initialized".to_string())
    })?;
    Ok(generator.next_id())
}

fn generator_state() -> &'static Mutex<GeneratorState> {
    GENERATOR_STATE.get_or_init(|| Mutex::new(GeneratorState::default()))
}

fn lock_generator_state() -> std::sync::MutexGuard<'static, GeneratorState> {
    match generator_state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

pub(crate) fn refresh_worker_lease(health: &Arc<WorkerLeaseHealth>) -> bool {
    let mut state = lock_generator_state();
    refresh_worker_lease_in_state(&mut state, health)
}

fn refresh_worker_lease_in_state(
    state: &mut GeneratorState,
    health: &Arc<WorkerLeaseHealth>,
) -> bool {
    let is_bound = state
        .lease_health
        .as_ref()
        .is_some_and(|bound| Arc::ptr_eq(bound, health));
    if !is_bound {
        health.lost();
        return false;
    }
    health.refreshed()
}

pub(crate) fn revoke_worker_lease(health: &Arc<WorkerLeaseHealth>) {
    let mut state = lock_generator_state();
    revoke_worker_lease_in_state(&mut state, health);
}

fn revoke_worker_lease_in_state(_state: &mut GeneratorState, health: &Arc<WorkerLeaseHealth>) {
    health.lost();
}

fn monotonic_millis() -> u64 {
    MONOTONIC_EPOCH
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis() as u64
}

fn options_for_worker(worker_id: u32) -> IdGeneratorOptions {
    IdGeneratorOptions::new()
        .worker_id(worker_id)
        .worker_id_bit_len(WORKER_ID_BIT_LEN)
        .seq_bit_len(SEQ_BIT_LEN)
}

fn build_generator(worker_id: u32) -> Result<CoreIdGenerator, MegaError> {
    let mut generator = CoreIdGenerator::default();
    generator
        .init(options_for_worker(worker_id))
        .map_err(|error| MegaError::IdGenerationUnavailable(error.to_string()))?;
    Ok(generator)
}

fn bind_generator_state(
    state: &mut GeneratorState,
    worker_id: u32,
    source: WorkerIdSource,
    lease_health: Option<Arc<WorkerLeaseHealth>>,
    generator: CoreIdGenerator,
) -> Result<(), MegaError> {
    if let Some(existing_worker_id) = state.worker_id {
        let same_lease = match (&state.lease_health, &lease_health) {
            (Some(existing), Some(requested)) => Arc::ptr_eq(existing, requested),
            (None, None) => true,
            _ => false,
        };
        if existing_worker_id != worker_id || state.source != Some(source) || !same_lease {
            return Err(MegaError::IdGenerationUnavailable(
                "ID generator is already bound to a different worker selection".to_string(),
            ));
        }
        if let Some(health) = state.lease_health.as_ref() {
            if !health.activate() {
                return Err(MegaError::IdGenerationUnavailable(
                    "Redis worker lease expired before ID generator initialization".to_string(),
                ));
            }
        }
        return Ok(());
    }

    if let Some(health) = lease_health.as_ref() {
        if !health.activate() {
            return Err(MegaError::IdGenerationUnavailable(
                "Redis worker lease expired before ID generator initialization".to_string(),
            ));
        }
    }
    state.generator = Some(generator);
    state.worker_id = Some(worker_id);
    state.source = Some(source);
    state.lease_health = lease_health;
    Ok(())
}

/// Bind the selected worker ID, its lease health, and the generator in one
/// process-wide critical section. A second, incompatible selection fails
/// instead of silently combining one process's lease with another ID.
pub(crate) fn initialize_worker(
    worker_id: u32,
    source: WorkerIdSource,
    lease_health: Option<Arc<WorkerLeaseHealth>>,
) -> Result<(), MegaError> {
    if worker_id > MAX_WORKER_ID {
        return Err(MegaError::IdGenerationUnavailable(format!(
            "worker ID {worker_id} is outside 0..={MAX_WORKER_ID}"
        )));
    }
    if source == WorkerIdSource::Redis && lease_health.is_none() {
        return Err(MegaError::IdGenerationUnavailable(
            "Redis worker selection requires an active lease".to_string(),
        ));
    }
    if source != WorkerIdSource::Redis && lease_health.is_some() {
        return Err(MegaError::IdGenerationUnavailable(
            "only Redis worker selections may carry a lease".to_string(),
        ));
    }

    let generator = build_generator(worker_id)?;

    let mut state = lock_generator_state();
    bind_generator_state(&mut state, worker_id, source, lease_health, generator)?;

    tracing::info!(
        worker_id,
        ?source,
        process_identity = %identity_digest(&process_identity()),
        "snowflake worker selection bound to ID generator"
    );
    Ok(())
}

pub fn process_identity() -> String {
    std::env::var("POD_UID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "mono-local".to_string())
}

/// FNV-1a 32-bit. Stable across rustc versions (unlike `DefaultHasher`).
pub fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

pub fn identity_digest(identity: &str) -> String {
    format!("{:08x}", fnv1a_32(identity.as_bytes()))
}

pub fn hash_worker_id(identity: &str) -> u32 {
    fnv1a_32(identity.as_bytes()) % (MAX_WORKER_ID + 1)
}

pub fn resolve_worker_id() -> (u32, WorkerIdSource) {
    resolve_worker_id_from(
        std::env::var(ENV_WORKER_ID).ok().as_deref(),
        CLAIMED_WORKER_ID.get().copied(),
        &process_identity(),
    )
}

fn parse_env_worker_id(raw: &str) -> Option<u32> {
    raw.parse::<u32>().ok().filter(|id| *id <= MAX_WORKER_ID)
}

/// Return the explicitly configured worker ID, logging invalid input once at
/// the boundary that consumes the environment variable.
pub fn configured_env_worker_id() -> Option<u32> {
    let Some(raw) = std::env::var(ENV_WORKER_ID).ok() else {
        return None;
    };
    if let Some(id) = parse_env_worker_id(&raw) {
        return Some(id);
    }

    tracing::warn!(
        raw_len = raw.len(),
        max = MAX_WORKER_ID,
        "ignoring out-of-range or invalid MEGA_ID_GENERATOR_WORKER_ID"
    );
    None
}

pub fn env_worker_id_is_valid() -> bool {
    configured_env_worker_id().is_some()
}

pub fn resolve_worker_id_from(
    env_val: Option<&str>,
    claimed: Option<u32>,
    identity: &str,
) -> (u32, WorkerIdSource) {
    if let Some(raw) = env_val {
        if let Some(id) = parse_env_worker_id(raw) {
            return (id, WorkerIdSource::Env);
        }
        tracing::warn!(
            raw_len = raw.len(),
            max = MAX_WORKER_ID,
            "ignoring out-of-range or invalid MEGA_ID_GENERATOR_WORKER_ID"
        );
    }
    if let Some(id) = claimed {
        return (id.min(MAX_WORKER_ID), WorkerIdSource::Redis);
    }
    (hash_worker_id(identity), WorkerIdSource::Hash)
}

/// Ensures the standalone generator is configured (idempotent). This legacy
/// path has no distributed lease and is therefore only suitable for a
/// single-writer process; production [`crate::context::AppContext`] binds a
/// Redis lease before allowing IDs to be generated.
pub fn ensure_initialized() -> Result<(), MegaError> {
    {
        let state = lock_generator_state();
        if state.worker_id.is_some() {
            if state.source == Some(WorkerIdSource::Redis) {
                let Some(health) = state.lease_health.as_ref() else {
                    return Err(MegaError::IdGenerationUnavailable(
                        "Redis worker selection is missing its active lease".to_string(),
                    ));
                };
                if !health.is_healthy() {
                    return Err(MegaError::IdGenerationUnavailable(
                        "Redis worker lease was lost or expired".to_string(),
                    ));
                }
            }
            return Ok(());
        }
    }

    let (worker_id, source) = resolve_worker_id();
    if source == WorkerIdSource::Hash {
        return Err(MegaError::IdGenerationUnavailable(
            "an exclusive worker ID is required; stable identity hash is diagnostic-only"
                .to_string(),
        ));
    }
    if source == WorkerIdSource::Redis {
        return Err(MegaError::IdGenerationUnavailable(
            "Redis worker selection requires an active lease".to_string(),
        ));
    }
    initialize_worker(worker_id, source, None)
}

pub fn set_up_options() -> Result<(), OptionError> {
    let (worker_id, source) = resolve_worker_id();
    if source != WorkerIdSource::Env {
        return Err(OptionError::InvalidWorkerId(
            "standalone ID generation requires MEGA_ID_GENERATOR_WORKER_ID".to_string(),
        ));
    }
    let generator = build_generator(worker_id)
        .map_err(|error| OptionError::InvalidWorkerId(error.to_string()))?;

    let mut state = lock_generator_state();
    if let Some(existing_worker_id) = state.worker_id {
        if existing_worker_id != worker_id || state.source != Some(source) {
            return Err(OptionError::InvalidWorkerId(
                "ID generator is already bound to another worker selection".to_string(),
            ));
        }
        return Ok(());
    }
    bind_generator_state(&mut state, worker_id, source, None, generator)
        .map_err(|error| OptionError::InvalidWorkerId(error.to_string()))?;

    let identity = process_identity();

    tracing::info!(
        worker_id,
        worker_id_bit_len = WORKER_ID_BIT_LEN,
        seq_bit_len = SEQ_BIT_LEN,
        timestamp_shift = TIMESTAMP_SHIFT,
        ?source,
        process_identity = %identity_digest(&identity),
        "snowflake id generator initialized"
    );
    Ok(())
}

#[cfg(test)]
static TEST_ID_GENERATOR_INIT: OnceLock<()> = OnceLock::new();

#[cfg(test)]
pub(crate) fn ensure_test_initialized() {
    TEST_ID_GENERATOR_INIT.get_or_init(|| {
        let state = lock_generator_state();
        if state.worker_id.is_some() {
            return;
        }
        drop(state);
        initialize_worker(0, WorkerIdSource::Env, None)
            .expect("test ID generator should bind a deterministic worker");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_in_range_wins() {
        let (id, source) = resolve_worker_id_from(Some("7"), Some(3), "pod-a");
        assert_eq!(id, 7);
        assert_eq!(source, WorkerIdSource::Env);
    }

    #[test]
    fn env_out_of_range_falls_through_to_claimed() {
        let too_big = (MAX_WORKER_ID + 1).to_string();
        let (id, source) = resolve_worker_id_from(Some(&too_big), Some(3), "pod-a");
        assert_eq!(id, 3);
        assert_eq!(source, WorkerIdSource::Redis);
    }

    #[test]
    fn env_invalid_falls_through_to_hash() {
        let expected = hash_worker_id("fixture-pod");
        let (id, source) = resolve_worker_id_from(Some("abc"), None, "fixture-pod");
        assert_eq!(id, expected);
        assert_eq!(source, WorkerIdSource::Hash);
    }

    #[test]
    fn missing_env_without_claim_hashes_identity() {
        let (id, source) = resolve_worker_id_from(None, None, "fixture-pod");
        assert_eq!(id, hash_worker_id("fixture-pod"));
        assert_eq!(source, WorkerIdSource::Hash);
        assert!(id <= MAX_WORKER_ID);
    }

    #[test]
    fn worker_layout_matches_idgenerator_budget() {
        assert_eq!(WORKER_ID_BIT_LEN + SEQ_BIT_LEN, 16);
        assert!(u16::from(WORKER_ID_BIT_LEN) + u16::from(SEQ_BIT_LEN) <= 22);
        assert_eq!(TIMESTAMP_SHIFT, 16);
        assert_eq!(MAX_WORKER_ID, 255);
        assert_eq!(1 << WORKER_ID_BIT_LEN, 256);
        assert_eq!(1 << SEQ_BIT_LEN, 256);
    }

    #[test]
    fn generated_id_contains_the_configured_worker_bits() {
        let mut generator = build_generator(MAX_WORKER_ID).expect("valid test worker ID");
        let id = generator.next_id();
        let worker_bits = (id >> SEQ_BIT_LEN) & i64::from(MAX_WORKER_ID);

        assert_eq!(worker_bits, i64::from(MAX_WORKER_ID));
    }

    #[test]
    fn hash_is_stable_and_bounded_for_distinct_identities() {
        let first = hash_worker_id("mono-engine-5f6d8d7cc9-45tx");
        let second = hash_worker_id("mono-engine-5f6d8d7cc9-rknxw");

        assert_eq!(first, hash_worker_id("mono-engine-5f6d8d7cc9-45tx"));
        assert_eq!(second, hash_worker_id("mono-engine-5f6d8d7cc9-rknxw"));
        assert_ne!(first, second);
        assert!(first <= MAX_WORKER_ID);
        assert!(second <= MAX_WORKER_ID);
    }

    #[test]
    fn claim_worker_id_rejects_invalid_ids() {
        assert!(!claim_worker_id(MAX_WORKER_ID + 1));
    }

    #[test]
    fn worker_lease_health_gate_is_fail_closed() {
        let health = WorkerLeaseHealth::claimed();
        health.activate();
        assert!(health.is_healthy());

        health.lost();
        assert!(!health.is_healthy());
    }

    #[test]
    fn worker_lease_health_gate_expires_before_redis_ttl() {
        let health = WorkerLeaseHealth::claimed();
        health.activate();
        health.expire();

        assert!(!health.is_healthy());
    }

    #[test]
    fn claimed_lease_rejects_activation_after_local_deadline() {
        let health = WorkerLeaseHealth::claimed();
        health.expire();

        assert!(!health.activate());
        assert!(!health.is_healthy());
    }

    #[test]
    fn redis_worker_binding_requires_a_lease() {
        let error = initialize_worker(7, WorkerIdSource::Redis, None)
            .expect_err("Redis worker IDs must be lease-backed");

        assert!(error.to_string().contains("active lease"));
    }

    #[test]
    fn production_id_gate_stops_after_lease_loss() {
        let health = WorkerLeaseHealth::claimed();
        health.activate();
        let mut state = GeneratorState {
            generator: Some(build_generator(7).expect("valid test worker ID")),
            worker_id: Some(7),
            source: Some(WorkerIdSource::Redis),
            lease_health: Some(health.clone()),
        };

        assert!(next_id_from_state(&mut state).is_ok());

        health.lost();
        let error = next_id_from_state(&mut state).expect_err("lost lease must stop ID writes");
        assert!(
            matches!(error, MegaError::IdGenerationUnavailable(message) if message.contains("lost or expired"))
        );
    }

    #[test]
    fn hash_selection_is_diagnostic_only() {
        let mut state = GeneratorState {
            generator: Some(build_generator(7).expect("valid test worker ID")),
            worker_id: Some(7),
            source: Some(WorkerIdSource::Hash),
            lease_health: None,
        };

        let error = next_id_from_state(&mut state).expect_err("hash selection must fail closed");
        assert!(
            matches!(error, MegaError::IdGenerationUnavailable(message) if message.contains("diagnostic-only"))
        );
    }

    #[test]
    fn worker_selection_binding_rejects_incompatible_second_context() {
        let health = WorkerLeaseHealth::claimed();
        let mut state = GeneratorState::default();
        bind_generator_state(
            &mut state,
            7,
            WorkerIdSource::Redis,
            Some(health.clone()),
            build_generator(7).expect("valid test worker ID"),
        )
        .expect("first selection should bind");

        let error = bind_generator_state(
            &mut state,
            8,
            WorkerIdSource::Redis,
            Some(WorkerLeaseHealth::claimed()),
            build_generator(8).expect("valid test worker ID"),
        )
        .expect_err("a second context must not combine another worker and lease");
        assert!(
            matches!(error, MegaError::IdGenerationUnavailable(message) if message.contains("different worker selection"))
        );
        assert_eq!(state.worker_id, Some(7));
        assert!(Arc::ptr_eq(
            state.lease_health.as_ref().expect("lease bound"),
            &health
        ));
    }

    #[test]
    fn late_refresh_cannot_reactivate_a_revoked_lease() {
        let health = WorkerLeaseHealth::claimed();
        let mut state = GeneratorState::default();
        bind_generator_state(
            &mut state,
            7,
            WorkerIdSource::Redis,
            Some(health.clone()),
            build_generator(7).expect("valid test worker ID"),
        )
        .expect("worker lease should bind");

        assert!(refresh_worker_lease_in_state(&mut state, &health));
        revoke_worker_lease_in_state(&mut state, &health);
        assert!(!refresh_worker_lease_in_state(&mut state, &health));
        assert!(next_id_from_state(&mut state).is_err());
    }

    #[test]
    fn unbound_lease_cannot_refresh_or_activate_id_generation() {
        let health = WorkerLeaseHealth::claimed();
        let mut state = GeneratorState::default();

        assert!(!refresh_worker_lease_in_state(&mut state, &health));
        assert!(!health.is_healthy());
    }
}
