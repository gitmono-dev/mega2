//! TP-11: startup reconcile and periodic inspect of `refs/heads/main` tree hashes.
//!
//! Holds `MONO_WRITE_LOCK` so a scan cannot race B3. Kind-specific missing-row
//! rules (attach skip, push create mid-state, merge missing-row warn) stay on
//! the reaper I3 path; this module only compares existing `refs/heads/main` rows
//! against `resolve(root, P)`. `main@/` is skipped (it is the resolve source).

use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait};
use tokio_util::sync::CancellationToken;

use crate::{
    callisto::mega_refs,
    common::{errors::MegaError, utils::MEGA_BRANCH_NAME},
    jupiter::{
        service::push_queue_service::PushQueueService,
        storage::{base_storage::StorageConnector, push_queue_storage::PushQueueStorage},
    },
};

/// Background inspect interval (not a `[monorepo]` key; TP-15 owns config).
pub const DEFAULT_INSPECT_INTERVAL: Duration = Duration::from_secs(30);
/// Rows compared per lock hold.
pub const DEFAULT_INSPECT_BATCH: u64 = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectReport {
    pub lock_acquired: bool,
    pub compared: u64,
    pub tombstoned: u64,
}

#[derive(Clone)]
pub struct MonoWriteAudit {
    service: PushQueueService,
    batch_size: u64,
    /// Wrap-around cursor for inspect (`id > cursor`). Reconcile ignores this.
    cursor: Arc<AtomicI64>,
}

impl MonoWriteAudit {
    pub fn from_service(service: PushQueueService) -> Self {
        Self {
            service,
            batch_size: DEFAULT_INSPECT_BATCH,
            cursor: Arc::new(AtomicI64::new(0)),
        }
    }

    pub fn with_batch_size(mut self, batch_size: u64) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// Startup full scan: successive batches until a short page.
    pub async fn reconcile_once(&self) -> Result<InspectReport, MegaError> {
        let mut total = InspectReport {
            lock_acquired: true,
            ..InspectReport::default()
        };
        let mut after = 0i64;
        loop {
            let page = self.scan_page(after, true).await?;
            if !page.lock_acquired {
                total.lock_acquired = false;
                break;
            }
            total.compared += page.compared;
            total.tombstoned += page.tombstoned;
            if page.compared < self.batch_size {
                break;
            }
            after = page.last_id;
            if after == 0 {
                break;
            }
        }
        Ok(total)
    }

    /// One batched inspect page, advancing the wrap-around cursor.
    pub async fn inspect_once(&self) -> Result<InspectReport, MegaError> {
        let after = self.cursor.load(Ordering::Relaxed);
        let page = self.scan_page(after, false).await?;
        if page.lock_acquired {
            if page.compared < self.batch_size {
                self.cursor.store(0, Ordering::Relaxed);
            } else {
                self.cursor.store(page.last_id, Ordering::Relaxed);
            }
        }
        Ok(InspectReport {
            lock_acquired: page.lock_acquired,
            compared: page.compared,
            tombstoned: page.tombstoned,
        })
    }

    /// Reconcile once, then inspect on `interval` until `shutdown`.
    pub fn spawn_background(self, interval: Duration, shutdown: CancellationToken) {
        tokio::spawn(async move {
            if shutdown.is_cancelled() {
                return;
            }
            if let Err(e) = self.reconcile_once().await {
                tracing::error!(
                    error = %e,
                    event = "mono_write_reconcile_failed",
                    "startup tree-hash reconcile failed"
                );
            }
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if shutdown.is_cancelled() {
                            break;
                        }
                        if let Err(e) = self.inspect_once().await {
                            tracing::error!(
                                error = %e,
                                event = "mono_write_inspect_failed",
                                "tree-hash inspect cycle failed"
                            );
                        }
                    }
                }
            }
        });
    }

    async fn scan_page(&self, after: i64, is_reconcile: bool) -> Result<Page, MegaError> {
        let conn = self.service.storage().get_connection();
        let txn = conn.begin().await?;
        if !PushQueueStorage::try_mono_write_lock(&txn).await? {
            txn.rollback().await?;
            return Ok(Page {
                lock_acquired: false,
                compared: 0,
                tombstoned: 0,
                last_id: after,
            });
        }

        let Some(root) = self
            .service
            .mono_storage_for_reaper()
            .get_main_ref_in_txn("/", &txn)
            .await?
        else {
            txn.rollback().await?;
            return Ok(Page {
                lock_acquired: true,
                compared: 0,
                tombstoned: 0,
                last_id: 0,
            });
        };
        let mono = self.service.mono_storage_for_reaper();
        if mono
            .get_tree_by_hash_in_txn(&root.ref_tree_hash, &txn)
            .await?
            .is_none()
        {
            tracing::warn!(
                event = "mono_write_scan_root_tree_missing",
                "skip inspect page; root tree object not persisted"
            );
            txn.rollback().await?;
            return Ok(Page {
                lock_acquired: true,
                compared: 0,
                tombstoned: 0,
                last_id: after,
            });
        }

        let rows = mega_refs::Entity::find()
            .filter(mega_refs::Column::RefName.eq(MEGA_BRANCH_NAME.to_owned()))
            .filter(mega_refs::Column::IsCl.eq(false))
            .filter(mega_refs::Column::Path.ne("/".to_owned()))
            .filter(mega_refs::Column::Id.gt(after))
            .order_by_asc(mega_refs::Column::Id)
            .limit(self.batch_size)
            .all(&txn)
            .await?;

        let compared = rows.len() as u64;
        let last_id = rows.last().map(|r| r.id).unwrap_or(0);
        let mut tombstoned = 0u64;
        for row in &rows {
            if self
                .service
                .assert_main_path_tree_hash_in_txn(&txn, &row.path, &root.ref_tree_hash)
                .await?
                .is_none()
            {
                continue;
            }
            mono.tombstone_and_delete_main_ref_in_txn(&row.path, &txn)
                .await?;
            tombstoned += 1;
        }

        if tombstoned > 0 {
            let counter = if is_reconcile {
                &self.service.metrics().reconcile_tombstoned
            } else {
                &self.service.metrics().inspect_tombstoned
            };
            counter.fetch_add(tombstoned, Ordering::Relaxed);
            tracing::info!(
                event = "mono_write_tree_hash_scan",
                compared,
                tombstoned,
                reconcile = is_reconcile,
                "stale main refs tombstoned"
            );
        }

        txn.commit().await?;
        Ok(Page {
            lock_acquired: true,
            compared,
            tombstoned,
            last_id,
        })
    }
}

struct Page {
    lock_acquired: bool,
    compared: u64,
    tombstoned: u64,
    last_id: i64,
}
