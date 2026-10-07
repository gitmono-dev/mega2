//! Ownership-based consumer memory admission. Backend allocations and allocator
//! overhead are separate; these credits are not a process RSS guarantee.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicUsize, Ordering},
};

use super::error::{SnapshotError, SnapshotErrorCode};

pub(crate) const PROJECTION_LIVE_BYTES: usize = 1024 * 1024 * 1024;
const RESPONSE_LIVE_BYTES: usize = 512 * 1024 * 1024;
const RANGE_SCRATCH_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const RANGE_WORK_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct MemoryBudget {
    limit: usize,
    used: AtomicUsize,
}

impl MemoryBudget {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
        })
    }

    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> Result<MemoryLease, SnapshotError> {
        if bytes > self.limit {
            return Err(SnapshotError::new(
                SnapshotErrorCode::LimitExceeded,
                "content operation exceeds its consumer memory budget",
            ));
        }
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            if bytes > self.limit - used {
                return Err(SnapshotError::new(
                    SnapshotErrorCode::TemporaryUnavailable,
                    "content consumer memory budget is occupied",
                ));
            }
            match self.used.compare_exchange_weak(
                used,
                used + bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(MemoryLease {
                        budget: self.clone(),
                        bytes,
                    });
                }
                Err(actual) => used = actual,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

pub(crate) struct MemoryLease {
    budget: Arc<MemoryBudget>,
    pub(crate) bytes: usize,
}

impl Drop for MemoryLease {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

pub(crate) fn projection_budget() -> &'static Arc<MemoryBudget> {
    static BUDGET: OnceLock<Arc<MemoryBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| MemoryBudget::new(PROJECTION_LIVE_BYTES))
}

pub(crate) fn reserve_response(bytes: usize) -> Result<MemoryLease, SnapshotError> {
    response_budget().reserve(bytes)
}

pub(crate) fn response_budget() -> &'static Arc<MemoryBudget> {
    static BUDGET: OnceLock<Arc<MemoryBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| MemoryBudget::new(RESPONSE_LIVE_BYTES))
}

pub(crate) fn reserve_range_work() -> Result<MemoryLease, SnapshotError> {
    range_budget().reserve(RANGE_WORK_BYTES)
}

pub(crate) fn range_budget() -> &'static Arc<MemoryBudget> {
    static BUDGET: OnceLock<Arc<MemoryBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| MemoryBudget::new(RANGE_SCRATCH_BYTES))
}

/// Every encoded frame owns shared credit, including after the body passes a
/// frame to a transport that retains or clones its Bytes.
pub(crate) struct BudgetedFrame {
    pub(crate) bytes: Vec<u8>,
    pub(crate) lease: Arc<MemoryLease>,
}

impl AsRef<[u8]> for BudgetedFrame {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_retain_credit_until_last_transport_clone_drops() {
        let budget = MemoryBudget::new(64);
        let lease = Arc::new(budget.reserve(64).unwrap());
        let frame = bytes::Bytes::from_owner(BudgetedFrame {
            bytes: vec![7; 32],
            lease: lease.clone(),
        });
        let transport = frame.clone();
        drop(lease);
        drop(frame);
        assert_eq!(budget.used(), 64);
        assert!(budget.reserve(1).is_err());
        drop(transport);
        assert_eq!(budget.used(), 0);
        assert!(budget.reserve(64).is_ok());
    }

    #[test]
    fn quota_rejects_without_waiting_for_a_callers_own_credits() {
        let budget = MemoryBudget::new(12);
        let first = budget.reserve(8).unwrap();
        assert_eq!(
            budget.reserve(5).err().unwrap().code,
            SnapshotErrorCode::TemporaryUnavailable
        );
        assert_eq!(
            budget.reserve(13).err().unwrap().code,
            SnapshotErrorCode::LimitExceeded
        );
        drop(first);
        assert_eq!(budget.used(), 0);
    }
}
