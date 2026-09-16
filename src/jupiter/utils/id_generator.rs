use std::sync::{Once, OnceLock};

use idgenerator::*;

/// Current production layout (`idgenerator` 2.0.0 requires
/// `worker_id_bit_len + seq_bit_len <= 22`). Mega's 8+8 target is crate-legal
/// (shift 16, 256 workers) but cannot mix with these 6+8 writers
/// (ADR-FC-04). This process keeps 6+8 (64 workers, shift 14) and adds
/// dynamic worker selection instead of changing the ID bit layout.
pub const WORKER_ID_BIT_LEN: u8 = 6;
pub const SEQ_BIT_LEN: u8 = 8;
pub const MAX_WORKER_ID: u32 = (1 << WORKER_ID_BIT_LEN) - 1;
pub const ENV_WORKER_ID: &str = "MEGA_ID_GENERATOR_WORKER_ID";

/// Crate-legal Mega target layout. Not used while old 6+8 writers may still run.
pub const MEGA_TARGET_WORKER_ID_BIT_LEN: u8 = 8;
pub const MEGA_TARGET_SEQ_BIT_LEN: u8 = 8;

static ID_GENERATOR_INIT: Once = Once::new();
static CLAIMED_WORKER_ID: OnceLock<u32> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerIdSource {
    Env,
    Redis,
    Hash,
}

/// Record a Redis-claimed worker id. Must run before [`set_up_options`].
/// Returns false if a claim was already stored.
pub fn claim_worker_id(id: u32) -> bool {
    CLAIMED_WORKER_ID.set(id.min(MAX_WORKER_ID)).is_ok()
}

pub fn process_identity() -> String {
    std::env::var("POD_UID")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "mega2-local".to_string())
}

/// FNV-1a 32-bit. Stable across rustc versions (unlike `DefaultHasher`).
pub fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for b in bytes {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
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

/// Ensures [`IdInstance`] is configured (idempotent; safe if [`set_up_options`] already ran).
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
    let options = IdGeneratorOptions::new()
        .worker_id(worker_id)
        .worker_id_bit_len(WORKER_ID_BIT_LEN)
        .seq_bit_len(SEQ_BIT_LEN);

    IdInstance::init(options)?;

    tracing::info!(
        worker_id,
        worker_id_bit_len = WORKER_ID_BIT_LEN,
        seq_bit_len = SEQ_BIT_LEN,
        timestamp_shift = WORKER_ID_BIT_LEN + SEQ_BIT_LEN,
        ?source,
        identity = %process_identity(),
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
    fn crate_budget_allows_mega_8_plus_8_target() {
        const {
            assert!(MEGA_TARGET_WORKER_ID_BIT_LEN + MEGA_TARGET_SEQ_BIT_LEN <= 22);
            assert!(MEGA_TARGET_WORKER_ID_BIT_LEN + MEGA_TARGET_SEQ_BIT_LEN == 16);
        };
    }

    #[test]
    fn retained_layout_is_6_plus_8() {
        const {
            assert!(WORKER_ID_BIT_LEN == 6);
            assert!(SEQ_BIT_LEN == 8);
            assert!(WORKER_ID_BIT_LEN + SEQ_BIT_LEN == 14);
            assert!(MAX_WORKER_ID == 63);
        };
    }

    #[test]
    fn hash_is_stable_and_in_range() {
        let a = hash_worker_id("mono-engine-5f6d8d7cc9-45tx8");
        let b = hash_worker_id("mono-engine-5f6d8d7cc9-rknxw");
        assert_eq!(a, hash_worker_id("mono-engine-5f6d8d7cc9-45tx8"));
        assert_ne!(a, b);
        assert!(a <= MAX_WORKER_ID);
        assert!(b <= MAX_WORKER_ID);
    }
}
