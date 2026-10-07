//! Bounded rooted maintenance. Local retry state is a hint, never GC authority.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GcPhase {
    Claim,
    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GcSeal {
    operation_id: String,
    page_id: [u8; 32],
    generation: i64,
    primary_scope: Vec<u8>,
    metadata_codec: i16,
    expected_size: i32,
    graph_present: bool,
    had_payload: bool,
    certificate_digest: Option<[u8; 32]>,
}

#[derive(Debug, Clone)]
struct RetryOperation {
    seal: GcSeal,
    phase: GcPhase,
}

#[derive(Debug, Default)]
pub(super) struct RootedMaintenanceState {
    retry: Option<RetryOperation>,
    cursor: Option<([u8; 32], i64)>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub(crate) struct RootedMaintenanceWork {
    pub collector_enabled: bool,
    pub examined: u16,
    pub lifetimes_examined: u16,
    pub owners_examined: u16,
    pub pending_replayed: u16,
    pub uncertain_receipts_observed: u16,
    pub absent_claims_observed: u16,
    pub gc_claimed: u16,
    pub gc_applied: u16,
    pub payload_pages_removed: u16,
    pub payload_bytes_removed: u64,
    pub readers_expired: u16,
    pub leases_expired: u16,
    pub prepares_aborted: u16,
    pub handovers_retired: u16,
    pub orphans_retired: u16,
}

struct GcRecord {
    seal: GcSeal,
    applied: bool,
}

fn uncertain(operation: &RetryOperation) -> SnapshotError {
    SnapshotError::new(
        crate::ceres::snapshot::error::SnapshotErrorCode::TemporaryUnavailable,
        format!(
            "qualified GC outcome unknown for operation {} generation {} phase {:?}; retry the sealed operation on the same primary",
            operation.seal.operation_id, operation.seal.generation, operation.phase
        ),
    )
}

fn gc_record(row: &QueryResult) -> Result<GcRecord, SnapshotError> {
    let operation_id: String = row.try_get("", "operation_id").map_err(internal)?;
    let id = uuid::Uuid::parse_str(&operation_id).map_err(internal)?;
    let certificate: Option<Vec<u8>> = row.try_get("", "certificate_digest").map_err(internal)?;
    let seal = GcSeal {
        operation_id,
        page_id: digest_column(row, "page_id")?,
        generation: row.try_get("", "generation").map_err(internal)?,
        primary_scope: row.try_get("", "primary_scope").map_err(internal)?,
        metadata_codec: row.try_get("", "metadata_codec").map_err(internal)?,
        expected_size: row.try_get("", "expected_size").map_err(internal)?,
        graph_present: row.try_get("", "graph_present").map_err(internal)?,
        had_payload: row.try_get("", "had_payload").map_err(internal)?,
        certificate_digest: certificate
            .map(|bytes| bytes.as_slice().try_into().map_err(internal))
            .transpose()?,
    };
    if id.get_version_num() != 4
        || id.to_string() != seal.operation_id
        || seal.generation <= 0
        || seal.metadata_codec != 1
        || seal.expected_size <= 0
        || seal.graph_present != seal.certificate_digest.is_some()
        || row
            .try_get::<String>("", "graph_domain")
            .map_err(internal)?
            != "qualified-v1"
    {
        return Err(integrity(
            "qualified GC immutable operation profile is invalid",
        ));
    }
    let state: String = row.try_get("", "state").map_err(internal)?;
    let applied = match state.as_str() {
        "PENDING" => false,
        "APPLIED" => true,
        _ => return Err(integrity("qualified GC operation has an unknown state")),
    };
    Ok(GcRecord { seal, applied })
}

async fn load_gc<C: ConnectionTrait>(
    db: &C,
    operation: &str,
) -> Result<Option<GcRecord>, SnapshotError> {
    db.query_one_raw(sql(
        "SELECT operation_id::text,page_id,generation,primary_scope,graph_domain,metadata_codec,
         expected_size,graph_present,had_payload,certificate_digest,state
         FROM mst2_metadata_gc_op WHERE operation_id=$1::uuid",
        [operation.into()],
    ))
    .await
    .map_err(internal)?
    .as_ref()
    .map(gc_record)
    .transpose()
}

impl RootedQualifiedMetadataRepository {
    pub(crate) fn start_maintenance(repository: &std::sync::Arc<Self>) {
        let repository = std::sync::Arc::downgrade(repository);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(repository) = repository.upgrade() else {
                    break;
                };
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    repository.maintenance_tick(64),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(code=?error.code, "rooted metadata maintenance failed; durable roots and sealed operations remain recoverable")
                    }
                    Err(_) => tracing::warn!(
                        "rooted metadata maintenance reached its deadline; retrying durable state on the next tick"
                    ),
                }
            }
        });
    }

    async fn collector_enabled(&self) -> Result<bool, SnapshotError> {
        let txn = self.transaction().await?;
        let result = async {
            let row = txn
                .query_one_raw(sql("SELECT mst2_metadata_gc_enabled() AS enabled", []))
                .await
                .map_err(internal)?
                .ok_or_else(|| integrity("qualified collector policy is missing"))?;
            row.try_get("", "enabled").map_err(internal)
        }
        .await;
        let _ = txn.rollback().await;
        result
    }

    fn require_gc_seal(&self, seal: &GcSeal) -> Result<(), SnapshotError> {
        if seal.primary_scope != self.primary_scope {
            return Err(integrity(
                "qualified GC seal belongs to another captured primary",
            ));
        }
        Ok(())
    }

    async fn prove_gc_record(
        &self,
        txn: &DatabaseTransaction,
        record: &GcRecord,
    ) -> Result<(), SnapshotError> {
        self.require_gc_seal(&record.seal)?;
        let seal = &record.seal;
        txn.query_one_raw(sql(
            "SELECT mst2_metadata_gc_proof($1,$2,$3,$4,$5,$6)",
            [
                seal.page_id.to_vec().into(),
                seal.generation.into(),
                if record.applied { "APPLIED" } else { "PENDING" }.into(),
                (!record.applied && seal.graph_present).into(),
                (!record.applied && seal.had_payload).into(),
                seal.certificate_digest.map(|value| value.to_vec()).into(),
            ],
        ))
        .await
        .map_err(internal)?;
        Ok(())
    }

    async fn observe_gc_retry(
        &self,
        operation: &RetryOperation,
    ) -> Result<Option<GcRecord>, SnapshotError> {
        self.require_gc_seal(&operation.seal)?;
        // transaction() acquires the same-primary route completion barrier.
        // A failed barrier or read never proves that an uncertain claim is absent.
        let txn = self.transaction().await.map_err(|_| uncertain(operation))?;
        let result = async {
            let record = load_gc(&txn, &operation.seal.operation_id).await?;
            if let Some(record) = &record {
                if record.seal != operation.seal {
                    return Err(integrity(
                        "qualified GC retry retargeted its immutable sealed identity",
                    ));
                }
                self.prove_gc_record(&txn, record).await?;
            }
            Ok(record)
        }
        .await;
        txn.rollback().await.map_err(|_| uncertain(operation))?;
        result.map_err(|error: SnapshotError| {
            if error.code == crate::ceres::snapshot::error::SnapshotErrorCode::Internal {
                uncertain(operation)
            } else {
                error
            }
        })
    }

    async fn claim_gc_candidate(
        &self,
        candidate: GcSeal,
        state: &mut RootedMaintenanceState,
    ) -> Result<GcSeal, SnapshotError> {
        self.require_gc_seal(&candidate)?;
        let txn = self.transaction().await?;
        let result = async {
            txn.query_one_raw(sql(
                "SELECT mst2_metadata_gc_claim($1,$2,$3,$4::uuid)",
                [
                    candidate.page_id.to_vec().into(),
                    candidate.generation.into(),
                    candidate.primary_scope.clone().into(),
                    candidate.operation_id.clone().into(),
                ],
            ))
            .await
            .map_err(internal)?;
            let record = load_gc(&txn, &candidate.operation_id)
                .await?
                .ok_or_else(|| integrity("qualified claimed GC operation disappeared"))?;
            if record.seal != candidate || record.applied {
                return Err(integrity(
                    "qualified claim differs from its exact captured candidate",
                ));
            }
            self.prove_gc_record(&txn, &record).await?;
            Ok(record.seal)
        }
        .await;
        let seal = match result {
            Ok(seal) => seal,
            Err(error) => {
                let _ = txn.rollback().await;
                return Err(error);
            }
        };
        let retry = RetryOperation {
            seal: seal.clone(),
            phase: GcPhase::Claim,
        };
        // Retain the hint before COMMIT so task cancellation cannot lose an
        // already transmitted commit whose durable outcome is still unknown.
        state.retry = Some(retry.clone());
        txn.commit().await.map_err(|_| uncertain(&retry))?;
        state.retry = None;
        Ok(seal)
    }

    async fn apply_gc_seal(
        &self,
        seal: &GcSeal,
        state: &mut RootedMaintenanceState,
    ) -> Result<bool, SnapshotError> {
        self.require_gc_seal(seal)?;
        let txn = self.transaction().await?;
        let result = async {
            let record = load_gc(&txn, &seal.operation_id)
                .await?
                .ok_or_else(|| integrity("qualified committed GC operation is missing"))?;
            if &record.seal != seal {
                return Err(integrity(
                    "qualified apply retargeted its exact immutable GC operation",
                ));
            }
            self.prove_gc_record(&txn, &record).await?;
            if record.applied {
                return Ok(false);
            }
            txn.query_one_raw(sql(
                "SELECT mst2_metadata_gc_apply($1::uuid)",
                [seal.operation_id.clone().into()],
            ))
            .await
            .map_err(internal)?;
            let applied = load_gc(&txn, &seal.operation_id)
                .await?
                .ok_or_else(|| integrity("qualified GC apply lost its durable receipt"))?;
            if &applied.seal != seal || !applied.applied {
                return Err(integrity(
                    "qualified GC apply did not preserve its sealed APPLIED identity",
                ));
            }
            self.prove_gc_record(&txn, &applied).await?;
            Ok(true)
        }
        .await;
        let changed = match result {
            Ok(changed) => changed,
            Err(error) => {
                let _ = txn.rollback().await;
                return Err(error);
            }
        };
        let retry = RetryOperation {
            seal: seal.clone(),
            phase: GcPhase::Apply,
        };
        state.retry = Some(retry.clone());
        txn.commit().await.map_err(|_| uncertain(&retry))?;
        state.retry = None;
        Ok(changed)
    }

    async fn pending_gc_seals(&self, limit: u16) -> Result<Vec<GcSeal>, SnapshotError> {
        let txn = self.transaction().await?;
        let result = async {
            let rows = txn.query_all_raw(sql(
                "SELECT operation_id::text,page_id,generation,primary_scope,graph_domain,metadata_codec,
                 expected_size,graph_present,had_payload,certificate_digest,state
                 FROM mst2_metadata_gc_op WHERE state='PENDING' ORDER BY created_at,operation_id LIMIT $1",
                [i64::from(limit).into()],
            )).await.map_err(internal)?;
            rows.iter().map(|row| {
                let record=gc_record(row)?;
                self.require_gc_seal(&record.seal)?;
                Ok(record.seal)
            }).collect()
        }.await;
        let _ = txn.rollback().await;
        result
    }

    async fn cleanup_gc_owners(
        &self,
        limit: u16,
        work: &mut RootedMaintenanceWork,
    ) -> Result<(), SnapshotError> {
        let txn = self.transaction().await?;
        let result = async {
            txn.query_one_raw(sql(
                "SELECT * FROM mst2_metadata_gc_owner_cleanup($1::integer)",
                [i32::from(limit).into()],
            ))
            .await
            .map_err(internal)?
            .ok_or_else(|| integrity("qualified owner cleanup lost its bounded counters"))
        }
        .await;
        let row = sessions::finish(txn, result).await?;
        let count = |name: &str| -> Result<u16, SnapshotError> {
            u16::try_from(row.try_get::<i64>("", name).map_err(internal)?).map_err(internal)
        };
        let examined = count("examined")?;
        let readers = count("readers_expired")?;
        let leases = count("leases_expired")?;
        let prepares = count("prepares_aborted")?;
        let handovers = count("handovers_retired")?;
        let orphans = count("orphans_retired")?;
        if examined > limit || readers + leases + prepares + handovers + orphans > examined {
            return Err(integrity(
                "qualified owner cleanup counters exceed their shared budget",
            ));
        }
        work.examined += examined;
        work.owners_examined += examined;
        work.readers_expired += readers;
        work.leases_expired += leases;
        work.prepares_aborted += prepares;
        work.handovers_retired += handovers;
        work.orphans_retired += orphans;
        Ok(())
    }

    async fn gc_candidates(
        &self,
        after: Option<([u8; 32], i64)>,
        limit: u16,
    ) -> Result<Vec<(GcSeal, bool)>, SnapshotError> {
        let (page, generation) = after.map(|(p, g)| (p.to_vec(), g)).unwrap_or_default();
        let txn = self.transaction().await?;
        let result = async {
            let rows = txn.query_all_raw(sql(
                "SELECT l.page_id,l.generation,l.metadata_codec,l.expected_size,
                 n.page_id IS NOT NULL AS graph_present,n.certificate_digest,
                 EXISTS(SELECT 1 FROM mst2_metadata_payload b WHERE b.page_id=l.page_id AND b.generation=l.generation) AS had_payload,
                 NOT EXISTS(SELECT 1 FROM mst2_metadata_root_anchor a WHERE a.root_page=l.page_id AND a.root_generation=l.generation)
                 AND NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_root r WHERE r.page_id=l.page_id AND r.generation=l.generation)
                 AND NOT EXISTS(SELECT 1 FROM mst2_metadata_graph_edge e WHERE e.child_page=l.page_id AND e.child_generation=l.generation)
                 AND NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
                   WHERE m.page_id=l.page_id AND m.generation=l.generation
                     AND (q.state='PREPARING' OR q.state='COMMITTED' AND q.coverage_retired_at IS NULL))
                 AND NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_reuse_root r JOIN mst2_metadata_prepare q USING(prepare_id)
                   WHERE r.root_page=l.page_id AND r.root_generation=l.generation
                     AND (q.state='PREPARING' OR q.state='COMMITTED' AND q.coverage_retired_at IS NULL))
                 AND (l.state='LIVE' OR EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
                   WHERE m.page_id=l.page_id AND m.generation=l.generation AND q.state='ABORTED' AND q.storage_seal IS NOT NULL)
                   AND NOT EXISTS(SELECT 1 FROM mst2_metadata_prepare_page m JOIN mst2_metadata_prepare q USING(prepare_id)
                     WHERE m.page_id=l.page_id AND m.generation=l.generation AND q.state<>'ABORTED')) AS eligible
                 FROM mst2_metadata_lifetime l JOIN mst2_metadata_current c USING(page_id,generation)
                 LEFT JOIN mst2_metadata_graph_node n USING(page_id,generation)
                 WHERE l.state IN ('RESERVED','LIVE') AND (l.page_id,l.generation)>($1,$2)
                 ORDER BY l.page_id,l.generation LIMIT $3",
                [page.into(),generation.into(),i64::from(limit).into()],
            )).await.map_err(internal)?;
            rows.iter().map(|row| {
                let certificate:Option<Vec<u8>>=row.try_get("","certificate_digest").map_err(internal)?;
                Ok((GcSeal {
                    operation_id:uuid::Uuid::new_v4().to_string(),
                    page_id:digest_column(row,"page_id")?,
                    generation:row.try_get("","generation").map_err(internal)?,
                    primary_scope:self.primary_scope.clone(),
                    metadata_codec:row.try_get("","metadata_codec").map_err(internal)?,
                    expected_size:row.try_get("","expected_size").map_err(internal)?,
                    graph_present:row.try_get("","graph_present").map_err(internal)?,
                    had_payload:row.try_get("","had_payload").map_err(internal)?,
                    certificate_digest:certificate.map(|bytes|bytes.as_slice().try_into().map_err(internal)).transpose()?,
                },row.try_get("","eligible").map_err(internal)?))
            }).collect()
        }.await;
        let _ = txn.rollback().await;
        result
    }

    /// A shared owner/page work budget; SQL proof and capacity scans are separately bounded.
    pub(crate) async fn maintenance_tick(
        &self,
        limit: u16,
    ) -> Result<RootedMaintenanceWork, SnapshotError> {
        if !(1..=64).contains(&limit) {
            return Err(integrity("qualified maintenance limit must be 1..=64"));
        }
        let mut state = self.maintenance_state.lock().await;
        let mut work = RootedMaintenanceWork {
            collector_enabled: self.collector_enabled().await?,
            ..Default::default()
        };
        if let Some(retry) = state.retry.clone() {
            work.examined += 1;
            work.lifetimes_examined += 1;
            match self.observe_gc_retry(&retry).await? {
                Some(record) if record.applied => {
                    state.retry = None;
                    work.uncertain_receipts_observed += 1;
                }
                Some(record) if work.collector_enabled => {
                    let changed = self.apply_gc_seal(&record.seal, &mut state).await?;
                    work.pending_replayed += 1;
                    work.record_applied(&record.seal, changed)?;
                }
                Some(_) => {}
                None if retry.phase == GcPhase::Claim => {
                    state.retry = None;
                    work.absent_claims_observed += 1;
                }
                None => {
                    return Err(integrity(
                        "qualified committed GC apply lost its sealed operation history",
                    ));
                }
            }
        }
        if work.collector_enabled && work.examined < limit {
            for seal in self.pending_gc_seals(limit - work.examined).await? {
                work.examined += 1;
                work.lifetimes_examined += 1;
                let changed = self.apply_gc_seal(&seal, &mut state).await?;
                work.pending_replayed += 1;
                work.record_applied(&seal, changed)?;
            }
        }
        if work.examined < limit {
            let remaining = limit - work.examined;
            let owner_budget = if work.collector_enabled {
                remaining.div_ceil(2)
            } else {
                remaining
            };
            self.cleanup_gc_owners(owner_budget, &mut work).await?;
        }
        if work.collector_enabled && work.examined < limit {
            let remaining = limit - work.examined;
            let candidates = self.gc_candidates(state.cursor, remaining).await?;
            let reached_end = candidates.len() < usize::from(remaining);
            for (candidate, eligible) in candidates {
                work.examined += 1;
                work.lifetimes_examined += 1;
                state.cursor = Some((candidate.page_id, candidate.generation));
                if eligible {
                    let seal = self.claim_gc_candidate(candidate, &mut state).await?;
                    work.gc_claimed += 1;
                    let changed = self.apply_gc_seal(&seal, &mut state).await?;
                    work.record_applied(&seal, changed)?;
                }
            }
            if reached_end {
                state.cursor = None;
            }
        }
        Ok(work)
    }
}

impl RootedMaintenanceWork {
    fn record_applied(&mut self, seal: &GcSeal, changed: bool) -> Result<(), SnapshotError> {
        self.gc_applied += 1;
        if changed && seal.had_payload {
            self.payload_pages_removed += 1;
            self.payload_bytes_removed += u64::try_from(seal.expected_size).map_err(internal)?;
        }
        Ok(())
    }
}
