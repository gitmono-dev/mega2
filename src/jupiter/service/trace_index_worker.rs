//! Background indexer for the durable trace ref journal.
//!
//! The worker reads only `trace_ref_update`. It does not watch the Git
//! transport, and it does not move `trace_stream.tip_oid`.

use std::time::Duration;

use sea_orm::DatabaseConnection;
use tokio_util::sync::CancellationToken;

use crate::{
    common::errors::MegaError,
    jupiter::{
        service::trace_object_service::TraceObjectService,
        storage::trace_projection_storage::TraceProjectionStore,
    },
};

/// How often a running service looks for a new journal row.
pub const INDEX_INTERVAL: Duration = Duration::from_millis(200);

/// Process-wide indexer started with the service.
#[derive(Clone)]
pub struct TraceIndexWorker {
    db: DatabaseConnection,
    objects: TraceObjectService,
}

impl TraceIndexWorker {
    pub fn new(db: DatabaseConnection, objects: TraceObjectService) -> Self {
        Self { db, objects }
    }

    pub fn spawn_background(self, shutdown: CancellationToken) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(INDEX_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if shutdown.is_cancelled() {
                            break;
                        }
                        if let Err(error) = self.index_once().await {
                            tracing::error!(error = %error, "trace index worker cycle failed");
                        }
                    }
                }
            }
        });
    }

    pub async fn index_once(&self) -> Result<usize, MegaError> {
        TraceProjectionStore::new(&self.db, &self.objects)
            .index_available()
            .await
    }
}
