use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Default)]
pub(crate) struct ViewMetrics {
    pub(crate) view_batch_premise_failures_total: Arc<AtomicU64>,
    pub(crate) view_projection_stops_total: Arc<AtomicU64>,
    pub(crate) view_pack_tree_mismatch_total: Arc<AtomicU64>,
}

impl ViewMetrics {
    pub(crate) fn increment_batch_premise_failures(&self) {
        self.view_batch_premise_failures_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_projection_stops(&self) {
        self.view_projection_stops_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_pack_tree_mismatch(&self) {
        self.view_pack_tree_mismatch_total
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn counters(&self) -> ViewMetricCounters {
        ViewMetricCounters {
            view_batch_premise_failures_total: self
                .view_batch_premise_failures_total
                .load(Ordering::Relaxed),
            view_projection_stops_total: self.view_projection_stops_total.load(Ordering::Relaxed),
            view_pack_tree_mismatch_total: self
                .view_pack_tree_mismatch_total
                .load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ViewMetricCounters {
    pub(crate) view_batch_premise_failures_total: u64,
    pub(crate) view_projection_stops_total: u64,
    pub(crate) view_pack_tree_mismatch_total: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ViewMetricsSnapshot {
    #[serde(flatten)]
    pub(crate) counters: ViewMetricCounters,
    pub(crate) view_cold_start_slots_in_use: u64,
}
