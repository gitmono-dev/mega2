use std::sync::{Once, OnceLock};

use idgenerator::*;

/// Eight worker bits provide 256 independently leased worker slots.
pub const WORKER_ID_BIT_LEN: u8 = 8;
/// Keep the existing 256 IDs/ms per worker sequence capacity.
pub const SEQ_BIT_LEN: u8 = 8;
/// `idgenerator` shifts the timestamp above worker and sequence bits.
pub const TIMESTAMP_SHIFT: u8 = WORKER_ID_BIT_LEN + SEQ_BIT_LEN;
pub const MAX_WORKER_ID: u32 = (1 << WORKER_ID_BIT_LEN) - 1;
pub const ENV_WORKER_ID: &str = "MEGA_ID_GENERATOR_WORKER_ID";

static ID_GENERATOR_INIT: Once = Once::new();
static CLAIMED_WORKER_ID: OnceLock<u32> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerIdSource {
    Env,
    Redis,
    Hash,
}

/// Record a Redis-claimed worker ID. Must run before [`set_up_options`].
pub fn claim_worker_id(id: u32) -> bool {
    id <= MAX_WORKER_ID && CLAIMED_WORKER_ID.set(id).is_ok()
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

pub fn env_worker_id_is_valid() -> bool {
    std::env::var(ENV_WORKER_ID)
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .is_some_and(|id| id <= MAX_WORKER_ID)
}

pub fn resolve_worker_id_from(
    env_val: Option<&str>,
    claimed: Option<u32>,
    identity: &str,
) -> (u32, WorkerIdSource) {
    if let Some(raw) = env_val {
        match raw.parse::<u32>() {
            Ok(id) if id <= MAX_WORKER_ID => return (id, WorkerIdSource::Env),
            _ => {
                tracing::warn!(
                    raw,
                    max = MAX_WORKER_ID,
                    "ignoring out-of-range or invalid MEGA_ID_GENERATOR_WORKER_ID"
                );
            }
        }
    }
    if let Some(id) = claimed {
        return (id.min(MAX_WORKER_ID), WorkerIdSource::Redis);
    }
    (hash_worker_id(identity), WorkerIdSource::Hash)
}

/// Ensures [`IdInstance`] is configured (idempotent; safe if [`set_up_options`] already ran, e.g. via [`crate::jupiter::storage::init::database_connection`]).
pub fn ensure_initialized() {
    ID_GENERATOR_INIT.call_once(|| {
        if let Err(e) = set_up_options() {
            tracing::debug!(
                ?e,
                "id_generator::set_up_options (ignored if already initialized)"
            );
        }
    });
}

pub fn set_up_options() -> Result<(), OptionError> {
    let (worker_id, source) = resolve_worker_id();
    let identity = process_identity();
    let options = IdGeneratorOptions::new()
        .worker_id(worker_id)
        .worker_id_bit_len(WORKER_ID_BIT_LEN)
        .seq_bit_len(SEQ_BIT_LEN);

    IdInstance::init(options)?;

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
}
