//! Bounded primary owners and exact physical-generation reconciliation.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use tokio::sync::Mutex;
use uuid::Uuid;

use super::*;
use crate::orbit_api::{
    error::IoOrbitError,
    object_storage::{ChunkMapReceiptDeletion, ChunkMapReceiptInventory},
};

const RENEW_INTERVAL: Duration = Duration::from_secs(10);
const HISTORY_CAP: i64 = 131_072;
const MAINTENANCE_MAX: usize = 64;
const OWNER_TTL: Duration = Duration::from_secs(59);

struct OwnerDeadline(std::sync::Mutex<Instant>);

impl OwnerDeadline {
    fn from_sql_start(started: Instant) -> Self {
        // The SQL INSERT/renew sets clock_timestamp()+59s after this sample.
        // Counting SQL and commit latency against the TTL is conservative.
        Self(std::sync::Mutex::new(started + OWNER_TTL))
    }

    fn get(&self) -> Result<Instant, SnapshotError> {
        let deadline = *self
            .0
            .lock()
            .map_err(|_| integrity("owner deadline is poisoned"))?;
        if deadline <= Instant::now() {
            return Err(expired("chunk-map ownership deadline elapsed"));
        }
        Ok(deadline)
    }

    fn observe(&self, deadline: Instant, renewed: bool) -> Result<(), SnapshotError> {
        let mut previous = self
            .0
            .lock()
            .map_err(|_| integrity("owner deadline is poisoned"))?;
        *previous = if renewed {
            deadline
        } else {
            (*previous).min(deadline)
        };
        if *previous <= Instant::now() {
            return Err(expired("chunk-map ownership deadline elapsed"));
        }
        Ok(())
    }

    async fn bound<F: std::future::Future>(&self, future: F) -> Result<F::Output, SnapshotError> {
        let deadline = self.get()?;
        let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
            .await
            .map_err(|_| expired("chunk-map ownership deadline elapsed"))?;
        self.get()?;
        Ok(result)
    }
}

fn remaining_deadline(started: Instant, row: &QueryResult) -> Result<Instant, SnapshotError> {
    let remaining = row.try_get::<i64>("", "remaining_us").map_err(db_error)?;
    let remaining = u64::try_from(remaining)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| expired("chunk-map database ownership deadline elapsed"))?;
    started
        .checked_add(Duration::from_micros(remaining))
        .ok_or_else(|| integrity("chunk-map ownership deadline is not representable"))
}

fn retention_budget() -> &'static Arc<MemoryBudget> {
    // Inventory survives requests. Its separate process-wide credits bound
    // independent repositories without retaining source or response credit.
    static BUDGET: OnceLock<Arc<MemoryBudget>> = OnceLock::new();
    BUDGET.get_or_init(|| MemoryBudget::new(128 * 1024 * 1024))
}

pub(super) struct BackingObservation {
    inventory: ChunkMapReceiptInventory,
    started_at: String,
    captured: Instant,
    backing: MegaObjectStorageWrapper,
    _memory: MemoryLease,
}

pub(crate) struct ChunkMapReader {
    repository: PostgresChunkMapRepository,
    source: ChunkMapSource,
    owner: Uuid,
    receipt_key: String,
    generation: i64,
    map_id: [u8; 32],
    map_generation: i64,
    confirmed: Mutex<(Instant, Instant)>,
    deadline: OwnerDeadline,
}

impl ChunkMapReader {
    #[cfg(test)]
    pub(crate) async fn test_check_next_owner_operation(&self) {
        let before = Instant::now() - Duration::from_secs(11);
        *self.confirmed.lock().await = (before, before);
    }
    pub(crate) async fn ensure_live(&self) -> Result<(), SnapshotError> {
        self.deadline.get()?;
        let mut confirmed = self.confirmed.lock().await;
        self.deadline.get()?;
        if confirmed.0.elapsed() < RENEW_INTERVAL {
            return Ok(());
        }
        let deadline = self.deadline.bound(self.check(false)).await??;
        self.deadline.observe(deadline, false)?;
        confirmed.0 = Instant::now();
        Ok(())
    }

    pub(super) async fn record_progress(&self) -> Result<(), SnapshotError> {
        self.deadline.get()?;
        let mut confirmed = self.confirmed.lock().await;
        self.deadline.get()?;
        if confirmed.1.elapsed() < RENEW_INTERVAL {
            return Ok(());
        }
        let deadline = self.deadline.bound(self.check(true)).await??;
        self.deadline.observe(deadline, true)?;
        *confirmed = (Instant::now(), Instant::now());
        Ok(())
    }

    pub(super) async fn validate_after_receipt(&self) -> Result<(), SnapshotError> {
        let mut confirmed = self.confirmed.lock().await;
        self.deadline.get()?;
        let deadline = self.deadline.bound(self.check(true)).await??;
        self.deadline.observe(deadline, true)?;
        *confirmed = (Instant::now(), Instant::now());
        Ok(())
    }

    pub(crate) async fn await_backend<F: std::future::Future>(
        &self,
        future: F,
    ) -> Result<F::Output, SnapshotError> {
        self.ensure_live().await?;
        tokio::pin!(future);
        loop {
            let deadline = self.deadline.get()?;
            let check_at = self.confirmed.lock().await.0 + RENEW_INTERVAL;
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    return Err(expired("chunk-map reader backend wait outlived its ownership"));
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(check_at)) => {
                    self.ensure_live().await?;
                }
                value = &mut future => {
                    self.deadline.get()?;
                    return Ok(value);
                }
            }
        }
    }

    async fn check(&self, renew: bool) -> Result<Instant, SnapshotError> {
        let started = Instant::now();
        let txn = self.repository.transaction().await?;
        let result = async {
            self.repository.require_scope(&txn).await?;
            require_current_source(&txn, &self.source, &self.repository.schema).await?;
            barrier(&txn, &self.repository.schema).await?;
            let row = if renew {txn.query_one_raw(stmt(&self.repository.schema,
                "UPDATE mst2_chunk_reader SET deadline=pg_catalog.clock_timestamp()+interval '59 seconds',last_progress=pg_catalog.clock_timestamp() WHERE owner=$1::uuid AND receipt_key=$2 AND source_generation=$3 AND map_id=$4 AND map_generation=$5 AND deadline>pg_catalog.clock_timestamp() RETURNING owner",
                [self.owner.to_string().into(),self.receipt_key.clone().into(),self.generation.into(),self.map_id.to_vec().into(),self.map_generation.into()])).await.map_err(db_error)?} else {
                    txn.query_one_raw(stmt(&self.repository.schema,"SELECT r.owner FROM mst2_chunk_reader r JOIN mst2_chunk_receipt_generation g ON g.receipt_key=r.receipt_key AND g.generation=r.source_generation JOIN mst2_chunk_map_lifetime l ON l.map_id=r.map_id AND l.generation=r.map_generation WHERE r.owner=$1::uuid AND r.receipt_key=$2 AND r.source_generation=$3 AND r.map_id=$4 AND r.map_generation=$5 AND r.deadline>pg_catalog.clock_timestamp() AND g.state='LIVE' AND l.state='LIVE'",[self.owner.to_string().into(),self.receipt_key.clone().into(),self.generation.into(),self.map_id.to_vec().into(),self.map_generation.into()])).await.map_err(db_error)?
                };
            if row.is_none() { return Err(expired("chunk-map reader expired before progress resumed")); }
            if renew {txn.execute_raw(stmt(&self.repository.schema,
                "UPDATE mst2_chunk_map_lifetime SET last_used=pg_catalog.clock_timestamp() WHERE map_id=$1 AND generation=$2 AND state='LIVE'",
                [self.map_id.to_vec().into(),self.map_generation.into()])).await.map_err(db_error)?;
            txn.execute_raw(stmt(&self.repository.schema,"UPDATE mst2_chunk_receipt_generation SET last_progress=pg_catalog.clock_timestamp() WHERE receipt_key=$1 AND generation=$2 AND state='LIVE'",[self.receipt_key.clone().into(),self.generation.into()])).await.map_err(db_error)?;
            }
            let remaining = txn.query_one_raw(stmt(&self.repository.schema,
                "SELECT pg_catalog.floor(EXTRACT(EPOCH FROM (deadline-pg_catalog.clock_timestamp()))*1000000)::bigint AS remaining_us FROM mst2_chunk_reader WHERE owner=$1::uuid",
                [self.owner.to_string().into()])).await.map_err(db_error)?
                .ok_or_else(|| expired("chunk-map reader deadline disappeared"))?;
            remaining_deadline(started, &remaining)
        }.await;
        finish(txn, result).await
    }
}

impl Drop for ChunkMapReader {
    fn drop(&mut self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let repository = self.repository.clone();
        let owner = self.owner;
        runtime.spawn(async move {
            let result = async {
                let txn = repository.transaction().await?;
                let result = async {
                    repository.require_scope(&txn).await?;
                    barrier(&txn, &repository.schema).await?;
                    txn.execute_raw(stmt(
                        &repository.schema,
                        "DELETE FROM mst2_chunk_reader WHERE owner=$1::uuid",
                        [owner.to_string().into()],
                    ))
                    .await
                    .map_err(db_error)?;
                    Ok(())
                }
                .await;
                finish(txn, result).await
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(
                    ?error,
                    "chunk-map reader release will expire by database deadline"
                );
            }
        });
    }
}

/// A capacity reservation exists before the verifier opens the source body.
/// It cannot publish source trust; installation still consumes the opaque
/// complete-stream verifier and independently checks the backend receipt.
pub(crate) struct ChunkMapInstall {
    repository: PostgresChunkMapRepository,
    source: ChunkMapSource,
    owner: Uuid,
    key: ObjectKey,
    generation: i64,
    progress: Mutex<(Instant, Instant, u64)>,
    deadline: OwnerDeadline,
}

#[derive(Clone)]
pub(super) struct CancelledInstall {
    owner: Uuid,
    key: String,
    generation: i64,
}

impl ChunkMapInstall {
    #[cfg(test)]
    pub(crate) async fn test_check_next_owner_operation(&self) {
        let mut progress = self.progress.lock().await;
        let before = Instant::now() - Duration::from_secs(11);
        progress.0 = before;
    }
    pub(crate) async fn ensure_live(&self) -> Result<(), SnapshotError> {
        self.renew(false, 0).await
    }

    pub(crate) async fn record_progress(&self, completed: u64) -> Result<(), SnapshotError> {
        self.renew(true, completed).await
    }

    pub(crate) async fn await_backend<F: std::future::Future>(
        &self,
        future: F,
    ) -> Result<F::Output, SnapshotError> {
        self.ensure_live().await?;
        tokio::pin!(future);
        loop {
            let deadline = self.deadline.get()?;
            let check_at = self.progress.lock().await.0 + RENEW_INTERVAL;
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    return Err(expired("chunk-map install backend wait outlived its ownership"));
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(check_at)) => {
                    self.ensure_live().await?;
                }
                value = &mut future => {
                    self.deadline.get()?;
                    return Ok(value);
                }
            }
        }
    }

    async fn renew(&self, progress: bool, completed: u64) -> Result<(), SnapshotError> {
        self.deadline.get()?;
        let mut confirmed = self.progress.lock().await;
        self.deadline.get()?;
        if progress && completed <= confirmed.2 {
            return Ok(());
        }
        if (if progress {
            confirmed.1.elapsed()
        } else {
            confirmed.0.elapsed()
        }) < RENEW_INTERVAL
        {
            if progress {
                confirmed.2 = completed;
            }
            return Ok(());
        }
        // Calling ensure_live after a long stalled await checks the old
        // deadline without extending it. Only new source progress renews it.
        let started = Instant::now();
        let deadline = self.deadline.bound(async {
        let txn = self.repository.transaction().await?;
        let result=async {
            self.repository.require_scope(&txn).await?;
            require_current_source(&txn,&self.source,&self.repository.schema).await?;
            barrier(&txn,&self.repository.schema).await?;
            let row=if progress {
                txn.query_one_raw(stmt(&self.repository.schema,"UPDATE mst2_chunk_receipt_generation SET deadline=pg_catalog.clock_timestamp()+interval '59 seconds',last_progress=pg_catalog.clock_timestamp() WHERE receipt_key=$1 AND generation=$2 AND owner=$3::uuid AND state IN ('RESERVED','CREATING') AND deadline>pg_catalog.clock_timestamp() RETURNING generation",[self.key.key.clone().into(),self.generation.into(),self.owner.to_string().into()])).await.map_err(db_error)?
            } else {
                txn.query_one_raw(stmt(&self.repository.schema,"SELECT generation FROM mst2_chunk_receipt_generation WHERE receipt_key=$1 AND generation=$2 AND owner=$3::uuid AND state IN ('RESERVED','CREATING') AND deadline>pg_catalog.clock_timestamp()",[self.key.key.clone().into(),self.generation.into(),self.owner.to_string().into()])).await.map_err(db_error)?
            };
            if row.is_none() { return Err(expired("chunk-map install reservation expired or was retired")); }
            let remaining = txn.query_one_raw(stmt(&self.repository.schema,
                "SELECT pg_catalog.floor(EXTRACT(EPOCH FROM (deadline-pg_catalog.clock_timestamp()))*1000000)::bigint AS remaining_us FROM mst2_chunk_receipt_generation WHERE receipt_key=$1 AND generation=$2 AND owner=$3::uuid AND state IN ('RESERVED','CREATING')",
                [self.key.key.clone().into(),self.generation.into(),self.owner.to_string().into()])).await.map_err(db_error)?
                .ok_or_else(|| expired("chunk-map install deadline disappeared"))?;
            remaining_deadline(started, &remaining)
        }.await;
        finish(txn, result).await
        }).await??;
        self.deadline.observe(deadline, progress)?;
        if progress {
            *confirmed = (Instant::now(), Instant::now(), completed);
        } else {
            confirmed.0 = Instant::now();
        }
        Ok(())
    }

    pub(super) async fn prepare(
        &self,
        verified: &VerifiedSourceChunkMap,
    ) -> Result<Vec<u8>, SnapshotError> {
        if verified.source() != &self.source {
            return Err(integrity(
                "verified source differs from install reservation",
            ));
        }
        let bytes = receipt(&self.repository.primary_scope, &self.source, verified.map())?;
        let started = Instant::now();
        self.deadline.bound(async {
        let txn = self.repository.transaction().await?;
        let result=async {
            self.repository.require_scope(&txn).await?;
            require_current_source(&txn,&self.source,&self.repository.schema).await?;
            barrier(&txn,&self.repository.schema).await?;
            let row=txn.query_one_raw(stmt(&self.repository.schema,"UPDATE mst2_chunk_receipt_generation SET state='CREATING',map_id=$4,receipt_bytes=$5,deadline=pg_catalog.clock_timestamp()+interval '59 seconds',last_progress=pg_catalog.clock_timestamp() WHERE receipt_key=$1 AND generation=$2 AND owner=$3::uuid AND state='RESERVED' AND deadline>pg_catalog.clock_timestamp() RETURNING generation",[self.key.key.clone().into(),self.generation.into(),self.owner.to_string().into(),verified.map().map_id().to_vec().into(),bytes.clone().into()])).await.map_err(db_error)?;
            if row.is_none() { return Err(expired("chunk-map install lost its exact reservation before receipt creation")); }
            Ok(())
        }.await;
        finish(txn, result).await?;
        Ok::<_, SnapshotError>(())
        }).await??;
        self.deadline.observe(started + OWNER_TTL, true)?;
        Ok(bytes)
    }

    pub(super) fn key(&self) -> &ObjectKey {
        &self.key
    }
    pub(super) fn generation(&self) -> i64 {
        self.generation
    }
    pub(super) fn owner(&self) -> Uuid {
        self.owner
    }

    pub(super) async fn acknowledge_create(&self) -> Result<(), SnapshotError> {
        // This method is called only after the actual atomic create future
        // returns success. Absence, a checksum or inventory cannot call it.
        let started = Instant::now();
        let renewed = self.deadline.bound(async {
        let txn = self.repository.transaction().await?;
        let result=async {
            self.repository.require_scope(&txn).await?;
            barrier(&txn,&self.repository.schema).await?;
            let row=txn.query_one_raw(stmt(&self.repository.schema,"UPDATE mst2_chunk_receipt_generation SET create_completed=true,create_completed_at=COALESCE(create_completed_at,pg_catalog.clock_timestamp()),deadline=CASE WHEN state='CREATING' AND owner=$3::uuid AND deadline>pg_catalog.clock_timestamp() THEN pg_catalog.clock_timestamp()+interval '59 seconds' ELSE deadline END WHERE receipt_key=$1 AND generation=$2 AND receipt_bytes IS NOT NULL RETURNING generation,CASE WHEN state='CREATING' AND owner=$3::uuid AND deadline>pg_catalog.clock_timestamp() THEN true ELSE false END AS deadline_renewed",[self.key.key.clone().into(),self.generation.into(),self.owner.to_string().into()])).await.map_err(db_error)?
                .ok_or_else(|| integrity("completed create lost its persistent generation fence"))?;
            row.try_get::<bool>("", "deadline_renewed").map_err(db_error)
        }.await;
        finish(txn, result).await
        }).await??;
        if renewed {
            self.deadline.observe(started + OWNER_TTL, true)?;
        }
        Ok(())
    }

    pub(super) async fn retire(&self) -> Result<(), SnapshotError> {
        retire_install(&self.repository, self.owner, &self.key.key, self.generation).await
    }
}

impl Drop for ChunkMapInstall {
    fn drop(&mut self) {
        if let Ok(mut cancelled) = self.repository.cancelled_installs.lock()
            && cancelled.len() < 64
        {
            cancelled.push(CancelledInstall {
                owner: self.owner,
                key: self.key.key.clone(),
                generation: self.generation,
            });
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let repository = self.repository.clone();
        let owner = self.owner;
        let key = self.key.key.clone();
        let generation = self.generation;
        runtime.spawn(async move {
            let result = retire_install(&repository, owner, &key, generation).await;
            if result.is_ok() {
                forget_cancelled_install(&repository, owner);
            }
            if let Err(error) = result {
                tracing::warn!(
                    ?error,
                    "chunk-map install cancellation will reconcile by database deadline"
                );
            }
        });
    }
}

fn forget_cancelled_install(repository: &PostgresChunkMapRepository, owner: Uuid) {
    if let Ok(mut cancelled) = repository.cancelled_installs.lock() {
        cancelled.retain(|install| install.owner != owner);
    }
}

async fn retire_install(
    repository: &PostgresChunkMapRepository,
    owner: Uuid,
    key: &str,
    generation: i64,
) -> Result<(), SnapshotError> {
    let txn = repository.transaction().await?;
    let result = async {
        repository.require_scope(&txn).await?;
        barrier(&txn, &repository.schema).await?;
        txn.execute_raw(stmt(&repository.schema,"UPDATE mst2_chunk_receipt_generation SET state=CASE WHEN receipt_bytes IS NULL THEN 'APPLIED' ELSE 'DELETING' END,owner=NULL,deadline=NULL,reserved_pages=0,reserved_nodes=0,reserved_bytes=0 WHERE receipt_key=$1 AND generation=$2 AND owner=$3::uuid AND state IN ('RESERVED','CREATING')",[key.into(),generation.into(),owner.to_string().into()])).await.map_err(db_error)?;
        Ok(())
    }.await;
    finish(txn, result).await
}

/// Constructor stays in the collector module. Its private fields carry a
/// real primary claim and the independently read exact backend body.
pub(crate) struct VerifiedReceiptDeletionClaim {
    key: ObjectKey,
    expected: Vec<u8>,
    _generation: i64,
}
impl VerifiedReceiptDeletionClaim {
    pub(crate) fn key(&self) -> &ObjectKey {
        &self.key
    }
    pub(crate) fn expected_bytes(&self) -> &[u8] {
        &self.expected
    }
}

impl PostgresChunkMapRepository {
    async fn replay_cancelled_installs(&self) -> Result<(), SnapshotError> {
        let cancelled = self
            .cancelled_installs
            .lock()
            .map_err(|_| integrity("cancelled install replay ownership is poisoned"))?
            .clone();
        for install in cancelled {
            retire_install(self, install.owner, &install.key, install.generation).await?;
            forget_cancelled_install(self, install.owner);
        }
        Ok(())
    }

    async fn maintain_before_admission(
        &self,
        objects: &MegaObjectStorageWrapper,
    ) -> Result<(), SnapshotError> {
        let mut completed = self.admission_maintenance.lock().await;
        self.replay_cancelled_installs().await?;
        if completed
            .as_ref()
            .is_some_and(|last| last.elapsed() < RENEW_INTERVAL)
        {
            let txn = self.transaction().await?;
            self.require_scope(&txn).await?;
            let pending=txn.query_one_raw(stmt(&self.schema,"SELECT EXISTS(SELECT 1 FROM mst2_chunk_receipt_generation WHERE state='DELETING' OR (state IN ('RESERVED','CREATING') AND deadline<=pg_catalog.clock_timestamp())) AS pending",[])).await.map_err(db_error)?.ok_or_else(||integrity("pending chunk-map admission replay observation missing"))?.try_get::<bool>("","pending").map_err(db_error)?;
            txn.commit().await.map_err(db_error)?;
            if !pending {
                return Ok(());
            }
        }
        self.maintain(objects, 8).await?;
        *completed = Some(Instant::now());
        Ok(())
    }
    #[cfg(test)]
    pub(crate) async fn test_new_install_capacity(
        &self,
        objects: &MegaObjectStorageWrapper,
    ) -> Result<(), SnapshotError> {
        let observation = inventory(self, objects).await?;
        let txn = self.transaction().await?;
        let result = async {
            self.require_scope(&txn).await?;
            barrier(&txn, &self.schema).await?;
            ensure_quotas(&txn, &self.schema, &observation, 1, 1, 1024 * 1024, None).await
        }
        .await;
        finish(txn, result).await
    }
    pub(super) async fn stage_map_start(
        &self,
        verified: &VerifiedSourceChunkMap,
        admission: &ChunkMapInstall,
        observation: &BackingObservation,
    ) -> Result<i64, SnapshotError> {
        let map = verified.map();
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            require_current_source(&txn,verified.source(),&self.schema).await?;
            map_barrier(&txn,map.map_id()).await?;
            barrier(&txn,&self.schema).await?;
            let inserted=txn.query_one_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_map(map_id,descriptor,page_count,pages_root) VALUES($1,$2,$3,$4) ON CONFLICT(map_id) DO NOTHING RETURNING map_id",[map.map_id().to_vec().into(),map.encode().into(),(map.page_count as i32).into(),map.pages_root.to_vec().into()])).await.map_err(db_error)?.is_some();
            let lifetime=txn.query_one_raw(stmt(&self.schema,"SELECT generation,state FROM mst2_chunk_map_lifetime WHERE map_id=$1",[map.map_id().to_vec().into()])).await.map_err(db_error)?;
            let generation=if let Some(row)=lifetime {
                if row.try_get::<String>("","state").map_err(db_error)?!="LIVE" {return Err(unavailable("shared chunk map collection is pending"));}
                row.try_get::<i64>("","generation").map_err(db_error)?
            } else if inserted {
                let generation=next_generation(&txn,&self.schema).await?;
                txn.execute_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_map_lifetime(map_id,generation,state) VALUES($1,$2,'LIVE')",[map.map_id().to_vec().into(),generation.into()])).await.map_err(db_error)?;
                generation
            } else {return Err(integrity("existing chunk map has no exact lifetime; never repair it"));};
            let row=txn.query_one_raw(stmt(&self.schema,"SELECT CASE WHEN pg_catalog.octet_length(descriptor)=100 THEN descriptor ELSE NULL END AS descriptor,page_count,CASE WHEN pg_catalog.octet_length(pages_root)=32 THEN pages_root ELSE NULL END AS pages_root FROM mst2_chunk_map WHERE map_id=$1",[map.map_id().to_vec().into()])).await.map_err(db_error)?.ok_or_else(||integrity("staged map descriptor missing"))?;
            if bounded_bytes(&row,"descriptor")?!=map.encode() || row.try_get::<i32>("","page_count").map_err(db_error)? as u64!=map.page_count || bounded_bytes(&row,"pages_root")?!=map.pages_root {return Err(integrity("immutable staged map descriptor conflicts with verified source"));}
            let assigned=txn.query_one_raw(stmt(&self.schema,"UPDATE mst2_chunk_receipt_generation SET map_generation=$4,deadline=pg_catalog.clock_timestamp()+interval '59 seconds',last_progress=pg_catalog.clock_timestamp() WHERE receipt_key=$1 AND generation=$2 AND owner=$3::uuid AND state='CREATING' AND deadline>pg_catalog.clock_timestamp() AND map_id=$5 AND create_completed RETURNING generation",[admission.key.key.clone().into(),admission.generation.into(),admission.owner.to_string().into(),generation.into(),map.map_id().to_vec().into()])).await.map_err(db_error)?;
            if assigned.is_none() {return Err(expired("map staging lost the exact completed-create reservation"));}
            ensure_quotas(&txn,&self.schema,observation,0,0,0,Some(admission.owner)).await?;
            Ok(generation)
        }.await;
        finish(txn, result).await
    }

    pub(super) async fn stage_map_batch(
        &self,
        verified: &VerifiedSourceChunkMap,
        admission: &ChunkMapInstall,
        generation: i64,
        leaves: Option<&[ChunkLeaf]>,
        nodes: Option<&[ChunkMapNode]>,
    ) -> Result<(), SnapshotError> {
        let map_id = verified.map().map_id();
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            require_current_source(&txn,verified.source(),&self.schema).await?;
            map_barrier(&txn,map_id).await?;
            let sealed=txn.query_one_raw(stmt(&self.schema,"SELECT EXISTS(SELECT 1 FROM mst2_chunk_map_source WHERE map_id=$1) AS sealed",[map_id.to_vec().into()])).await.map_err(db_error)?.ok_or_else(||integrity("staged map seal observation missing"))?.try_get::<bool>("","sealed").map_err(db_error)?;
            if !sealed {
                if let Some(batch)=leaves {
                    txn.execute_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_map_leaf(map_id,page_index,payload) SELECT $1,p.page_index,pg_catalog.decode(p.payload,'hex') FROM pg_catalog.jsonb_to_recordset($2::jsonb) AS p(page_index integer,payload text) ON CONFLICT(map_id,page_index) DO NOTHING",[map_id.to_vec().into(),encode_leaves(batch)?.into()])).await.map_err(db_error)?;
                }
                if let Some(batch)=nodes {
                    txn.execute_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_map_node(map_id,first_page,page_count,digest) SELECT $1,p.first_page,p.page_count,pg_catalog.decode(p.digest,'hex') FROM pg_catalog.jsonb_to_recordset($2::jsonb) AS p(first_page integer,page_count integer,digest text) ON CONFLICT(map_id,first_page,page_count) DO NOTHING",[map_id.to_vec().into(),encode_nodes(batch)?.into()])).await.map_err(db_error)?;
                }
            }
            self.checkpoint_staging(&txn,admission,map_id,generation).await
        }.await;
        finish(txn, result).await
    }

    async fn checkpoint_staging(
        &self,
        txn: &DatabaseTransaction,
        admission: &ChunkMapInstall,
        map_id: [u8; 32],
        generation: i64,
    ) -> Result<(), SnapshotError> {
        barrier(txn, &self.schema).await?;
        let progress=txn.query_one_raw(stmt(&self.schema,"UPDATE mst2_chunk_receipt_generation SET deadline=pg_catalog.clock_timestamp()+interval '59 seconds',last_progress=pg_catalog.clock_timestamp() WHERE receipt_key=$1 AND generation=$2 AND owner=$3::uuid AND state='CREATING' AND deadline>pg_catalog.clock_timestamp() AND map_id=$4 AND map_generation=$5 AND EXISTS(SELECT 1 FROM mst2_chunk_map_lifetime WHERE map_id=$4 AND generation=$5 AND state='LIVE') RETURNING generation",[admission.key.key.clone().into(),admission.generation.into(),admission.owner.to_string().into(),map_id.to_vec().into(),generation.into()])).await.map_err(db_error)?;
        if progress.is_none() {
            return Err(expired(
                "bounded map publication batch lost its exact install generation",
            ));
        }
        Ok(())
    }

    pub(super) async fn compare_staged_map(
        &self,
        verified: &VerifiedSourceChunkMap,
        nodes: &[ChunkMapNode],
        admission: &ChunkMapInstall,
        generation: i64,
    ) -> Result<(), SnapshotError> {
        let map = verified.map();
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            require_current_source(&txn,verified.source(),&self.schema).await?;
            map_barrier(&txn,map.map_id()).await?;
            let row=txn.query_one_raw(stmt(&self.schema,"SELECT CASE WHEN pg_catalog.octet_length(descriptor)=100 THEN descriptor ELSE NULL END AS descriptor,page_count,CASE WHEN pg_catalog.octet_length(pages_root)=32 THEN pages_root ELSE NULL END AS pages_root,(SELECT pg_catalog.count(*) FROM mst2_chunk_map_leaf WHERE map_id=$1) AS leaves,(SELECT pg_catalog.count(*) FROM mst2_chunk_map_node WHERE map_id=$1) AS nodes FROM mst2_chunk_map WHERE map_id=$1",[map.map_id().to_vec().into()])).await.map_err(db_error)?.ok_or_else(||integrity("staged map descriptor missing"))?;
            if bounded_bytes(&row,"descriptor")?!=map.encode() || row.try_get::<i32>("","page_count").map_err(db_error)? as u64!=map.page_count || bounded_bytes(&row,"pages_root")?!=map.pages_root || row.try_get::<i64>("","leaves").map_err(db_error)? as u64!=map.page_count || row.try_get::<i64>("","nodes").map_err(db_error)? as usize!=nodes.len() {return Err(integrity("staged map has conflicting descriptor or incomplete coverage"));}
            self.checkpoint_staging(&txn,admission,map.map_id(),generation).await
        }.await;
        finish(txn, result).await?;
        for batch in verified.leaves().chunks(64) {
            let txn = self.transaction().await?;
            let result=async {
                self.require_scope(&txn).await?;
                require_current_source(&txn,verified.source(),&self.schema).await?;
                map_barrier(&txn,map.map_id()).await?;
                let bad=txn.query_one_raw(stmt(&self.schema,"SELECT p.page_index FROM pg_catalog.jsonb_to_recordset($2::jsonb) AS p(page_index integer,payload text) LEFT JOIN mst2_chunk_map_leaf l ON l.map_id=$1 AND l.page_index=p.page_index WHERE l.payload IS DISTINCT FROM pg_catalog.decode(p.payload,'hex') LIMIT 1",[map.map_id().to_vec().into(),encode_leaves(batch)?.into()])).await.map_err(db_error)?;
                if bad.is_some() {return Err(integrity("immutable stored chunk map leaf conflicts with verified source"));}
                self.checkpoint_staging(&txn,admission,map.map_id(),generation).await
            }.await;
            finish(txn, result).await?;
        }
        for batch in nodes.chunks(1024) {
            let txn = self.transaction().await?;
            let result=async {
                self.require_scope(&txn).await?;
                require_current_source(&txn,verified.source(),&self.schema).await?;
                map_barrier(&txn,map.map_id()).await?;
                let bad=txn.query_one_raw(stmt(&self.schema,"SELECT p.first_page FROM pg_catalog.jsonb_to_recordset($2::jsonb) AS p(first_page integer,page_count integer,digest text) LEFT JOIN mst2_chunk_map_node n ON n.map_id=$1 AND n.first_page=p.first_page AND n.page_count=p.page_count WHERE n.digest IS DISTINCT FROM pg_catalog.decode(p.digest,'hex') LIMIT 1",[map.map_id().to_vec().into(),encode_nodes(batch)?.into()])).await.map_err(db_error)?;
                if bad.is_some() {return Err(integrity("immutable stored chunk map node conflicts with verified source"));}
                self.checkpoint_staging(&txn,admission,map.map_id(),generation).await
            }.await;
            finish(txn, result).await?;
        }
        Ok(())
    }

    pub(crate) async fn maintain(
        &self,
        objects: &MegaObjectStorageWrapper,
        limit: usize,
    ) -> Result<(), SnapshotError> {
        if !(1..=MAINTENANCE_MAX).contains(&limit) {
            return Err(SnapshotError::new(
                SnapshotErrorCode::InvalidRequest,
                "chunk-map maintenance limit must be 1..64",
            ));
        }
        // Unsupported enumeration must fail before changing any resident
        // generation. Overquota stores may still replay known deletions.
        match inventory(self, objects).await {
            Ok(_) => {}
            Err(error) if error.code == SnapshotErrorCode::LimitExceeded => {}
            Err(error) => return Err(error),
        }
        self.replay_cancelled_installs().await?;
        let _memory = retention_budget().reserve(8 * 1024 * 1024)?;
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            barrier(&txn,&self.schema).await?;
            txn.execute_raw(stmt(&self.schema,"DELETE FROM mst2_chunk_reader WHERE owner IN (SELECT owner FROM mst2_chunk_reader WHERE deadline<=pg_catalog.clock_timestamp() ORDER BY deadline,owner LIMIT $1)",[(limit as i64).into()])).await.map_err(db_error)?;
            txn.execute_raw(stmt(&self.schema,"UPDATE mst2_chunk_receipt_generation SET state=CASE WHEN receipt_bytes IS NULL THEN 'APPLIED' ELSE 'DELETING' END,owner=NULL,deadline=NULL,reserved_pages=0,reserved_nodes=0,reserved_bytes=0 WHERE receipt_key IN (SELECT receipt_key FROM mst2_chunk_receipt_generation WHERE state IN ('RESERVED','CREATING') AND deadline<=pg_catalog.clock_timestamp() ORDER BY deadline,receipt_key LIMIT $1)",[(limit as i64).into()])).await.map_err(db_error)?;
            // Replay unfinished receipt claims before admitting new victims.
            let pending=txn.query_one_raw(stmt(&self.schema,"SELECT EXISTS(SELECT 1 FROM mst2_chunk_receipt_generation WHERE state='DELETING') AS pending",[])).await.map_err(db_error)?.ok_or_else(||integrity("chunk receipt replay observation missing"))?.try_get::<bool>("","pending").map_err(db_error)?;
            if !pending {
                txn.execute_raw(stmt(&self.schema,"UPDATE mst2_chunk_receipt_generation SET state='DELETING' WHERE receipt_key IN (SELECT g.receipt_key FROM mst2_chunk_receipt_generation g WHERE g.state='LIVE' AND g.last_progress<=pg_catalog.clock_timestamp()-interval '1 hour' AND NOT EXISTS(SELECT 1 FROM mst2_chunk_reader r WHERE r.receipt_key=g.receipt_key AND r.source_generation=g.generation AND r.deadline>pg_catalog.clock_timestamp()) ORDER BY g.last_progress,g.receipt_key LIMIT $1)",[(limit as i64).into()])).await.map_err(db_error)?;
            }
            // Rotate APPLIED claims too: absence is never a promise that a
            // cancelled remote create cannot restore this retired key later.
            txn.query_all_raw(stmt(&self.schema,"SELECT receipt_key,generation,CASE WHEN pg_catalog.octet_length(receipt_bytes)<=4096 THEN receipt_bytes ELSE NULL END AS receipt_bytes,CASE WHEN pg_catalog.octet_length(primary_scope)<=1024 THEN primary_scope ELSE NULL END AS primary_scope,map_id,map_generation,state FROM mst2_chunk_receipt_generation WHERE state IN ('DELETING','APPLIED') ORDER BY (state='DELETING') DESC,checked_at,receipt_key LIMIT $1",[(limit as i64).into()])).await.map_err(db_error)
        }.await;
        let claims = finish(txn, result).await?;
        for row in claims {
            self.reconcile_receipt(objects, row).await?;
        }
        self.collect_maps(limit).await?;
        let observation = inventory(self, objects).await?;
        let requested = serde_json::to_string(
            &observation
                .inventory
                .objects
                .iter()
                .map(|(key, _)| &key.key)
                .collect::<Vec<_>>(),
        )
        .map_err(db_error)?;
        let txn = self.transaction().await?;
        self.require_scope(&txn).await?;
        let unknown=txn.query_all_raw(stmt(&self.schema,"SELECT p.key FROM pg_catalog.jsonb_array_elements_text($1::jsonb) AS p(key) LEFT JOIN mst2_chunk_receipt_generation g ON g.receipt_key=p.key WHERE g.receipt_key IS NULL ORDER BY p.key LIMIT $2",[requested.into(),(limit as i64).into()])).await.map_err(db_error)?;
        txn.commit().await.map_err(db_error)?;
        for row in unknown {
            let key = ObjectKey {
                namespace: ObjectNamespace::ChunkMapReceipt,
                key: row.try_get("", "key").map_err(db_error)?,
            };
            self.claim_orphan(objects, &key).await?;
        }
        tracing::debug!(target:"mst2::chunk_map",maintenance_limit=limit,actual_backing_receipts=observation.inventory.objects.len(),actual_backing_bytes=observation.inventory.bytes,"bounded chunk-map retention tick");
        Ok(())
    }

    async fn reconcile_receipt(
        &self,
        objects: &MegaObjectStorageWrapper,
        row: QueryResult,
    ) -> Result<(), SnapshotError> {
        if bounded_bytes(&row, "primary_scope")? != self.primary_scope {
            return Err(integrity(
                "retired receipt belongs to a different primary scope",
            ));
        }
        let key = ObjectKey {
            namespace: ObjectNamespace::ChunkMapReceipt,
            key: row.try_get("", "receipt_key").map_err(db_error)?,
        };
        let generation: i64 = row.try_get("", "generation").map_err(db_error)?;
        let expected: Option<Vec<u8>> = row.try_get("", "receipt_bytes").map_err(db_error)?;
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            barrier(&txn,&self.schema).await?;
            txn.query_one_raw(stmt(&self.schema,"SELECT generation FROM mst2_chunk_receipt_generation WHERE receipt_key=$1 AND generation=$2 AND receipt_bytes IS NOT DISTINCT FROM $3::bytea AND primary_scope=$4 AND state IN ('DELETING','APPLIED') AND NOT EXISTS(SELECT 1 FROM mst2_chunk_reader WHERE receipt_key=$1 AND source_generation=$2 AND deadline>pg_catalog.clock_timestamp())",[key.key.clone().into(),generation.into(),expected.clone().into(),self.primary_scope.clone().into()])).await.map_err(db_error)
        }.await;
        if finish(txn, result).await?.is_none() {
            return Ok(());
        }
        if let Some(bytes) = expected.as_ref() {
            let (scope, _, map, physical_generation) = decode_orphan(&key, bytes)?;
            let stored_map: Option<Vec<u8>> = row.try_get("", "map_id").map_err(db_error)?;
            if scope != self.primary_scope
                || physical_generation != generation
                || stored_map.as_deref() != Some(map.map_id().as_slice())
            {
                return Err(integrity(
                    "receipt deletion claim disagrees with the independent physical generation",
                ));
            }
        }
        if let Some(expected) = expected {
            let present = tokio::time::timeout(Duration::from_secs(5), objects.inner.exists(&key))
                .await
                .map_err(|_| unavailable("retired receipt existence check timed out"))?
                .map_err(storage_error)?;
            if present {
                read_receipt(objects, &key, &expected).await?;
                let authority = ChunkMapReceiptDeletion::from_claim(VerifiedReceiptDeletionClaim {
                    key: key.clone(),
                    expected,
                    _generation: generation,
                });
                tokio::time::timeout(
                    Duration::from_secs(5),
                    objects.inner.delete_chunk_map_receipt(&authority),
                )
                .await
                .map_err(|_| unavailable("retired receipt deletion timed out"))?
                .map_err(storage_error)?;
            }
        } else if tokio::time::timeout(Duration::from_secs(5), objects.inner.exists(&key))
            .await
            .map_err(|_| unavailable("unused receipt check timed out"))?
            .map_err(storage_error)?
        {
            return Err(integrity(
                "receipt exists for a generation that never prepared a create",
            ));
        }
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            barrier(&txn,&self.schema).await?;
            let applied=txn.query_one_raw(stmt(&self.schema,"UPDATE mst2_chunk_receipt_generation SET state='APPLIED',owner=NULL,deadline=NULL,reserved_pages=0,reserved_nodes=0,reserved_bytes=0,checked_at=pg_catalog.clock_timestamp() WHERE receipt_key=$1 AND generation=$2 AND state IN ('DELETING','APPLIED') AND NOT EXISTS(SELECT 1 FROM mst2_chunk_reader r WHERE r.receipt_key=$1 AND r.source_generation=$2 AND r.deadline>pg_catalog.clock_timestamp()) RETURNING generation",[key.key.clone().into(),generation.into()])).await.map_err(db_error)?;
            if applied.is_none() { return Err(integrity("retired receipt lost its exact reader-free completion claim")); }
            txn.execute_raw(stmt(&self.schema,"DELETE FROM mst2_chunk_map_source WHERE receipt_key=$1 AND receipt_generation=$2",[key.key.into(),generation.into()])).await.map_err(db_error)?;
            Ok(())
        }.await;
        finish(txn, result).await
    }

    async fn claim_orphan(
        &self,
        objects: &MegaObjectStorageWrapper,
        key: &ObjectKey,
    ) -> Result<(), SnapshotError> {
        let actual = read_bounded_orphan(objects, key).await?;
        let Some(actual) = actual else { return Ok(()) };
        let (scope, source_bytes, map, generation) = decode_orphan(key, &actual)?;
        if scope != self.primary_scope {
            return Err(integrity(
                "orphan receipt belongs to a different captured primary",
            ));
        }
        let identity = source_id(&scope, &source_bytes);
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            barrier(&txn,&self.schema).await?;
            let count=txn.query_one_raw(stmt(&self.schema,"SELECT pg_catalog.count(*) AS history FROM mst2_chunk_receipt_generation",[])).await.map_err(db_error)?.ok_or_else(||integrity("orphan receipt history count missing"))?.try_get::<i64>("","history").map_err(db_error)?;
            if count>=HISTORY_CAP { return Err(SnapshotError::new(SnapshotErrorCode::LimitExceeded,"chunk receipt reconciliation history quota exhausted")); }
            // An orphan never admits a source or map. Its actual body only
            // identifies a retired physical key for collection.
            txn.execute_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_receipt_generation(receipt_key,source_id,generation,source_bytes,primary_scope,map_id,receipt_bytes,state,reserved_pages,reserved_nodes,reserved_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,'DELETING',0,0,0) ON CONFLICT(receipt_key) DO NOTHING",[key.key.clone().into(),identity.to_vec().into(),generation.into(),source_bytes.into(),scope.into(),map.map_id().to_vec().into(),actual.into()])).await.map_err(db_error)?;
            Ok(())
        }.await;
        finish(txn, result).await
    }

    pub(super) async fn collect_maps(&self, limit: usize) -> Result<(), SnapshotError> {
        let txn = self.transaction().await?;
        self.require_scope(&txn).await?;
        let rows=txn.query_all_raw(stmt(&self.schema,"SELECT l.map_id,l.generation,l.state FROM mst2_chunk_map_lifetime l WHERE l.state='DELETING' OR (NOT EXISTS(SELECT 1 FROM mst2_chunk_map_lifetime WHERE state='DELETING') AND (l.last_used<=pg_catalog.clock_timestamp()-interval '1 hour' OR EXISTS(SELECT 1 FROM mst2_chunk_receipt_generation g WHERE g.map_id=l.map_id AND g.map_generation=l.generation AND g.state IN ('DELETING','APPLIED'))) AND NOT EXISTS(SELECT 1 FROM mst2_chunk_map_source s WHERE s.map_id=l.map_id) AND NOT EXISTS(SELECT 1 FROM mst2_chunk_receipt_generation g WHERE g.map_id=l.map_id AND g.map_generation=l.generation AND g.state='CREATING' AND g.deadline>pg_catalog.clock_timestamp())) ORDER BY (l.state='DELETING') DESC,l.last_used,l.map_id LIMIT $1",[(limit as i64).into()])).await.map_err(db_error)?;
        txn.commit().await.map_err(db_error)?;
        for row in rows {
            let map_id: Vec<u8> = row.try_get("", "map_id").map_err(db_error)?;
            let generation: i64 = row.try_get("", "generation").map_err(db_error)?;
            let txn = self.transaction().await?;
            let result=async {
                self.require_scope(&txn).await?;
                let acquired=txn.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,"SELECT pg_catalog.pg_try_advisory_xact_lock(1296717364,pg_catalog.hashtext($1)) AS acquired",[hex::encode(&map_id).into()])).await.map_err(db_error)?.ok_or_else(||integrity("map collection lock observation missing"))?.try_get::<bool>("","acquired").map_err(db_error)?;
                if !acquired { return Ok(()); }
                barrier(&txn,&self.schema).await?;
                let eligible=txn.query_one_raw(stmt(&self.schema,"SELECT state FROM mst2_chunk_map_lifetime l WHERE map_id=$1 AND generation=$2 AND (state='DELETING' OR last_used<=pg_catalog.clock_timestamp()-interval '1 hour' OR EXISTS(SELECT 1 FROM mst2_chunk_receipt_generation g WHERE g.map_id=l.map_id AND g.map_generation=l.generation AND g.state IN ('DELETING','APPLIED'))) AND NOT EXISTS(SELECT 1 FROM mst2_chunk_map_source s WHERE s.map_id=$1) AND NOT EXISTS(SELECT 1 FROM mst2_chunk_reader r WHERE r.map_id=$1 AND r.map_generation=$2 AND r.deadline>pg_catalog.clock_timestamp()) AND NOT EXISTS(SELECT 1 FROM mst2_chunk_receipt_generation g WHERE g.map_id=$1 AND g.map_generation=$2 AND g.state='CREATING' AND g.deadline>pg_catalog.clock_timestamp())",[map_id.clone().into(),generation.into()])).await.map_err(db_error)?;
                let Some(eligible)=eligible else { return Ok(()) };
                if eligible.try_get::<String>("","state").map_err(db_error)?=="LIVE" {
                    let count=txn.query_one_raw(stmt(&self.schema,"SELECT pg_catalog.count(*) AS history FROM mst2_chunk_map_gc",[])).await.map_err(db_error)?.ok_or_else(||integrity("map collection history count missing"))?.try_get::<i64>("","history").map_err(db_error)?;
                    if count>=HISTORY_CAP { return Err(SnapshotError::new(SnapshotErrorCode::LimitExceeded,"chunk map collection history quota exhausted")); }
                    txn.execute_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_map_gc(map_id,generation,state) VALUES($1,$2,'PENDING') ON CONFLICT(map_id,generation) DO NOTHING",[map_id.clone().into(),generation.into()])).await.map_err(db_error)?;
                    txn.execute_raw(stmt(&self.schema,"UPDATE mst2_chunk_map_lifetime SET state='DELETING' WHERE map_id=$1 AND generation=$2 AND state='LIVE'",[map_id.clone().into(),generation.into()])).await.map_err(db_error)?;
                }
                // One bounded resident batch per exact map generation. Large
                // maps stay PENDING and are replayed before new map victims.
                txn.execute_raw(stmt(&self.schema,"DELETE FROM mst2_chunk_map_leaf WHERE map_id=$1 AND page_index IN (SELECT page_index FROM mst2_chunk_map_leaf WHERE map_id=$1 ORDER BY page_index LIMIT 1024)",[map_id.clone().into()])).await.map_err(db_error)?;
                txn.execute_raw(stmt(&self.schema,"DELETE FROM mst2_chunk_map_node WHERE (map_id,first_page,page_count) IN (SELECT map_id,first_page,page_count FROM mst2_chunk_map_node WHERE map_id=$1 ORDER BY first_page,page_count LIMIT 1024)",[map_id.clone().into()])).await.map_err(db_error)?;
                let empty=txn.query_one_raw(stmt(&self.schema,"SELECT NOT EXISTS(SELECT 1 FROM mst2_chunk_map_leaf WHERE map_id=$1) AND NOT EXISTS(SELECT 1 FROM mst2_chunk_map_node WHERE map_id=$1) AS empty",[map_id.clone().into()])).await.map_err(db_error)?.ok_or_else(||integrity("map collection completion observation missing"))?.try_get::<bool>("","empty").map_err(db_error)?;
                if empty {
                    // Deferred lifetime FK permits map deletion while its
                    // exact PENDING generation is still available to guards.
                    txn.execute_raw(stmt(&self.schema,"DELETE FROM mst2_chunk_map WHERE map_id=$1",[map_id.clone().into()])).await.map_err(db_error)?;
                    txn.execute_raw(stmt(&self.schema,"DELETE FROM mst2_chunk_map_lifetime WHERE map_id=$1 AND generation=$2 AND state='DELETING'",[map_id.clone().into(),generation.into()])).await.map_err(db_error)?;
                    txn.execute_raw(stmt(&self.schema,"UPDATE mst2_chunk_map_gc SET state='APPLIED',applied_at=pg_catalog.clock_timestamp() WHERE map_id=$1 AND generation=$2 AND state='PENDING'",[map_id.clone().into(),generation.into()])).await.map_err(db_error)?;
                }
                Ok(())
            }.await;
            finish(txn, result).await?;
        }
        Ok(())
    }

    pub(crate) async fn admit_install(
        &self,
        source: &ChunkMapSource,
        objects: &MegaObjectStorageWrapper,
    ) -> Result<ChunkMapInstall, SnapshotError> {
        // Replay first. Every backend await happens outside the primary
        // completion barrier; active reservations count concurrent creates.
        self.maintain_before_admission(objects).await?;
        let inventory = inventory(self, objects).await?;
        let source_bytes = source.canonical_bytes()?;
        let identity = source_id(&self.primary_scope, &source_bytes);
        let pages = (source.fact().size as u64)
            .div_ceil(mst2_codec::chunkmap::CHUNK_SIZE as u64)
            .div_ceil(CHUNKS_PER_PAGE as u64) as i32;
        let nodes = pages * 2 - 1;
        let reserved_bytes = pages as i64 * 12288 + nodes as i64 * 512 + 1024 * 1024;
        let owner = Uuid::new_v4();
        let txn = self.transaction().await?;
        let result=async {
            self.require_scope(&txn).await?;
            require_current_source(&txn,source,&self.schema).await?;
            barrier(&txn,&self.schema).await?;
            ensure_quotas(&txn,&self.schema,&inventory,pages,nodes,reserved_bytes,None).await?;
            let row=txn.query_one_raw(stmt(&self.schema,"SELECT EXISTS(SELECT 1 FROM mst2_chunk_receipt_generation WHERE source_id=$1) AS used,EXISTS(SELECT 1 FROM mst2_chunk_receipt_generation WHERE source_id=$1 AND state IN ('RESERVED','CREATING','LIVE','DELETING')) AS occupied",[identity.to_vec().into()])).await.map_err(db_error)?.ok_or_else(|| integrity("chunk-map reservation observation missing"))?;
            if row.try_get::<bool>("","occupied").map_err(db_error)? { return Err(unavailable("exact source already has a live or pending chunk-map generation")); }
            let generation=if row.try_get::<bool>("","used").map_err(db_error)? { next_generation(&txn,&self.schema).await? } else { 1 };
            let key=physical_key(identity,generation);
            let owner_started=Instant::now();
            txn.execute_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_receipt_generation(receipt_key,source_id,generation,source_bytes,primary_scope,owner,state,reserved_pages,reserved_nodes,reserved_bytes,deadline) VALUES($1,$2,$3,$4,$5,$6::uuid,'RESERVED',$7,$8,$9,pg_catalog.clock_timestamp()+interval '59 seconds')",[key.key.clone().into(),identity.to_vec().into(),generation.into(),source_bytes.clone().into(),self.primary_scope.clone().into(),owner.to_string().into(),pages.into(),nodes.into(),reserved_bytes.into()])).await.map_err(db_error)?;
            Ok(ChunkMapInstall{repository:self.clone(),source:source.clone(),owner,key,generation,progress:Mutex::new((Instant::now(),Instant::now(),0)),deadline:OwnerDeadline::from_sql_start(owner_started)})
        }.await;
        // An uncertain commit is an error. Its UUID is never reused to mint
        // another reservation; the durable deadline resolves any lost reply.
        finish(txn, result).await
    }

    pub(super) async fn admit_reader(
        &self,
        txn: &DatabaseTransaction,
        source: &ChunkMapSource,
        row: &QueryResult,
        map: &ChunkMap,
    ) -> Result<ChunkMapReader, SnapshotError> {
        barrier(txn, &self.schema).await?;
        let key: String = row.try_get("", "receipt_key").map_err(db_error)?;
        let generation: i64 = row.try_get("", "receipt_generation").map_err(db_error)?;
        let map_generation: i64 = row
            .try_get::<Option<i64>>("", "map_generation")
            .map_err(db_error)?
            .ok_or_else(|| integrity("admitted chunk map lifetime is missing"))?;
        let state: Option<String> = row.try_get("", "generation_state").map_err(db_error)?;
        let map_state: Option<String> = row.try_get("", "map_state").map_err(db_error)?;
        if state.as_deref() == Some("DELETING") {
            return Err(unavailable("admitted chunk map generation is retiring"));
        }
        if state.as_deref() != Some("LIVE") || map_state.as_deref() != Some("LIVE") {
            return Err(integrity(
                "admitted source has a missing or terminal map lifetime",
            ));
        }
        let identity = self.source_identity(source)?;
        if key != physical_key(identity, generation).key
            || bounded_bytes(row, "generation_receipt_bytes")?
                != receipt(&self.primary_scope, source, map)?
        {
            return Err(integrity(
                "chunk map lifetime and independent receipt identity disagree",
            ));
        }
        txn.execute_raw(stmt(&self.schema,"DELETE FROM mst2_chunk_reader WHERE owner IN (SELECT owner FROM mst2_chunk_reader WHERE deadline<=pg_catalog.clock_timestamp() ORDER BY deadline,owner LIMIT 64)",[])).await.map_err(db_error)?;
        let count = txn
            .query_one_raw(stmt(
                &self.schema,
                "SELECT pg_catalog.count(*) AS readers FROM mst2_chunk_reader",
                [],
            ))
            .await
            .map_err(db_error)?
            .ok_or_else(|| integrity("chunk reader count missing"))?
            .try_get::<i64>("", "readers")
            .map_err(db_error)?;
        if count >= 4096 {
            return Err(unavailable("chunk-map reader quota is occupied"));
        }
        let owner = Uuid::new_v4();
        let owner_started = Instant::now();
        txn.execute_raw(stmt(&self.schema,"INSERT INTO mst2_chunk_reader(owner,receipt_key,source_generation,map_id,map_generation,deadline) VALUES($1::uuid,$2,$3,$4,$5,pg_catalog.clock_timestamp()+interval '59 seconds')",[owner.to_string().into(),key.clone().into(),generation.into(),map.map_id().to_vec().into(),map_generation.into()])).await.map_err(db_error)?;
        txn.execute_raw(stmt(&self.schema,"UPDATE mst2_chunk_map_lifetime SET last_used=pg_catalog.clock_timestamp() WHERE map_id=$1 AND generation=$2 AND state='LIVE'",[map.map_id().to_vec().into(),map_generation.into()])).await.map_err(db_error)?;
        txn.execute_raw(stmt(&self.schema,"UPDATE mst2_chunk_receipt_generation SET last_progress=pg_catalog.clock_timestamp() WHERE receipt_key=$1 AND generation=$2 AND state='LIVE'",[key.clone().into(),generation.into()])).await.map_err(db_error)?;
        Ok(ChunkMapReader {
            repository: self.clone(),
            source: source.clone(),
            owner,
            receipt_key: key,
            generation,
            map_id: map.map_id(),
            map_generation,
            confirmed: Mutex::new((Instant::now(), Instant::now())),
            deadline: OwnerDeadline::from_sql_start(owner_started),
        })
    }

    pub(super) async fn require_missing_source_retired(
        &self,
        txn: &DatabaseTransaction,
        source: &ChunkMapSource,
    ) -> Result<bool, SnapshotError> {
        let identity = self.source_identity(source)?;
        let row=txn.query_one_raw(stmt(&self.schema,"SELECT state FROM mst2_chunk_receipt_generation WHERE source_id=$1 AND state IN ('LIVE','DELETING') ORDER BY generation DESC LIMIT 1",[identity.to_vec().into()])).await.map_err(db_error)?;
        if let Some(row) = row {
            if row.try_get::<String>("", "state").map_err(db_error)? == "LIVE" {
                return Err(integrity(
                    "live chunk-map source row is missing; never fall back to the cold body",
                ));
            }
            return Ok(true);
        }
        Ok(false)
    }
}

pub(super) async fn barrier(txn: &DatabaseTransaction, schema: &str) -> Result<(), SnapshotError> {
    txn.query_one_raw(stmt(schema, "SELECT mst2_chunk_retention_barrier()", []))
        .await
        .map_err(db_error)?;
    Ok(())
}

pub(super) async fn map_barrier(
    txn: &DatabaseTransaction,
    map_id: [u8; 32],
) -> Result<(), SnapshotError> {
    // Per-map lock precedes the short global completion barrier. Collection
    // never holds the global barrier while waiting for a map writer.
    txn.query_one_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_catalog.pg_advisory_xact_lock(1296717364,pg_catalog.hashtext($1))",
        [hex::encode(map_id).into()],
    ))
    .await
    .map_err(db_error)?;
    Ok(())
}

pub(super) async fn next_generation(
    txn: &DatabaseTransaction,
    schema: &str,
) -> Result<i64, SnapshotError> {
    txn.query_one_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_catalog.nextval($1::regclass) AS generation",
        [format!("{}.mst2_chunk_generation_sequence", quoted(schema)).into()],
    ))
    .await
    .map_err(db_error)?
    .ok_or_else(|| integrity("chunk generation sequence missing"))?
    .try_get("", "generation")
    .map_err(db_error)
}

fn physical_key(source: [u8; 32], generation: i64) -> ObjectKey {
    ObjectKey {
        namespace: ObjectNamespace::ChunkMapReceipt,
        key: if generation == 1 {
            hex::encode(source)
        } else {
            format!("{}-{generation:016x}", hex::encode(source))
        },
    }
}

pub(super) async fn inventory(
    repository: &PostgresChunkMapRepository,
    objects: &MegaObjectStorageWrapper,
) -> Result<Arc<BackingObservation>, SnapshotError> {
    let mut cached = repository.receipt_observation.lock().await;
    if let Some(observation) = cached.as_ref().filter(|observation| {
        Arc::ptr_eq(&observation.backing.inner, &objects.inner)
            && observation.captured.elapsed() < RENEW_INTERVAL
    }) {
        return Ok(observation.clone());
    }
    let memory = retention_budget().reserve(4 * 1024 * 1024)?;
    let txn = repository.transaction().await?;
    repository.require_scope(&txn).await?;
    let started_at = txn
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT pg_catalog.clock_timestamp()::text AS started_at",
        ))
        .await
        .map_err(db_error)?
        .ok_or_else(|| integrity("backing inventory database clock missing"))?
        .try_get("", "started_at")
        .map_err(db_error)?;
    txn.commit().await.map_err(db_error)?;
    let inventory = tokio::time::timeout(
        Duration::from_secs(30),
        objects.inner.chunk_map_receipt_inventory(),
    )
    .await
    .map_err(|_| unavailable("bounded chunk receipt inventory timed out"))?
    .map_err(|error| match error {
        IoOrbitError::ChunkMapRetentionUnsupported => SnapshotError::new(
            SnapshotErrorCode::TemporaryUnavailable,
            "backing store does not support bounded chunk-map retention",
        ),
        IoOrbitError::ChunkMapRetentionCapacityExceeded => SnapshotError::new(
            SnapshotErrorCode::LimitExceeded,
            "actual backing chunk-map receipts exceed retention quota",
        ),
        error => storage_error(error),
    })?;
    let observation = Arc::new(BackingObservation {
        inventory,
        started_at,
        captured: Instant::now(),
        backing: objects.clone(),
        _memory: memory,
    });
    *cached = Some(observation.clone());
    Ok(observation)
}

pub(super) async fn ensure_quotas(
    txn: &DatabaseTransaction,
    schema: &str,
    observation: &BackingObservation,
    pages: i32,
    nodes: i32,
    bytes: i64,
    exclude: Option<Uuid>,
) -> Result<(), SnapshotError> {
    let inventory = &observation.inventory;
    let row=txn.query_one_raw(stmt(schema,"SELECT (SELECT pg_catalog.count(*) FROM mst2_chunk_map) AS maps,(SELECT pg_catalog.count(*) FROM mst2_chunk_map_source) AS sources,(SELECT pg_catalog.count(*) FROM mst2_chunk_map_leaf) AS leaves,(SELECT pg_catalog.count(*) FROM mst2_chunk_map_node) AS nodes,(SELECT pg_catalog.count(*) FROM mst2_chunk_receipt_generation) AS history,(SELECT pg_catalog.count(*) FROM mst2_chunk_map_gc) AS map_history,(SELECT pg_catalog.count(*) FROM mst2_chunk_receipt_generation WHERE receipt_bytes IS NOT NULL AND NOT create_completed) AS uncertain,(SELECT pg_catalog.count(*) FROM mst2_chunk_receipt_generation WHERE receipt_bytes IS NOT NULL AND (created_at>=$2::timestamptz OR create_completed_at>=$2::timestamptz)) AS recent,pg_catalog.count(*) AS pending,COALESCE(pg_catalog.sum(reserved_pages),0)::bigint AS pending_pages,COALESCE(pg_catalog.sum(reserved_nodes),0)::bigint AS pending_nodes,COALESCE(pg_catalog.sum(reserved_bytes),0)::bigint AS pending_bytes FROM mst2_chunk_receipt_generation WHERE state IN ('RESERVED','CREATING') AND ($1::uuid IS NULL OR owner IS DISTINCT FROM $1::uuid)",[exclude.map(|id|id.to_string()).into(),observation.started_at.clone().into()])).await.map_err(db_error)?.ok_or_else(|| integrity("chunk-map actual quota observation missing"))?;
    let get = |name: &str| row.try_get::<i64>("", name).map_err(db_error);
    let pending = get("pending")?;
    let relation_bytes=txn.query_one_raw(Statement::from_sql_and_values(DbBackend::Postgres,"SELECT COALESCE(pg_catalog.sum(pg_catalog.pg_total_relation_size(c.oid)),0)::bigint AS bytes FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relkind='r' AND c.relname IN ('mst2_chunk_map','mst2_chunk_map_source','mst2_chunk_map_leaf','mst2_chunk_map_node','mst2_chunk_receipt_generation','mst2_chunk_map_lifetime','mst2_chunk_reader','mst2_chunk_map_gc')",[schema.into()])).await.map_err(db_error)?.ok_or_else(||integrity("chunk relation byte observation missing"))?.try_get::<i64>("","bytes").map_err(db_error)?;
    let extra = if exclude.is_none() { 1 } else { 0 };
    if get("maps")? + pending + extra > 4096
        || get("sources")? + pending + extra > 16384
        || get("leaves")? + get("pending_pages")? + pages as i64 > 65536
        || get("nodes")? + get("pending_nodes")? + nodes as i64 > 131072
        || relation_bytes + get("pending_bytes")? + bytes + 1024 * 1024 > 536870912
        || get("history")? + extra > HISTORY_CAP
        || get("map_history")? > HISTORY_CAP
        || pending + extra > 64
        || inventory.objects.len() as i64 + pending + get("uncertain")? + get("recent")? + extra
            > 16384
        || inventory.bytes + ((pending + get("uncertain")? + get("recent")? + extra) as u64) * 4096
            > 64 * 1024 * 1024
    {
        return Err(SnapshotError::new(
            SnapshotErrorCode::LimitExceeded,
            "actual chunk-map rows, history, backing or reserved capacity exceed retention quota",
        ));
    }
    Ok(())
}

fn unavailable(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::TemporaryUnavailable, message)
}
fn expired(message: &str) -> SnapshotError {
    SnapshotError::new(SnapshotErrorCode::LeaseExpired, message)
}

async fn read_bounded_orphan(
    objects: &MegaObjectStorageWrapper,
    key: &ObjectKey,
) -> Result<Option<Vec<u8>>, SnapshotError> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut stream, meta) = match objects.inner.get_stream(key).await {
            Ok(value) => value,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(storage_error(error)),
        };
        if !(129..=4096).contains(&meta.size) {
            return Err(integrity("orphan receipt exceeds its byte profile"));
        }
        let mut bytes = Vec::with_capacity(meta.size as usize);
        while let Some(part) = stream.next().await {
            let part = part.map_err(storage_error)?;
            if part.len() > meta.size as usize - bytes.len() {
                return Err(integrity(
                    "orphan receipt exceeds its advertised exact size",
                ));
            }
            bytes.extend_from_slice(&part);
        }
        if bytes.len() != meta.size as usize {
            return Err(integrity("orphan receipt is truncated"));
        }
        Ok(Some(bytes))
    })
    .await
    .map_err(|_| unavailable("orphan receipt read timed out"))?
}

fn decode_orphan(
    key: &ObjectKey,
    bytes: &[u8],
) -> Result<(Vec<u8>, Vec<u8>, ChunkMap, i64), SnapshotError> {
    if !bytes.starts_with(RECEIPT_DOMAIN) {
        return Err(integrity("orphan is not a canonical trusted receipt"));
    }
    let mut offset = RECEIPT_DOMAIN.len();
    let take = |offset: &mut usize, max: usize| -> Result<Vec<u8>, SnapshotError> {
        let len_bytes = bytes
            .get(*offset..*offset + 4)
            .ok_or_else(|| integrity("orphan receipt length is truncated"))?;
        let len = u32::from_be_bytes(
            len_bytes
                .try_into()
                .map_err(|_| integrity("invalid orphan receipt length"))?,
        ) as usize;
        *offset += 4;
        if len == 0 || len > max {
            return Err(integrity("orphan receipt field exceeds its profile"));
        }
        let field = bytes
            .get(*offset..*offset + len)
            .ok_or_else(|| integrity("orphan receipt field is truncated"))?
            .to_vec();
        *offset += len;
        Ok(field)
    };
    let scope = take(&mut offset, 1024)?;
    let source = take(&mut offset, 2048)?;
    let descriptor = bytes
        .get(offset..)
        .ok_or_else(|| integrity("orphan map descriptor is missing"))?;
    let map = ChunkMap::decode(descriptor)
        .map_err(|_| integrity("orphan map descriptor is not canonical MCM2"))?;
    if map.encode() != descriptor {
        return Err(integrity("orphan receipt has noncanonical map bytes"));
    }
    let generation = if key.key.len() == 64 {
        1
    } else if key.key.len() == 81 && key.key.as_bytes()[64] == b'-' {
        i64::from_str_radix(&key.key[65..], 16)
            .map_err(|_| integrity("orphan physical generation is invalid"))?
    } else {
        return Err(integrity(
            "orphan receipt key has invalid generation profile",
        ));
    };
    if generation <= 0 || physical_key(source_id(&scope, &source), generation) != *key {
        return Err(integrity(
            "orphan physical receipt key disagrees with its exact body",
        ));
    }
    Ok((scope, source, map, generation))
}
