use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Default)]
pub(crate) struct ViewMetrics {
    pub(crate) view_batch_premise_failures_total: Arc<AtomicU64>,
}

impl ViewMetrics {
    pub(crate) fn increment_batch_premise_failures(&self) {
        self.view_batch_premise_failures_total
            .fetch_add(1, Ordering::Relaxed);
    }
}
