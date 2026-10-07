//! Bounded exact-source install flights. Completed data is always re-read
//! from the requesting source's trusted receipt, never retained in a flight.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};

use super::error::{SnapshotError, SnapshotErrorCode};

const MAX_INSTALL_FLIGHTS: usize = 128;
type SourceGate = tokio::sync::Mutex<()>;

#[derive(Default)]
struct Registry {
    entries: HashMap<[u8; 32], Weak<SourceGate>>,
}

pub(crate) struct InstallFlight {
    key: [u8; 32],
    gate: Option<Arc<SourceGate>>,
    registry: Arc<Mutex<Registry>>,
}

impl InstallFlight {
    pub(crate) fn acquire(key: [u8; 32]) -> Result<Self, SnapshotError> {
        static REGISTRY: OnceLock<Arc<Mutex<Registry>>> = OnceLock::new();
        Self::from_registry(REGISTRY.get_or_init(Arc::default).clone(), key)
    }

    fn from_registry(registry: Arc<Mutex<Registry>>, key: [u8; 32]) -> Result<Self, SnapshotError> {
        let gate = {
            let mut state = registry.lock().map_err(|_| internal())?;
            if let Some(gate) = state.entries.get(&key).and_then(Weak::upgrade) {
                gate
            } else {
                state.entries.remove(&key);
                if state.entries.len() >= MAX_INSTALL_FLIGHTS {
                    return Err(SnapshotError::new(
                        SnapshotErrorCode::LimitExceeded,
                        "too many distinct source map installations in flight",
                    ));
                }
                let gate = Arc::new(SourceGate::new(()));
                state.entries.insert(key, Arc::downgrade(&gate));
                gate
            }
        };
        Ok(Self {
            key,
            gate: Some(gate),
            registry,
        })
    }

    pub(crate) async fn lock(&self) -> Result<tokio::sync::MutexGuard<'_, ()>, SnapshotError> {
        Ok(self.gate.as_ref().ok_or_else(internal)?.lock().await)
    }

    #[cfg(test)]
    pub(crate) fn test_owner_count(&self) -> usize {
        self.gate.as_ref().map_or(0, Arc::strong_count)
    }
}

impl Drop for InstallFlight {
    fn drop(&mut self) {
        let Some(gate) = self.gate.take() else {
            return;
        };
        if let Ok(mut state) = self.registry.lock() {
            if Arc::strong_count(&gate) == 1
                && state
                    .entries
                    .get(&self.key)
                    .is_some_and(|entry| entry.ptr_eq(&Arc::downgrade(&gate)))
            {
                state.entries.remove(&self.key);
            }
            // Owners must release their strong reference while the registry
            // is locked, so concurrent final drops cannot leave a stale slot.
            drop(gate);
        }
    }
}

fn internal() -> SnapshotError {
    SnapshotError::new(
        SnapshotErrorCode::Internal,
        "source map flight registry is unavailable",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_source_gate_blocks_only_same_source_and_returns_capacity_after_last_owner() {
        let registry = Arc::new(Mutex::new(Registry::default()));
        let first = InstallFlight::from_registry(registry.clone(), [1; 32]).unwrap();
        let lock = first.lock().await.unwrap();
        let same = InstallFlight::from_registry(registry.clone(), [1; 32]).unwrap();
        assert!(same.gate.as_ref().unwrap().try_lock().is_err());
        let other = InstallFlight::from_registry(registry.clone(), [2; 32]).unwrap();
        assert!(other.gate.as_ref().unwrap().try_lock().is_ok());
        drop(lock);
        drop(first);
        assert!(same.gate.as_ref().unwrap().try_lock().is_ok());
        assert_eq!(registry.lock().unwrap().entries.len(), 2);
        drop(same);
        drop(other);
        assert!(registry.lock().unwrap().entries.is_empty());
        let mut held = Vec::new();
        for i in 0..MAX_INSTALL_FLIGHTS {
            let mut key = [0; 32];
            key[..8].copy_from_slice(&(i as u64).to_le_bytes());
            held.push(InstallFlight::from_registry(registry.clone(), key).unwrap());
        }
        assert_eq!(
            InstallFlight::from_registry(registry.clone(), [255; 32])
                .err()
                .unwrap()
                .code,
            SnapshotErrorCode::LimitExceeded
        );
        let joined_at_capacity = InstallFlight::from_registry(registry.clone(), [0; 32]).unwrap();
        assert_eq!(joined_at_capacity.test_owner_count(), 2);
        assert_eq!(registry.lock().unwrap().entries.len(), MAX_INSTALL_FLIGHTS);
        drop(joined_at_capacity);
        drop(held);
        assert!(registry.lock().unwrap().entries.is_empty());
        assert!(InstallFlight::from_registry(registry, [255; 32]).is_ok());
    }

    #[test]
    fn concurrent_last_owners_release_registry_capacity() {
        for _ in 0..64 {
            let registry = Arc::new(Mutex::new(Registry::default()));
            let first = InstallFlight::from_registry(registry.clone(), [1; 32]).unwrap();
            let second = InstallFlight::from_registry(registry.clone(), [1; 32]).unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            std::thread::scope(|scope| {
                let other = barrier.clone();
                scope.spawn(move || {
                    other.wait();
                    drop(first);
                });
                scope.spawn(move || {
                    barrier.wait();
                    drop(second);
                });
            });
            assert!(registry.lock().unwrap().entries.is_empty());
        }
    }

    #[test]
    fn dropping_an_old_claim_does_not_remove_a_replacement_gate() {
        let registry = Arc::new(Mutex::new(Registry::default()));
        let old = InstallFlight::from_registry(registry.clone(), [1; 32]).unwrap();
        let replacement = Arc::new(SourceGate::new(()));
        registry
            .lock()
            .unwrap()
            .entries
            .insert([1; 32], Arc::downgrade(&replacement));
        drop(old);
        assert_eq!(registry.lock().unwrap().entries.len(), 1);
        let current = InstallFlight::from_registry(registry.clone(), [1; 32]).unwrap();
        assert!(Arc::ptr_eq(current.gate.as_ref().unwrap(), &replacement));
        drop(replacement);
        drop(current);
        assert!(registry.lock().unwrap().entries.is_empty());
        registry
            .lock()
            .unwrap()
            .entries
            .insert([1; 32], Weak::new());
        let reclaimed = InstallFlight::from_registry(registry.clone(), [1; 32]).unwrap();
        assert_eq!(registry.lock().unwrap().entries.len(), 1);
        drop(reclaimed);
        assert!(registry.lock().unwrap().entries.is_empty());
    }
}
